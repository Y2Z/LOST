//! Search engine (ARCHITECTURE.md §6.2, §7).
//!
//! Owns the Tantivy schema; stores and merges documents; turns parsed queries
//! into Tantivy queries; ranks results and builds snippets. Kept
//! self-contained so it can be replaced without touching the rest.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use serde::Serialize;
use tantivy::collector::{Count, DocSetCollector, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{
    BooleanQuery, BoostQuery, EmptyQuery, EnableScoring, Explanation, Occur, PhrasePrefixQuery,
    PhraseQuery, Query, QueryClone, Scorer, TermQuery, Weight,
};
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing,
    TextOptions, Value,
};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{
    DateTime, DocAddress, DocId, DocSet, Index, IndexReader, IndexWriter, ReloadPolicy, Score,
    Searcher, SegmentReader, TantivyDocument, Term,
};

use crate::Result;
use crate::query::{self, Limit, Node, Scope};
use crate::record::{Doc, merge};
use crate::text::{char_ranges, format_date, single_line, truncate_at_word};
use crate::urls::{Url, site_host, site_term};

/// Folder of the Tantivy index inside the data folder.
pub const INDEX_DIR: &str = "index";

/// The layout of the index: its fields and what goes in them (ARCHITECTURE.md
/// §6.2). Raised whenever that changes, with a step in [`upgrade`] that
/// brings an index of the layout before up to date.
///
/// - 1: LOST 0.1–0.2.
/// - 2: URLs tokenized with their punctuation (`inurl:gemini://`), links'
///   file extensions (`contains:`), pages' languages (`lang:`).
pub const INDEX_LAYOUT: u32 = 2;
/// In the index folder: the layout it has, and the LOST that last opened it.
const STAMP_FILE: &str = "lost-index.json";

#[derive(Serialize, serde::Deserialize)]
struct Stamp {
    layout: u32,
    #[serde(default)]
    written_by: String,
}

/// Which pages [`Engine::for_each_doc`] (and an export) goes through.
#[derive(Debug, Clone, Copy)]
pub enum Selection<'a> {
    /// Every page.
    All,
    /// Every page a search finds.
    Search(&'a str),
    /// The pages whose current version came from these imports.
    Imports(&'a [u64]),
}

/// Documents are merged and committed in batches of this many.
const BATCH_SIZE: usize = 5_000;

/// Autocomplete: dictionary words looked at per field and segment, words
/// ranked against the rest of the query, pages read to fill a `_`, and how
/// much of each page's text is read.
const MAX_SUGGEST_SCAN: usize = 5_000;
const MAX_SUGGEST_CANDIDATES: usize = 20;
const MAX_SUGGEST_PAGES: usize = 50;
const MAX_SUGGEST_TEXT: usize = 64 * 1024;

/// Most combinations of `*` gap sizes tried for one phrase.
const MAX_WILDCARD_LAYOUTS: usize = 9;

/// `catf*`: the most words it stands for (the most common ones), and the
/// most dictionary words looked at per field and segment to find them.
const MAX_PREFIX_TERMS: usize = 64;
const MAX_PREFIX_SCAN: usize = 200_000;

/// `related:`: pages of a site read to learn what it's about, how much of
/// each page's text, and how many of its most telling words are looked for.
const RELATED_SEEDS: usize = 20;
const RELATED_TEXT: usize = 16 * 1024;
const RELATED_TERMS: usize = 30;

/// The tokenizer of `url_text`: words and every punctuation mark on its own
/// (ARCHITECTURE.md §6.2), so `inurl:gemini://` means just that.
const URL_TOKENIZER: &str = "url";
/// URL words longer than this (hashes, base64…) aren't indexed.
const MAX_URL_WORD: usize = 64;

/// Snippet length, in characters.
const SNIPPET_CHARS: usize = 240;

/// Field boosts (ARCHITECTURE.md §7.2).
const BOOST_TITLE: f32 = 3.0;
const BOOST_URL: f32 = 2.0;
const BOOST_BODY: f32 = 1.0;

#[derive(Clone, Copy)]
struct Fields {
    key: Field,
    url: Field,
    url_text: Field,
    host_rev: Field,
    ext: Field,
    sources: Field,
    title: Field,
    body: Field,
    links: Field,
    link_hosts: Field,
    link_ext: Field,
    lang: Field,
    crawled: Field,
    import_id: Field,
}

impl Fields {
    fn stored(&self) -> Stored {
        Stored {
            key: self.key,
            url: self.url,
            sources: self.sources,
            title: self.title,
            body: self.body,
            links: self.links,
            crawled: self.crawled,
            import_id: self.import_id,
        }
    }
}

/// The stored fields, which hold everything a page is: what an index of
/// any layout is rebuilt from.
#[derive(Clone, Copy)]
struct Stored {
    key: Field,
    url: Field,
    sources: Field,
    title: Field,
    body: Field,
    links: Field,
    crawled: Field,
    import_id: Field,
}

impl Stored {
    /// The stored fields of an index, found by name (so in an index of an
    /// older layout too).
    fn of(schema: &Schema) -> Result<Stored> {
        let field = |name: &str| {
            schema
                .get_field(name)
                .map_err(|_| format!("the search index has no {name} field"))
        };
        Ok(Stored {
            key: field("key")?,
            url: field("url")?,
            sources: field("sources")?,
            title: field("title")?,
            body: field("body")?,
            links: field("links")?,
            crawled: field("crawled")?,
            import_id: field("import_id")?,
        })
    }

    fn decode(&self, d: &TantivyDocument) -> Doc {
        let text = |field: Field| -> Option<String> {
            d.get_first(field)
                .and_then(|v| v.as_str().map(str::to_string))
        };
        let number = |field: Field| -> Option<u64> { d.get_first(field).and_then(|v| v.as_u64()) };
        let date = |field: Field| -> Option<i64> {
            d.get_first(field)
                .and_then(|v| v.as_datetime())
                .map(|t| t.into_timestamp_secs())
        };
        let all = |field: Field| -> Vec<String> {
            d.get_all(field)
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        Doc {
            key: text(self.key).unwrap_or_default(),
            url: text(self.url).unwrap_or_default(),
            sources: all(self.sources),
            title: text(self.title),
            body: text(self.body).unwrap_or_default(),
            links: all(self.links),
            crawled: date(self.crawled),
            import_id: number(self.import_id).unwrap_or(0),
        }
    }
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let fields = Fields {
        key: b.add_text_field("key", STRING | STORED),
        url: b.add_text_field("url", STORED),
        url_text: b.add_text_field(
            "url_text",
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(URL_TOKENIZER)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            ),
        ),
        host_rev: b.add_text_field("host_rev", STRING),
        ext: b.add_text_field("ext", STRING),
        sources: b.add_text_field("sources", STRING | STORED),
        title: b.add_text_field("title", TEXT | STORED),
        body: b.add_text_field("body", TEXT | STORED),
        links: b.add_text_field("links", STRING | STORED),
        link_hosts: b.add_text_field("link_hosts", STRING),
        link_ext: b.add_text_field("link_ext", STRING),
        lang: b.add_text_field("lang", STRING),
        crawled: b.add_date_field("crawled", STORED),
        import_id: b.add_u64_field("import_id", INDEXED | STORED),
    };
    (b.build(), fields)
}

/// Checks the index's layout stamp (ARCHITECTURE.md §6.6): an index from
/// a newer LOST isn't touched, one from an older LOST is upgraded first. A
/// new index, or one from before stamps (layout 1), gets stamped.
fn check_layout(dir: &Path) -> Result<()> {
    let path = dir.join(STAMP_FILE);
    let layout = match std::fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice::<Stamp>(&bytes)
                .map_err(|_| format!("{} is damaged", path.display()))?
                .layout
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Tantivy's own file: an index made before stamps existed.
            if dir.join("meta.json").exists() {
                1
            } else {
                INDEX_LAYOUT
            }
        }
        Err(e) => return Err(format!("can't read {}: {e}", path.display()).into()),
    };
    if layout > INDEX_LAYOUT {
        return Err(format!(
            "the search index in {} was made by a newer version of LOST; update LOST to use it",
            dir.display()
        )
        .into());
    }
    let mut now = layout;
    while now < INDEX_LAYOUT {
        now = upgrade(dir, now)?;
    }
    write_stamp(dir, now)
}

/// Stamps the index in `dir` with `layout`, and this LOST as the last to
/// open it (unless it says that already).
fn write_stamp(dir: &Path, layout: u32) -> Result<()> {
    let path = dir.join(STAMP_FILE);
    let written_by = format!("LOST {}", env!("CARGO_PKG_VERSION"));
    let current = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Stamp>(&b).ok());
    if current.is_none_or(|s| s.layout != layout || s.written_by != written_by) {
        let stamp = serde_json::to_vec_pretty(&Stamp { layout, written_by })?;
        let partial = dir.join(format!("{STAMP_FILE}.part"));
        std::fs::write(&partial, stamp)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Brings an index of layout `from` up to date, returning the layout it has
/// now. Each layout change adds its step here: done in place where it can
/// be, or by rebuilding the index from its stored fields.
fn upgrade(dir: &Path, from: u32) -> Result<u32> {
    match from {
        // Layout 2 changes only what's worked out from the stored fields.
        1 => {
            rebuild(dir)?;
            Ok(INDEX_LAYOUT)
        }
        _ => Err(format!(
            "the search index in {} has a layout this LOST doesn't know ({from}); \
             remove it and import your datasets again",
            dir.display()
        )
        .into()),
    }
}

/// `index.old`, `index.upgrading`: folders next to the index's.
fn sibling(dir: &Path, what: &str) -> std::path::PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| INDEX_DIR.to_string());
    dir.with_file_name(format!("{name}.{what}"))
}

/// After an upgrade was interrupted while swapping folders: the index is
/// put back (it'll be upgraded again), or what's left of the old one goes.
fn recover(dir: &Path) -> Result<()> {
    let previous = sibling(dir, "old");
    if !previous.exists() {
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(&previous)
            .map_err(|e| format!("can't remove {}: {e}", previous.display()))?;
    } else {
        std::fs::rename(&previous, dir)
            .map_err(|e| format!("can't move {} back: {e}", previous.display()))?;
    }
    Ok(())
}

/// Builds the index in `dir` again, in the current layout, from what it
/// stores — every page is all there — next to it in `index.upgrading`,
/// then puts it in its place. Needs room for a second copy while it runs.
/// The busy lock keeps imports and other upgrades out meanwhile.
fn rebuild(dir: &Path) -> Result<()> {
    let data_dir = dir.parent().unwrap_or(dir);
    let _busy = crate::store::BusyLock::acquire(data_dir, "upgrade of the search index")?;
    let fresh = sibling(dir, "upgrading");
    let previous = sibling(dir, "old");
    let _ = std::fs::remove_dir_all(&fresh);
    std::fs::create_dir_all(&fresh)
        .map_err(|e| format!("can't create {}: {e}", fresh.display()))?;

    {
        let old = Index::open(MmapDirectory::open(dir)?)
            .map_err(|e| format!("can't open the index in {}: {e}", dir.display()))?;
        let stored = Stored::of(&old.schema())?;
        // Read once, with nothing watching the folder: it's moved afterwards.
        let reader: IndexReader = old
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let searcher = reader.searcher();
        let total = searcher.num_docs();

        let (schema, f) = schema();
        let index = Index::create_in_dir(&fresh, schema)
            .map_err(|e| format!("can't create an index in {}: {e}", fresh.display()))?;
        register_tokenizers(&index);
        let threads = writer_threads();
        let mut writer: IndexWriter =
            index.writer_with_num_threads(threads, threads * 50_000_000)?;

        let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
        let report = |done: u64| {
            let line = format!(
                "Upgrading the search index for LOST {}: {} of {} pages…",
                env!("CARGO_PKG_VERSION"),
                crate::text::thousands(done),
                crate::text::thousands(total)
            );
            if interactive {
                eprint!("\r{line}");
            } else if done == 0 || done == total {
                eprintln!("{line}");
            }
        };
        report(0);
        let mut batch: Vec<Doc> = Vec::with_capacity(BATCH_SIZE);
        let mut done = 0;
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            for id in segment.doc_ids_alive() {
                let td: TantivyDocument = searcher.doc(DocAddress::new(ord as u32, id))?;
                batch.push(stored.decode(&td));
                if batch.len() >= BATCH_SIZE {
                    done += write_docs(&writer, &f, &mut batch)?;
                    report(done);
                }
            }
        }
        done += write_docs(&writer, &f, &mut batch)?;
        writer.commit()?;
        writer.wait_merging_threads()?;
        report(done);
        if interactive {
            eprintln!();
        }
        write_stamp(&fresh, INDEX_LAYOUT)?;
    }

    std::fs::rename(dir, &previous)
        .map_err(|e| format!("can't move {} aside: {e}", dir.display()))?;
    std::fs::rename(&fresh, dir).map_err(|e| {
        let _ = std::fs::rename(&previous, dir);
        format!("can't put the upgraded index in {}: {e}", dir.display())
    })?;
    std::fs::remove_dir_all(&previous)
        .map_err(|e| format!("can't remove {}: {e}", previous.display()))?;
    Ok(())
}

/// Indexing threads: as many as there are cores, up to 4.
fn writer_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Adds `docs` (emptying it) to the index, and returns how many.
fn write_docs(writer: &IndexWriter, f: &Fields, docs: &mut Vec<Doc>) -> Result<u64> {
    let languages = detect_languages(docs);
    let count = docs.len() as u64;
    for (doc, lang) in docs.drain(..).zip(languages) {
        writer.add_document(encode(f, &doc, lang))?;
    }
    Ok(count)
}

/// The language of each page (ARCHITECTURE.md §7.1), worked out on every
/// core: it's most of the work of indexing a page.
fn detect_languages(docs: &[Doc]) -> Vec<Option<&'static str>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8);
    if docs.len() < 64 || threads == 1 {
        return docs
            .iter()
            .map(|d| crate::lang::detect(d.title.as_deref(), &d.body))
            .collect();
    }
    let size = docs.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = docs
            .chunks(size)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|d| crate::lang::detect(d.title.as_deref(), &d.body))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Registers the tokenizers the schema names, besides Tantivy's own.
fn register_tokenizers(index: &Index) {
    index.tokenizers().register(URL_TOKENIZER, UrlTokenizer);
}

/// A URL's words (letters and digits, lowercase) and each punctuation mark
/// on its own, with their byte offsets and positions: `gemini://a.b/c` is
/// `gemini` `:` `/` `/` `a` `.` `b` `/` `c`. A word longer than
/// [`MAX_URL_WORD`] bytes is left out, leaving a gap.
fn url_tokens(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((from, c)) = chars.next() {
        let to = if c.is_alphanumeric() {
            let mut to = from + c.len_utf8();
            while let Some(&(at, next)) = chars.peek() {
                if !next.is_alphanumeric() {
                    break;
                }
                to = at + next.len_utf8();
                chars.next();
            }
            to
        } else if c.is_whitespace() {
            continue;
        } else {
            from + c.len_utf8()
        };
        if to - from <= MAX_URL_WORD {
            tokens.push(Token {
                offset_from: from,
                offset_to: to,
                position,
                text: text[from..to].to_lowercase(),
                position_length: 1,
            });
        }
        position += 1;
    }
    tokens
}

#[derive(Clone)]
struct UrlTokenizer;

struct UrlTokenStream {
    tokens: Vec<Token>,
    /// The token `token()` gives, plus one: none before the first `advance`.
    next: usize,
    empty: Token,
}

impl Tokenizer for UrlTokenizer {
    type TokenStream<'a> = UrlTokenStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> UrlTokenStream {
        UrlTokenStream {
            tokens: url_tokens(text),
            next: 0,
            empty: Token::default(),
        }
    }
}

impl TokenStream for UrlTokenStream {
    fn advance(&mut self) -> bool {
        if self.next < self.tokens.len() {
            self.next += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        match self.next.checked_sub(1) {
            Some(at) => &self.tokens[at],
            None => &self.empty,
        }
    }

    fn token_mut(&mut self) -> &mut Token {
        match self.next.checked_sub(1) {
            Some(at) => &mut self.tokens[at],
            None => &mut self.empty,
        }
    }
}

/// One search result.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub url: String,
    pub title: Option<String>,
    pub snippet: String,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawled: Option<String>,
    /// Where the query's words appear in `title`: `[start, end)` character offsets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub title_matches: Vec<[usize; 2]>,
    /// Where the query's words appear in `snippet`: `[start, end)` character offsets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub snippet_matches: Vec<[usize; 2]>,
}

/// A page as stored: what `cache:URL` shows (ARCHITECTURE.md §7.6). Plain
/// text only; the page's HTML was never kept.
#[derive(Debug, Clone, Serialize)]
pub struct CachedPage {
    pub url: String,
    pub title: Option<String>,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crawled: Option<String>,
    pub text: String,
    /// Where the words of the search it was opened from appear in `title`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub title_matches: Vec<[usize; 2]>,
    /// … and in `text`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub text_matches: Vec<[usize; 2]>,
}

/// The answer to a search: what the CLI prints with `-f json` and what
/// `/api/find` returns (ARCHITECTURE.md §10.2).
#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    pub query: String,
    pub offset: usize,
    pub count: usize,
    pub total: usize,
    pub took_ms: u64,
    pub results: Vec<Hit>,
}

/// The search index.
pub struct Engine {
    index: Index,
    reader: IndexReader,
    f: Fields,
}

impl Engine {
    /// Opens the index in `data_dir`, creating it if it doesn't exist.
    pub fn open(data_dir: &Path) -> Result<Engine> {
        let dir = data_dir.join(INDEX_DIR);
        recover(&dir)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        check_layout(&dir)?;
        let (schema, f) = schema();
        let index = Index::open_or_create(MmapDirectory::open(&dir)?, schema)
            .map_err(|e| format!("can't open the index in {}: {e}", dir.display()))?;
        register_tokenizers(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Engine { index, reader, f })
    }

    /// Number of documents in the index.
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Runs a search (ARCHITECTURE.md §7).
    pub fn search(&self, query_text: &str, limit: Limit) -> Result<SearchResults> {
        let started = Instant::now();
        self.reader.reload()?;
        let searcher = self.reader.searcher();

        let mut out = SearchResults {
            query: query_text.to_string(),
            offset: limit.offset,
            count: limit.count,
            total: 0,
            took_ms: 0,
            results: Vec::new(),
        };

        // Only exclusions (`-site:example.com`) ask for nothing: no results,
        // as on Google, rather than everything else.
        let node = query::parse(query_text).filter(query::has_positive);
        let built = match &node {
            Some(node) => self.build(node, &self.text_fields(None))?,
            None => None,
        };
        let Some(q) = built else {
            out.took_ms = started.elapsed().as_millis() as u64;
            return Ok(out);
        };

        let top = if limit.count == 0 {
            out.total = searcher.search(&*q, &Count)?;
            Vec::new()
        } else {
            let collector = (
                TopDocs::with_limit(limit.count)
                    .and_offset(limit.offset)
                    .order_by_score(),
                Count,
            );
            let (top, total) = searcher.search(&*q, &collector)?;
            out.total = total;
            top
        };

        if !top.is_empty() {
            let mut snippets = SnippetGenerator::create(&searcher, &*q, self.f.body)?;
            snippets.set_max_num_chars(SNIPPET_CHARS);

            // The index terms of every word and phrase that isn't excluded.
            let marks = self.marks(node.as_ref())?;
            let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
            let mut word_ranges = |text: &str| -> Vec<(usize, usize)> {
                if marks.is_empty() {
                    return Vec::new();
                }
                tokenize(&mut analyzer, text)
                    .into_iter()
                    .filter(|(token, _, _)| marks.matches(token))
                    .map(|(_, from, to)| (from, to))
                    .collect()
            };

            for (_score, address) in top {
                let td: TantivyDocument = searcher.doc(address)?;
                let doc = self.decode(&td);

                let mut snippet = snippets.snippet_from_doc(&td).fragment().to_string();
                if snippet.trim().is_empty() {
                    snippet = truncate_at_word(&doc.body, SNIPPET_CHARS).to_string();
                }

                let snippet = single_line(&snippet);
                let title_matches = doc
                    .title
                    .as_deref()
                    .map(|t| char_ranges(t, &word_ranges(t)))
                    .unwrap_or_default();
                let snippet_matches = char_ranges(&snippet, &word_ranges(&snippet));

                out.results.push(Hit {
                    url: doc.url,
                    title: doc.title,
                    snippet,
                    title_matches,
                    snippet_matches,
                    sources: doc.sources,
                    crawled: doc.crawled.map(format_date),
                });
            }
        }

        out.took_ms = started.elapsed().as_millis() as u64;
        Ok(out)
    }

    /// Autocomplete suggestions for what's been typed so far (ARCHITECTURE.md §7.4).
    ///
    /// - The last word is completed from the words in the index, most common
    ///   first. With other words before it, completions that appear together
    ///   with them are preferred, and ones that never do are dropped.
    /// - A standalone `_` is a wildcard, as on Google: `how to _ catfish`
    ///   offers the words that fill the gap in matching pages.
    ///
    /// Each suggestion is the whole query, ready to search.
    pub fn suggest(&self, input: &str, limit: usize) -> Result<Vec<String>> {
        let words: Vec<&str> = input.split_whitespace().collect();
        if words.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.reader.reload()?;
        let searcher = self.reader.searcher();

        if words.contains(&"_") {
            return self.fill_gap(&searcher, &words, limit);
        }
        // Nothing to complete once the last word is finished (typed a space),
        // or when it's an operator, an exclusion or a phrase.
        let last = *words.last().unwrap_or(&"");
        if input.ends_with(char::is_whitespace)
            || last.contains(':')
            || last.starts_with('-')
            || last.contains('"')
            || last.ends_with('*')
        {
            return Ok(Vec::new());
        }
        let prefix = match self.tokens(last)?.as_slice() {
            [one] => one.clone(),
            _ => return Ok(Vec::new()),
        };

        // Candidate words by document frequency, from titles and text.
        let mut counts: HashMap<String, u64> = HashMap::new();
        for segment in searcher.segment_readers() {
            for field in [self.f.title, self.f.body] {
                let index = segment.inverted_index(field)?;
                let mut stream = index.terms().range().ge(prefix.as_bytes()).into_stream()?;
                let mut seen = 0;
                while stream.advance() && seen < MAX_SUGGEST_SCAN {
                    let key = stream.key();
                    if !key.starts_with(prefix.as_bytes()) {
                        break;
                    }
                    seen += 1;
                    if let Ok(word) = std::str::from_utf8(key) {
                        *counts.entry(word.to_string()).or_default() +=
                            u64::from(stream.value().doc_freq);
                    }
                }
            }
        }
        let mut candidates: Vec<(String, u64)> = counts.into_iter().collect();
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        candidates.truncate(MAX_SUGGEST_CANDIDATES);

        let context = &words[..words.len() - 1];
        if !context.is_empty() {
            // Rank by how many pages have the candidate together with the
            // words before it.
            let context_text = context.join(" ");
            if self.build_text(&context_text)?.is_some() {
                let mut ranked = Vec::new();
                for (word, _) in candidates {
                    let (Some(context_query), Some(word_query)) =
                        (self.build_text(&context_text)?, self.build_text(&word)?)
                    else {
                        continue;
                    };
                    let both = BooleanQuery::new(vec![
                        (Occur::Must, context_query),
                        (Occur::Must, word_query),
                    ]);
                    let together = searcher.search(&both, &Count)? as u64;
                    if together > 0 {
                        ranked.push((word, together));
                    }
                }
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                candidates = ranked;
            }
        }

        let lead = context.join(" ");
        Ok(candidates
            .into_iter()
            .take(limit)
            .map(|(word, _)| {
                if lead.is_empty() {
                    word
                } else {
                    format!("{lead} {word}")
                }
            })
            .collect())
    }

    /// `how to _ catfish`: the words found in place of `_` in matching pages.
    fn fill_gap(&self, searcher: &Searcher, words: &[&str], limit: usize) -> Result<Vec<String>> {
        let gap = words.iter().position(|w| *w == "_").unwrap_or(0);
        let before = self.tokens(&words[..gap].join(" "))?;
        let after = self.tokens(&words[gap + 1..].join(" ").replace('_', " "))?;
        if before.is_empty() && after.is_empty() {
            return Ok(Vec::new());
        }

        // Pages with the words around the gap, one word apart.
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for field in [self.f.title, self.f.body] {
            let mut terms = Vec::new();
            for (i, token) in before.iter().enumerate() {
                terms.push((i, Term::from_field_text(field, token)));
            }
            for (i, token) in after.iter().enumerate() {
                terms.push((before.len() + 1 + i, Term::from_field_text(field, token)));
            }
            let q: Box<dyn Query> = if terms.len() == 1 {
                Box::new(TermQuery::new(terms.remove(0).1, IndexRecordOption::Basic))
            } else {
                Box::new(PhraseQuery::new_with_offset(terms))
            };
            clauses.push((Occur::Should, q));
        }
        let query = BooleanQuery::new(clauses);
        let top = searcher.search(
            &query,
            &TopDocs::with_limit(MAX_SUGGEST_PAGES).order_by_score(),
        )?;

        // Count what fills the gap in those pages' titles and text.
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        let mut counts: HashMap<String, u64> = HashMap::new();
        for (_, address) in top {
            let doc = self.decode(&searcher.doc::<TantivyDocument>(address)?);
            let text = format!(
                "{}\n{}",
                doc.title.unwrap_or_default(),
                truncate_at_word(&doc.body, MAX_SUGGEST_TEXT)
            );
            let tokens: Vec<String> = tokenize(&mut analyzer, &text)
                .into_iter()
                .map(|(t, _, _)| t)
                .collect();
            let span = before.len() + 1 + after.len();
            for window in tokens.windows(span) {
                if window[..before.len()] == before[..] && window[before.len() + 1..] == after[..] {
                    *counts.entry(window[before.len()].clone()).or_default() += 1;
                }
            }
        }
        let mut found: Vec<(String, u64)> = counts.into_iter().collect();
        found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(found
            .into_iter()
            .take(limit)
            .map(|(word, _)| {
                let mut filled: Vec<&str> = words.to_vec();
                filled[gap] = &word;
                filled.join(" ")
            })
            .collect())
    }

    /// The query for plain text (no operators) across all text fields.
    fn build_text(&self, text: &str) -> Result<Option<Box<dyn Query>>> {
        self.text_query(text, &self.text_fields(None))
    }

    /// The stored page for `url` (the same page for any URL that normalizes
    /// the same way, as imports merge them: §6.5), with the words of
    /// `highlight` (a search) marked. `None` if it isn't in the index.
    pub fn page(&self, url: &str, highlight: Option<&str>) -> Result<Option<CachedPage>> {
        let Some(key) = crate::record::url_key(url) else {
            return Ok(None);
        };
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let Some(doc) = self.load(&searcher, &key)? else {
            return Ok(None);
        };

        let marks = self.marks(highlight.and_then(query::parse).as_ref())?;
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        let mut matches = |text: &str| -> Vec<[usize; 2]> {
            if marks.is_empty() {
                return Vec::new();
            }
            let bytes: Vec<(usize, usize)> = tokenize(&mut analyzer, text)
                .into_iter()
                .filter(|(token, _, _)| marks.matches(token))
                .map(|(_, from, to)| (from, to))
                .collect();
            char_ranges(text, &bytes)
        };
        let title_matches = doc.title.as_deref().map(&mut matches).unwrap_or_default();
        let text_matches = matches(&doc.body);
        Ok(Some(CachedPage {
            url: doc.url,
            title: doc.title,
            sources: doc.sources,
            crawled: doc.crawled.map(format_date),
            text: doc.body,
            title_matches,
            text_matches,
        }))
    }

    /// How many pages' current version came from import `id`.
    pub fn count_import(&self, id: u64) -> Result<u64> {
        self.reader.reload()?;
        let query = TermQuery::new(
            Term::from_field_u64(self.f.import_id, id),
            IndexRecordOption::Basic,
        );
        Ok(self.reader.searcher().search(&query, &Count)? as u64)
    }

    /// Deletes the pages whose current version came from import `id`, and
    /// returns how many. (Earlier versions aren't kept, so pages it updated
    /// go too.) Takes the index writer: the caller holds the busy lock.
    pub fn delete_import(&self, id: u64) -> Result<u64> {
        let count = self.count_import(id)?;
        let mut writer: IndexWriter = self.index.writer_with_num_threads(1, 50_000_000)?;
        writer.delete_term(Term::from_field_u64(self.f.import_id, id));
        writer.commit()?;
        self.reader.reload()?;
        Ok(count)
    }

    /// Hands every page to `each` — or, with a query, every page it finds
    /// (in index order, not ranked) — along with how many there are in all.
    /// Returns that number.
    pub fn for_each_doc(
        &self,
        selection: Selection,
        each: &mut dyn FnMut(Doc, u64) -> Result<()>,
    ) -> Result<u64> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let mut addresses: Vec<DocAddress> = match selection {
            Selection::Imports(ids) => {
                let clauses: Vec<(Occur, Box<dyn Query>)> = ids
                    .iter()
                    .map(|&id| {
                        let term = Term::from_field_u64(self.f.import_id, id);
                        let query: Box<dyn Query> =
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                        (Occur::Should, query)
                    })
                    .collect();
                if clauses.is_empty() {
                    Vec::new()
                } else {
                    searcher
                        .search(&BooleanQuery::new(clauses), &DocSetCollector)?
                        .into_iter()
                        .collect()
                }
            }
            Selection::All => searcher
                .segment_readers()
                .iter()
                .enumerate()
                .flat_map(|(ord, segment)| {
                    segment
                        .doc_ids_alive()
                        .map(move |id| DocAddress::new(ord as u32, id))
                })
                .collect(),
            Selection::Search(text) => {
                let node = query::parse(text).filter(query::has_positive);
                let built = match &node {
                    Some(node) => self.build(node, &self.text_fields(None))?,
                    None => None,
                };
                match built {
                    Some(q) => searcher
                        .search(&*q, &DocSetCollector)?
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                }
            }
        };
        // In storage order, so the document store is read front to back.
        addresses.sort_by_key(|a| (a.segment_ord, a.doc_id));
        let total = addresses.len() as u64;
        for address in addresses {
            let td: TantivyDocument = searcher.doc(address)?;
            each(self.decode(&td), total)?;
        }
        Ok(total)
    }

    /// Starts adding documents. Only one indexer may exist at a time, across
    /// all processes; the busy lock (store.rs) makes sure of that.
    pub fn indexer(&self) -> Result<Indexer<'_>> {
        let threads = writer_threads();
        let writer = self
            .index
            .writer_with_num_threads(threads, threads * 50_000_000)?;
        Ok(Indexer {
            engine: self,
            writer,
            batch: HashMap::new(),
            stats: IndexStats::default(),
        })
    }

    // ── Queries ──────────────────────────────────────────────────────────

    /// The fields a word is searched in, with their boosts: all of them, or
    /// the one an `in…:` operator names.
    fn text_fields(&self, scope: Option<Scope>) -> Vec<(Field, f32)> {
        match scope {
            None => vec![
                (self.f.title, BOOST_TITLE),
                (self.f.url_text, BOOST_URL),
                (self.f.body, BOOST_BODY),
            ],
            Some(Scope::Title) => vec![(self.f.title, 1.0)],
            Some(Scope::Url) => vec![(self.f.url_text, 1.0)],
            Some(Scope::Text) => vec![(self.f.body, 1.0)],
        }
    }

    fn build(&self, node: &Node, fields: &[(Field, f32)]) -> Result<Option<Box<dyn Query>>> {
        let term = |field: Field, text: &str| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(field, text),
                IndexRecordOption::Basic,
            ))
        };
        Ok(match node {
            Node::Word(text) | Node::Phrase(text) => self.text_query(text, fields)?,
            Node::In(scope, inner) => self.build(inner, &self.text_fields(Some(*scope)))?,
            Node::Site(site) => site_term(site).map(|t| term(self.f.host_rev, &t)),
            Node::Ext(ext) => (!ext.is_empty()).then(|| term(self.f.ext, ext)),
            Node::Link(target) => match link_target(target) {
                Some(LinkTarget::Exact(url)) => Some(term(self.f.links, &url)),
                Some(LinkTarget::Host) => site_term(target).map(|t| term(self.f.link_hosts, &t)),
                None => None,
            },
            Node::Near {
                parts,
                distance,
                scope,
            } => self.near_query(parts, *distance, *scope)?,
            Node::Contains(ext) => (!ext.is_empty()).then(|| term(self.f.link_ext, ext)),
            Node::Related(target) => self.related_query(target)?,
            // A language LOST can't tell matches nothing.
            Node::Lang(name) => Some(match crate::lang::code(name) {
                Some(code) => term(self.f.lang, code),
                None => Box::new(EmptyQuery) as Box<dyn Query>,
            }),
            // An exclusion on its own (e.g. in an OR) asks for nothing.
            Node::Not(_) => None,
            Node::And(children) => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for child in children {
                    match child {
                        Node::Not(inner) => {
                            if let Some(q) = self.build(inner, fields)? {
                                clauses.push((Occur::MustNot, Box::new(Excluded(q))));
                            }
                        }
                        other => {
                            if let Some(q) = self.build(other, fields)? {
                                clauses.push((Occur::Must, q));
                            }
                        }
                    }
                }
                // Exclusions alone ask for nothing (ARCHITECTURE.md §7.1).
                if clauses.iter().any(|(occur, _)| *occur != Occur::MustNot) {
                    Some(Box::new(BooleanQuery::new(clauses)))
                } else {
                    None
                }
            }
            Node::Or(children) => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for child in children {
                    if let Some(q) = self.build(child, fields)? {
                        clauses.push((Occur::Should, q));
                    }
                }
                (!clauses.is_empty())
                    .then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
            }
        })
    }

    /// A word or phrase searched in the given fields. Text that tokenizes into
    /// several words ("e-mail", "fried catfish") is matched as a phrase; a `*`
    /// in a phrase stands for one to three missing words, and one at the end
    /// of the last word (`catf*`) for the rest of it.
    fn text_query(&self, text: &str, fields: &[(Field, f32)]) -> Result<Option<Box<dyn Query>>> {
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &(field, boost) in fields {
            if let Some(q) = self.field_text_query(text, field)? {
                clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
            }
        }
        Ok((!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>))
    }

    /// [`Engine::text_query`] in one field, tokenized as that field is
    /// (URLs keep their punctuation). `None` if nothing in `text` is a word
    /// there.
    fn field_text_query(&self, text: &str, field: Field) -> Result<Option<Box<dyn Query>>> {
        let mut analyzer = self.index.tokenizer_for_field(field)?;
        let pieces = query::wildcard_chunks(text);
        let last_piece = pieces.len().saturating_sub(1);
        // Each chunk's terms, and the number of `*` before it.
        let mut chunks: Vec<(usize, Vec<String>)> = Vec::new();
        // Whether the last term is a prefix (`catf*`).
        let mut prefix = false;
        for (i, (stars, chunk)) in pieces.into_iter().enumerate() {
            let words: Vec<&str> = chunk.split_whitespace().collect();
            let mut tokens = Vec::new();
            for (j, word) in words.iter().enumerate() {
                let last = i == last_piece && j + 1 == words.len();
                let (piece, is_prefix) = match query::prefix(word) {
                    Some(stem) => (stem, last),
                    None => (word.trim_end_matches('*'), false),
                };
                let found = tokens_of(&mut analyzer, piece);
                prefix = is_prefix && !found.is_empty();
                tokens.extend(found);
            }
            if tokens.is_empty() {
                continue;
            }
            let stars = if chunks.is_empty() { 0 } else { stars };
            chunks.push((stars, tokens));
        }
        if chunks.is_empty() {
            return Ok(None);
        }
        let gaps: Vec<usize> = chunks.iter().skip(1).map(|(stars, _)| *stars).collect();
        let layouts = query::gap_layouts(&gaps, MAX_WILDCARD_LAYOUTS);

        let mut alternatives: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for layout in &layouts {
            // Term positions: consecutive within a chunk, with a gap before each later chunk.
            let mut terms = Vec::new();
            let mut position = 0;
            for (i, (_, tokens)) in chunks.iter().enumerate() {
                if i > 0 {
                    position += layout[i - 1];
                }
                for token in tokens {
                    terms.push((position, Term::from_field_text(field, token)));
                    position += 1;
                }
            }
            let q: Box<dyn Query> = if prefix && terms.len() == 1 {
                let last = chunks.last().and_then(|(_, t)| t.last()).cloned();
                self.prefix_query(field, &last.unwrap_or_default())?
            } else if prefix {
                let mut q = PhrasePrefixQuery::new_with_offset(terms);
                q.set_max_expansions(MAX_PREFIX_TERMS as u32);
                Box::new(q)
            } else if terms.len() == 1 {
                Box::new(TermQuery::new(
                    terms.remove(0).1,
                    IndexRecordOption::WithFreqs,
                ))
            } else if layout.iter().all(|&g| g == 0) {
                Box::new(PhraseQuery::new(
                    terms.into_iter().map(|(_, t)| t).collect(),
                ))
            } else {
                Box::new(PhraseQuery::new_with_offset(terms))
            };
            alternatives.push((Occur::Should, q));
        }
        Ok(Some(if alternatives.len() == 1 {
            alternatives.remove(0).1
        } else {
            Box::new(BooleanQuery::new(alternatives))
        }))
    }

    /// `catf*` in one field: its [`MAX_PREFIX_TERMS`] most common words
    /// starting with `prefix`. Matches nothing if there are none.
    fn prefix_query(&self, field: Field, prefix: &str) -> Result<Box<dyn Query>> {
        let searcher = self.reader.searcher();
        let mut counts: HashMap<String, u64> = HashMap::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(field)?;
            let mut stream = index.terms().range().ge(prefix.as_bytes()).into_stream()?;
            let mut seen = 0;
            while stream.advance() && seen < MAX_PREFIX_SCAN {
                let key = stream.key();
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                seen += 1;
                if let Ok(word) = std::str::from_utf8(key) {
                    *counts.entry(word.to_string()).or_default() +=
                        u64::from(stream.value().doc_freq);
                }
            }
        }
        let mut words: Vec<(String, u64)> = counts.into_iter().collect();
        words.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        words.truncate(MAX_PREFIX_TERMS);
        if words.is_empty() {
            return Ok(Box::new(EmptyQuery));
        }
        let clauses = words
            .into_iter()
            .map(|(word, _)| {
                let q: Box<dyn Query> = Box::new(TermQuery::new(
                    Term::from_field_text(field, &word),
                    IndexRecordOption::WithFreqs,
                ));
                (Occur::Should, q)
            })
            .collect();
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// `catfish near:3 cornmeal`: the words (or phrases) at most `distance`
    /// words apart, in either order, in the title or the text (or where
    /// `scope` says). Phrases must also be found as they're written.
    fn near_query(
        &self,
        parts: &[String],
        distance: u32,
        scope: Option<Scope>,
    ) -> Result<Option<Box<dyn Query>>> {
        let fields = match scope {
            Some(scope) => self.text_fields(Some(scope)),
            None => vec![(self.f.title, BOOST_TITLE), (self.f.body, BOOST_BODY)],
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (field, boost) in fields {
            let mut analyzer = self.index.tokenizer_for_field(field)?;
            // Each part's terms (stars mean nothing here).
            let tokenized: Vec<(&String, Vec<String>)> = parts
                .iter()
                .map(|part| (part, tokens_of(&mut analyzer, &part.replace('*', " "))))
                .filter(|(_, tokens)| !tokens.is_empty())
                .collect();
            let q: Option<Box<dyn Query>> = match tokenized.as_slice() {
                [] => None,
                // Nothing to be near: just the one part.
                [(part, _)] => self.field_text_query(part, field)?,
                _ => {
                    let phrase = |order: Vec<&Vec<String>>| -> Box<dyn Query> {
                        let terms: Vec<(usize, Term)> = order
                            .into_iter()
                            .flatten()
                            .enumerate()
                            .map(|(i, word)| (i, Term::from_field_text(field, word)))
                            .collect();
                        Box::new(PhraseQuery::new_with_offset_and_slop(terms, distance))
                    };
                    let forward = phrase(tokenized.iter().map(|(_, t)| t).collect());
                    let backward = phrase(tokenized.iter().rev().map(|(_, t)| t).collect());
                    let mut must: Vec<(Occur, Box<dyn Query>)> = vec![(
                        Occur::Must,
                        Box::new(BooleanQuery::new(vec![
                            (Occur::Should, forward),
                            (Occur::Should, backward),
                        ])),
                    )];
                    for (_, tokens) in &tokenized {
                        if tokens.len() > 1 {
                            let terms = tokens
                                .iter()
                                .map(|word| Term::from_field_text(field, word))
                                .collect();
                            must.push((Occur::Must, Box::new(PhraseQuery::new(terms))));
                        }
                    }
                    Some(Box::new(BooleanQuery::new(must)))
                }
            };
            if let Some(q) = q {
                clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
            }
        }
        Ok((!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>))
    }

    /// `related:example.com`: pages that use the words that most set the
    /// site's pages apart, on other sites. `related:URL` (with a path): pages
    /// like that one page. Matches nothing if the site or page isn't stored.
    fn related_query(&self, target: &str) -> Result<Option<Box<dyn Query>>> {
        let searcher = self.reader.searcher();
        let nothing = || Ok(Some(Box::new(EmptyQuery) as Box<dyn Query>));
        let target = target.trim();
        let as_url = if target.contains("://") {
            target.to_string()
        } else {
            format!("https://{target}")
        };
        let page = Url::parse(&as_url).filter(|u| u.path != "/" || u.query.is_some());
        let (seeds, exclude): (Vec<Doc>, Box<dyn Query>) = match page {
            Some(_) => {
                let Some(key) = crate::record::url_key(&as_url) else {
                    return nothing();
                };
                let Some(doc) = self.load(&searcher, &key)? else {
                    return nothing();
                };
                let this = TermQuery::new(
                    Term::from_field_text(self.f.key, &key),
                    IndexRecordOption::Basic,
                );
                (vec![doc], Box::new(this))
            }
            None => {
                let Some(site) = site_term(target) else {
                    return Ok(None);
                };
                let on_site = TermQuery::new(
                    Term::from_field_text(self.f.host_rev, &site),
                    IndexRecordOption::Basic,
                );
                let top = searcher.search(
                    &on_site,
                    &TopDocs::with_limit(RELATED_SEEDS).order_by_score(),
                )?;
                let mut docs = Vec::new();
                for (_, address) in top {
                    docs.push(self.decode(&searcher.doc::<TantivyDocument>(address)?));
                }
                (docs, Box::new(on_site))
            }
        };
        if seeds.is_empty() {
            return nothing();
        }

        // How often each word comes up in the seeds' titles and texts.
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        let mut counts: HashMap<(Field, String), u32> = HashMap::new();
        for doc in &seeds {
            let texts = [
                (self.f.title, doc.title.as_deref().unwrap_or("")),
                (self.f.body, truncate_at_word(&doc.body, RELATED_TEXT)),
            ];
            for (field, text) in texts {
                for word in tokens_of(&mut analyzer, text) {
                    let telling =
                        word.chars().count() >= 3 && !word.chars().all(|c| c.is_numeric());
                    if telling {
                        *counts.entry((field, word)).or_default() += 1;
                    }
                }
            }
        }
        // Weighed by how rare they are elsewhere (as BM25's IDF does). Words
        // only the seeds have can't find anything else; words on more than a
        // tenth of all pages (in a database big enough to say) tell nothing.
        let total = searcher.num_docs() as f32;
        let mut scored: Vec<(Field, String, f32)> = Vec::new();
        for ((field, word), count) in counts {
            let doc_freq = searcher.doc_freq(&Term::from_field_text(field, &word))? as f32;
            if doc_freq <= seeds.len() as f32 || (total >= 1000.0 && doc_freq * 10.0 > total) {
                continue;
            }
            let idf = ((total - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
            scored.push((field, word, count as f32 * idf));
        }
        scored.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
        scored.truncate(RELATED_TERMS);
        let Some(best) = scored.first().map(|(_, _, score)| *score) else {
            return nothing();
        };
        let clauses: Vec<(Occur, Box<dyn Query>)> = scored
            .iter()
            .map(|(field, word, score)| {
                let term = TermQuery::new(
                    Term::from_field_text(*field, word),
                    IndexRecordOption::WithFreqs,
                );
                let q: Box<dyn Query> = Box::new(BoostQuery::new(Box::new(term), score / best));
                (Occur::Should, q)
            })
            .collect();
        // A page has to share a few of those words, not just one.
        let required = (clauses.len() / 5).clamp(1, 6);
        let similar = BooleanQuery::with_minimum_required_clauses(clauses, required);
        Ok(Some(Box::new(BooleanQuery::new(vec![
            (Occur::Must, Box::new(similar)),
            (Occur::MustNot, exclude),
        ]))))
    }

    /// What to mark in titles and snippets: the index terms of every word and
    /// phrase of `node` that isn't excluded, and the beginnings of words
    /// that `catf*` stands for.
    fn marks(&self, node: Option<&Node>) -> Result<Marks> {
        let mut marks = Marks::default();
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        for text in node.map(query::positive_terms).unwrap_or_default() {
            for word in text.split_whitespace() {
                match query::prefix(word) {
                    Some(stem) => {
                        let mut tokens = tokens_of(&mut analyzer, stem);
                        if let Some(last) = tokens.pop() {
                            marks.prefixes.push(last);
                        }
                        marks.words.extend(tokens);
                    }
                    None => marks.words.extend(tokens_of(&mut analyzer, word)),
                }
            }
        }
        Ok(marks)
    }

    /// Splits text into index terms, the same way the text fields were indexed.
    fn tokens(&self, text: &str) -> Result<Vec<String>> {
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        Ok(tokenize(&mut analyzer, text)
            .into_iter()
            .map(|(term, _, _)| term)
            .collect())
    }

    // ── Documents ────────────────────────────────────────────────────────

    fn load(&self, searcher: &Searcher, key: &str) -> Result<Option<Doc>> {
        let q = TermQuery::new(
            Term::from_field_text(self.f.key, key),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&q, &TopDocs::with_limit(1).order_by_score())?;
        match top.first() {
            Some(&(_, address)) => {
                let td: TantivyDocument = searcher.doc(address)?;
                Ok(Some(self.decode(&td)))
            }
            None => Ok(None),
        }
    }

    fn decode(&self, d: &TantivyDocument) -> Doc {
        self.f.stored().decode(d)
    }
}

/// What to mark in a result: [`Engine::marks`].
#[derive(Default)]
struct Marks {
    words: HashSet<String>,
    prefixes: Vec<String>,
}

impl Marks {
    fn is_empty(&self) -> bool {
        self.words.is_empty() && self.prefixes.is_empty()
    }

    fn matches(&self, token: &str) -> bool {
        self.words.contains(token) || self.prefixes.iter().any(|p| token.starts_with(p.as_str()))
    }
}

/// A page as the index holds it; `lang` is its language (§7.1).
fn encode(f: &Fields, doc: &Doc, lang: Option<&str>) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(f.key, &doc.key);
    d.add_text(f.url, &doc.url);
    d.add_text(f.url_text, &doc.url);
    if let Some(url) = Url::parse(&doc.url) {
        for suffix in url.host_suffixes() {
            d.add_text(f.host_rev, &suffix);
        }
        if let Some(ext) = url.extension() {
            d.add_text(f.ext, &ext);
        }
    }
    for source in &doc.sources {
        d.add_text(f.sources, source);
    }
    if let Some(title) = &doc.title {
        d.add_text(f.title, title);
    }
    d.add_text(f.body, &doc.body);
    let mut hosts = HashSet::new();
    let mut extensions = HashSet::new();
    for link in &doc.links {
        d.add_text(f.links, link);
        if let Some(url) = Url::parse(link) {
            hosts.extend(url.host_suffixes());
            extensions.extend(url.extension());
        }
    }
    for host in hosts {
        d.add_text(f.link_hosts, &host);
    }
    for extension in extensions {
        d.add_text(f.link_ext, &extension);
    }
    if let Some(lang) = lang {
        d.add_text(f.lang, lang);
    }
    if let Some(crawled) = doc.crawled {
        d.add_date(f.crawled, DateTime::from_timestamp_secs(crawled));
    }
    d.add_u64(f.import_id, doc.import_id);
    d
}

/// A query whose matches are excluded (`-"exact phrase"`, `-inurl:gemini://`).
///
/// Tantivy checks each candidate against an exclusion with `seek_danger`,
/// which for a phrase asserts the candidate isn't before the phrase's
/// current match — but a phrase scorer moves to its first match as soon as
/// it's made, so the assertion fails in debug builds whenever that match
/// comes after the first candidate (release builds answer correctly). This
/// wrapper leaves `seek_danger` to `DocSet`'s default, which allows it.
struct Excluded(Box<dyn Query>);

impl Clone for Excluded {
    fn clone(&self) -> Self {
        Excluded(self.0.box_clone())
    }
}

impl std::fmt::Debug for Excluded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Excluded").field(&self.0).finish()
    }
}

impl Query for Excluded {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(ExcludedWeight(self.0.weight(enable_scoring)?)))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.0.query_terms(visitor);
    }
}

struct ExcludedWeight(Box<dyn Weight>);

impl Weight for ExcludedWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(Box::new(ExcludedScorer(self.0.scorer(reader, boost)?)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.0.explain(reader, doc)
    }
}

struct ExcludedScorer(Box<dyn Scorer>);

impl DocSet for ExcludedScorer {
    fn advance(&mut self) -> DocId {
        self.0.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.0.seek(target)
    }

    fn doc(&self) -> DocId {
        self.0.doc()
    }

    fn size_hint(&self) -> u32 {
        self.0.size_hint()
    }

    fn cost(&self) -> u64 {
        self.0.cost()
    }
}

impl Scorer for ExcludedScorer {
    fn score(&mut self) -> Score {
        self.0.score()
    }
}

/// What a `link:` operator looks for.
enum LinkTarget {
    /// Pages linking to exactly this URL.
    Exact(String),
    /// Pages linking anywhere on this host (or its subdomains).
    Host,
}

/// `link:https://example.com/page` is a URL; `link:example.com` (or a URL with
/// no path) is a site.
fn link_target(value: &str) -> Option<LinkTarget> {
    match Url::parse(value) {
        Some(mut url) if url.path != "/" || url.query.is_some() => {
            url.fragment = None;
            Some(LinkTarget::Exact(url.to_string()))
        }
        _ => site_host(value).map(|_| LinkTarget::Host),
    }
}

/// Index terms of `text` with their byte offsets.
fn tokenize(analyzer: &mut TextAnalyzer, text: &str) -> Vec<(String, usize, usize)> {
    let mut stream = analyzer.token_stream(text);
    let mut out = Vec::new();
    while stream.advance() {
        let token = stream.token();
        out.push((token.text.clone(), token.offset_from, token.offset_to));
    }
    out
}

/// Index terms of `text`.
fn tokens_of(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    tokenize(analyzer, text)
        .into_iter()
        .map(|(term, _, _)| term)
        .collect()
}

/// Counts from an indexing run.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexStats {
    /// Pages for URLs that weren't in the database before.
    pub new: u64,
    /// Pages that were in the database and got replaced by a fuller or newer version.
    pub updated: u64,
    /// Pages that were already in the database as they are (duplicates).
    pub unchanged: u64,
    /// Records for a URL that already came up earlier in the same import.
    pub repeated: u64,
}

/// Adds documents to the index, merging each with any existing document for
/// the same URL (ARCHITECTURE.md §6.4). Commits every [`BATCH_SIZE`] documents,
/// so results appear while a long import is still running.
pub struct Indexer<'a> {
    engine: &'a Engine,
    writer: IndexWriter,
    batch: HashMap<String, Doc>,
    stats: IndexStats,
}

impl Indexer<'_> {
    pub fn add(&mut self, doc: Doc) -> Result<()> {
        let doc = match self.batch.remove(&doc.key) {
            Some(earlier) => {
                self.stats.repeated += 1;
                merge(earlier, doc)
            }
            None => doc,
        };
        self.batch.insert(doc.key.clone(), doc);
        if self.batch.len() >= BATCH_SIZE {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes and commits the current batch.
    pub fn flush(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let engine = self.engine;
        engine.reader.reload()?;
        let searcher = engine.reader.searcher();

        let mut docs = Vec::with_capacity(self.batch.len());
        for (key, doc) in self.batch.drain() {
            let doc = match engine.load(&searcher, &key)? {
                Some(existing) => {
                    let merged = merge(existing.clone(), doc);
                    if merged == existing {
                        self.stats.unchanged += 1;
                        continue;
                    }
                    self.stats.updated += 1;
                    merged
                }
                None => {
                    self.stats.new += 1;
                    doc
                }
            };
            self.writer
                .delete_term(Term::from_field_text(engine.f.key, &key));
            docs.push(doc);
        }
        write_docs(&self.writer, &engine.f, &mut docs)?;
        self.writer.commit()?;
        engine.reader.reload()?;
        Ok(())
    }

    /// Writes what's left and returns the counts.
    pub fn finish(mut self) -> Result<IndexStats> {
        self.flush()?;
        Ok(self.stats)
    }
}

#[cfg(test)]
mod layout_tests {
    use super::{INDEX_LAYOUT, STAMP_FILE, check_layout};

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lost-layout-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stamps_new_and_old_indexes_and_refuses_newer_ones() {
        // A new index is stamped with the current layout.
        let new = temp("new");
        check_layout(&new).unwrap();
        let stamp = std::fs::read_to_string(new.join(STAMP_FILE)).unwrap();
        assert!(stamp.contains(&format!("\"layout\": {INDEX_LAYOUT}")));

        // One stamped by an older LOST, of the current layout, gets this
        // LOST's name. (One from before stamps is layout 1: upgraded, see
        // `search_tests::upgrading_from_layout_1`.)
        let old = temp("old");
        let stamp = format!("{{\"layout\": {INDEX_LAYOUT}, \"written_by\": \"LOST 0.0.1\"}}");
        std::fs::write(old.join(STAMP_FILE), stamp).unwrap();
        check_layout(&old).unwrap();
        let stamp = std::fs::read_to_string(old.join(STAMP_FILE)).unwrap();
        assert!(stamp.contains(env!("CARGO_PKG_VERSION")), "{stamp}");

        // One from a newer LOST is left alone.
        let newer = temp("newer");
        let stamp = format!(
            "{{\"layout\": {}, \"written_by\": \"LOST 9\"}}",
            INDEX_LAYOUT + 1
        );
        std::fs::write(newer.join(STAMP_FILE), &stamp).unwrap();
        let error = check_layout(&newer).unwrap_err().to_string();
        assert!(error.contains("newer version of LOST"), "{error}");
        assert_eq!(
            std::fs::read_to_string(newer.join(STAMP_FILE)).unwrap(),
            stamp
        );

        for dir in [new, old, newer] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lost-search-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn doc(url: &str, title: &str, body: &str, links: &[&str]) -> Doc {
        Doc {
            key: crate::record::url_key(url).unwrap(),
            url: url.into(),
            sources: vec!["test".into()],
            title: Some(title.into()),
            body: body.into(),
            links: links.iter().map(|l| l.to_string()).collect(),
            crawled: None,
            import_id: 1,
        }
    }

    fn found(engine: &Engine, query: &str) -> Vec<String> {
        let mut urls: Vec<String> = engine
            .search(query, Limit::new(0, 100))
            .unwrap()
            .results
            .into_iter()
            .map(|hit| hit.url)
            .collect();
        urls.sort();
        urls
    }

    const GERMAN: &str = "Das ist ein ganz normaler deutscher Satz über das Wetter in Berlin, \
                          und noch einer über die Stadt und ihre vielen schönen Häuser.";

    fn pages() -> Vec<Doc> {
        vec![
            doc(
                "gemini://capsule.example/notes/catfish.gmi",
                "Fried catfish",
                "Roll the catfish in cornmeal and fry it.",
                &["gemini://capsule.example/recipes.pdf"],
            ),
            doc(
                "https://web.example/gemini/protocol",
                "About the Gemini protocol",
                "Cornmeal is not mentioned here, but catfish are, far away from that \
                 other word: one two three four five six seven eight nine ten cornmeal.",
                &["https://web.example/spec.html"],
            ),
            doc(
                "http://hidden.onion/index.html",
                "Hidden service",
                GERMAN,
                &[],
            ),
        ]
    }

    fn engine_with(name: &str, docs: Vec<Doc>) -> (Engine, std::path::PathBuf) {
        let dir = temp(name);
        let engine = Engine::open(&dir).unwrap();
        let mut indexer = engine.indexer().unwrap();
        for d in docs {
            indexer.add(d).unwrap();
        }
        indexer.finish().unwrap();
        (engine, dir)
    }

    #[test]
    fn url_words_and_punctuation() {
        let tokens: Vec<(String, usize)> = url_tokens("Gemini://A.b/c-d?é=1")
            .into_iter()
            .map(|t| (t.text, t.position))
            .collect();
        let expected: Vec<(String, usize)> = [
            "gemini", ":", "/", "/", "a", ".", "b", "/", "c", "-", "d", "?", "é", "=", "1",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| (t.to_string(), i))
        .collect();
        assert_eq!(tokens, expected);
        // A long word is left out, but keeps its place.
        let long = format!("a/{}/b", "x".repeat(MAX_URL_WORD + 1));
        let tokens: Vec<(String, usize)> = url_tokens(&long)
            .into_iter()
            .map(|t| (t.text, t.position))
            .collect();
        assert_eq!(
            tokens,
            [
                ("a".into(), 0),
                ("/".into(), 1),
                ("/".into(), 3),
                ("b".into(), 4)
            ]
        );
    }

    #[test]
    fn operators() {
        let (engine, dir) = engine_with("operators", pages());
        let gemini = "gemini://capsule.example/notes/catfish.gmi";
        let web = "https://web.example/gemini/protocol";
        let onion = "http://hidden.onion/index.html";

        // inurl: with punctuation.
        assert_eq!(found(&engine, "inurl:gemini"), [gemini, web]);
        assert_eq!(found(&engine, "inurl:gemini://"), [gemini]);
        assert_eq!(found(&engine, "inurl:.onion/"), [onion]);
        assert_eq!(found(&engine, "allinurl:https:// gemini"), [web]);
        assert_eq!(found(&engine, "-inurl:gemini:// catfish"), [web]);

        // near:
        assert_eq!(found(&engine, "catfish near:3 cornmeal"), [gemini]);
        assert_eq!(found(&engine, "cornmeal near:3 catfish"), [gemini]);
        assert_eq!(found(&engine, "catfish near:30 cornmeal").len(), 2);
        assert_eq!(found(&engine, "catfish cornmeal").len(), 2);

        // Prefixes.
        assert_eq!(found(&engine, "catf*").len(), 2);
        assert_eq!(found(&engine, "intitle:catf*"), [gemini]);
        assert_eq!(found(&engine, "\"fried catf*\""), [gemini]);
        assert_eq!(found(&engine, "catfz*"), Vec::<String>::new());
        assert_eq!(found(&engine, "inurl:capsu*"), [gemini]);
        let hits = engine.search("corn*", Limit::new(0, 10)).unwrap();
        assert!(hits.results.iter().all(|h| !h.snippet_matches.is_empty()));

        // contains:
        assert_eq!(found(&engine, "contains:pdf"), [gemini]);
        assert_eq!(found(&engine, "contains:.HTML"), [web]);

        // lang:
        assert_eq!(found(&engine, "lang:de"), [onion]);
        assert_eq!(found(&engine, "language:German"), [onion]);
        assert_eq!(found(&engine, "lang:deutsch Berlin"), [onion]);
        assert!(found(&engine, "lang:klingon").is_empty());
        assert!(found(&engine, "lang:de -lang:deu").is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn related_pages() {
        let words = "catfish cornmeal buttermilk skillet hushpuppies remoulade";
        let mut docs = vec![
            doc("https://fish.example/", "Fish fry", words, &[]),
            doc("https://fish.example/more", "More fish fry", words, &[]),
            doc("https://other.example/fry", "Southern fry", words, &[]),
            doc(
                "https://unrelated.example/",
                "Gardening",
                "tomatoes compost trellis mulch seedlings",
                &[],
            ),
        ];
        // Enough other pages for the words to be rare.
        for i in 0..20 {
            docs.push(doc(
                &format!("https://filler.example/{i}"),
                "Filler",
                "nothing to see here at all",
                &[],
            ));
        }
        let (engine, dir) = engine_with("related", docs);
        assert_eq!(
            found(&engine, "related:fish.example"),
            ["https://other.example/fry"]
        );
        assert_eq!(
            found(&engine, "related:https://other.example/fry"),
            ["https://fish.example/", "https://fish.example/more"]
        );
        assert!(found(&engine, "related:nowhere.example").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn upgrading_from_layout_1() {
        // An index as LOST 0.2 made it: URLs tokenized like text, no link
        // extensions or languages, no stamp.
        let data = temp("upgrade");
        let dir = data.join(INDEX_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let mut b = Schema::builder();
            let key = b.add_text_field("key", STRING | STORED);
            let url = b.add_text_field("url", STORED);
            let url_text = b.add_text_field("url_text", TEXT);
            b.add_text_field("host_rev", STRING);
            b.add_text_field("ext", STRING);
            let sources = b.add_text_field("sources", STRING | STORED);
            let title = b.add_text_field("title", TEXT | STORED);
            let body = b.add_text_field("body", TEXT | STORED);
            let links = b.add_text_field("links", STRING | STORED);
            b.add_text_field("link_hosts", STRING);
            b.add_date_field("crawled", STORED);
            let import_id = b.add_u64_field("import_id", INDEXED | STORED);
            let index = Index::create_in_dir(&dir, b.build()).unwrap();
            let mut writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
            for page in pages() {
                let mut d = TantivyDocument::default();
                d.add_text(key, &page.key);
                d.add_text(url, &page.url);
                d.add_text(url_text, &page.url);
                d.add_text(sources, "test");
                d.add_text(title, page.title.as_deref().unwrap());
                d.add_text(body, &page.body);
                for link in &page.links {
                    d.add_text(links, link);
                }
                d.add_u64(import_id, 7);
                writer.add_document(d).unwrap();
            }
            writer.commit().unwrap();
        }
        let engine = Engine::open(&data).unwrap();
        assert_eq!(engine.num_docs(), 3);
        assert_eq!(
            found(&engine, "inurl:gemini://"),
            ["gemini://capsule.example/notes/catfish.gmi"]
        );
        assert_eq!(
            found(&engine, "lang:de"),
            ["http://hidden.onion/index.html"]
        );
        assert_eq!(engine.count_import(7).unwrap(), 3);
        let stamp = std::fs::read_to_string(dir.join(STAMP_FILE)).unwrap();
        assert!(stamp.contains(&format!("\"layout\": {INDEX_LAYOUT}")));
        assert!(!sibling(&dir, "old").exists());
        assert!(!sibling(&dir, "upgrading").exists());
        drop(engine);
        // Opened again: nothing more to do.
        let engine = Engine::open(&data).unwrap();
        assert_eq!(engine.num_docs(), 3);
        drop(engine);
        let _ = std::fs::remove_dir_all(data);
    }
}
