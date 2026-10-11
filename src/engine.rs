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
use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{TextAnalyzer, TokenStream};
use tantivy::{
    DateTime, DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument,
    Term,
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
pub const INDEX_LAYOUT: u32 = 1;
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
    crawled: Field,
    import_id: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let fields = Fields {
        key: b.add_text_field("key", STRING | STORED),
        url: b.add_text_field("url", STORED),
        url_text: b.add_text_field("url_text", TEXT),
        host_rev: b.add_text_field("host_rev", STRING),
        ext: b.add_text_field("ext", STRING),
        sources: b.add_text_field("sources", STRING | STORED),
        title: b.add_text_field("title", TEXT | STORED),
        body: b.add_text_field("body", TEXT | STORED),
        links: b.add_text_field("links", STRING | STORED),
        link_hosts: b.add_text_field("link_hosts", STRING),
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
    let written_by = format!("LOST {}", env!("CARGO_PKG_VERSION"));
    let current = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Stamp>(&b).ok());
    if current.is_none_or(|s| s.layout != now || s.written_by != written_by) {
        let stamp = serde_json::to_vec_pretty(&Stamp {
            layout: now,
            written_by,
        })?;
        let partial = dir.join(format!("{STAMP_FILE}.part"));
        std::fs::write(&partial, stamp)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Brings an index of layout `from` to the next layout, returning it. Each
/// layout change adds its step here: done in place where it can be, or by
/// rebuilding the index — from the capture archive once there is one
/// (§6.7), otherwise by importing again. There are none yet.
fn upgrade(dir: &Path, from: u32) -> Result<u32> {
    Err(format!(
        "the search index in {} was made by an older version of LOST (layout {from}) \
         and can't be upgraded; remove it and import your datasets again",
        dir.display()
    )
    .into())
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
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        check_layout(&dir)?;
        let (schema, f) = schema();
        let index = Index::open_or_create(MmapDirectory::open(&dir)?, schema)
            .map_err(|e| format!("can't open the index in {}: {e}", dir.display()))?;
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
            let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
            let terms: HashSet<String> = node
                .as_ref()
                .map(query::positive_terms)
                .unwrap_or_default()
                .iter()
                .flat_map(|t| {
                    tokenize(&mut analyzer, t)
                        .into_iter()
                        .map(|(text, _, _)| text)
                })
                .collect();
            let mut word_ranges = |text: &str| -> Vec<(usize, usize)> {
                if terms.is_empty() {
                    return Vec::new();
                }
                tokenize(&mut analyzer, text)
                    .into_iter()
                    .filter(|(token, _, _)| terms.contains(token))
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

        let terms: HashSet<String> = highlight
            .and_then(query::parse)
            .map(|node| query::positive_terms(&node))
            .unwrap_or_default()
            .iter()
            .map(|t| self.tokens(t))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut analyzer = self.index.tokenizer_for_field(self.f.body)?;
        let mut matches = |text: &str| -> Vec<[usize; 2]> {
            if terms.is_empty() {
                return Vec::new();
            }
            let bytes: Vec<(usize, usize)> = tokenize(&mut analyzer, text)
                .into_iter()
                .filter(|(token, _, _)| terms.contains(token))
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
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 4);
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
            // An exclusion on its own (e.g. in an OR) asks for nothing.
            Node::Not(_) => None,
            Node::And(children) => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for child in children {
                    match child {
                        Node::Not(inner) => {
                            if let Some(q) = self.build(inner, fields)? {
                                clauses.push((Occur::MustNot, q));
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
    /// in a phrase stands for one to three missing words.
    fn text_query(&self, text: &str, fields: &[(Field, f32)]) -> Result<Option<Box<dyn Query>>> {
        // Each chunk's terms, and the number of `*` before it.
        let mut chunks: Vec<(usize, Vec<String>)> = Vec::new();
        for (stars, chunk) in query::wildcard_chunks(text) {
            let tokens = self.tokens(&chunk)?;
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

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &(field, boost) in fields {
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
                let q: Box<dyn Query> = if terms.len() == 1 {
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
                clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
            }
        }
        Ok(Some(Box::new(BooleanQuery::new(clauses))))
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

    fn encode(&self, doc: &Doc) -> TantivyDocument {
        let f = &self.f;
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
        for link in &doc.links {
            d.add_text(f.links, link);
            if let Some(url) = Url::parse(link) {
                hosts.extend(url.host_suffixes());
            }
        }
        for host in hosts {
            d.add_text(f.link_hosts, &host);
        }
        if let Some(crawled) = doc.crawled {
            d.add_date(f.crawled, DateTime::from_timestamp_secs(crawled));
        }
        d.add_u64(f.import_id, doc.import_id);
        d
    }

    fn decode(&self, d: &TantivyDocument) -> Doc {
        let f = &self.f;
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
            key: text(f.key).unwrap_or_default(),
            url: text(f.url).unwrap_or_default(),
            sources: all(f.sources),
            title: text(f.title),
            body: text(f.body).unwrap_or_default(),
            links: all(f.links),
            crawled: date(f.crawled),
            import_id: number(f.import_id).unwrap_or(0),
        }
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
            self.writer.add_document(engine.encode(&doc))?;
        }
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

        // One from before stamps is layout 1, which is current.
        let old = temp("old");
        std::fs::write(old.join("meta.json"), "{}").unwrap();
        check_layout(&old).unwrap();
        assert!(old.join(STAMP_FILE).exists());

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
