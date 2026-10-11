//! Data sources (ARCHITECTURE.md §8).
//!
//! Works out which parser can read a file, and runs an import: busy lock,
//! `imports` row, parsing, merging into the index, and the final report.

pub mod common_crawl;
pub mod hacker_news;
pub mod lost;

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Result;
use crate::config::Trust;
use crate::engine::Engine;
use crate::record::{Doc, Record};
use crate::store::{BusyLock, ImportRow, Store, now};

/// The kinds of dataset LOST can import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    HackerNews,
    CommonCrawl(common_crawl::Flavor),
    /// LOST's own format.
    Lost,
}

impl Kind {
    /// Name of the data source, stored in the index and the `imports` table.
    /// Files of one import must share it (e.g. WET and WAT files together).
    pub fn name(self) -> &'static str {
        match self {
            Kind::HackerNews => hacker_news::SOURCE,
            Kind::CommonCrawl(_) => common_crawl::SOURCE,
            Kind::Lost => lost::SOURCE,
        }
    }
}

/// What LOST can read, for error messages.
const SUPPORTED: &str =
    ".lost files, Hacker News CSV from BigQuery, Common Crawl WET and WAT files";

/// Opens a file for reading, decompressing it on the fly if it's gzip
/// (`.gz`, including Common Crawl's multi-member files).
pub fn open_input(path: &Path) -> Result<Box<dyn BufRead>> {
    let mut file = File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let mut magic = [0u8; 2];
    let n = file.read(&mut magic)?;
    file.seek(SeekFrom::Start(0))?;
    let file = BufReader::with_capacity(1 << 20, Counted(file));
    Ok(if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::MultiGzDecoder::new(file),
        ))
    } else {
        Box::new(file)
    })
}

/// Works out what kind of dataset a file is, from its first (decompressed) bytes.
pub fn detect(path: &Path) -> Result<Option<Kind>> {
    // .lost files aren't gzipped: look at the bytes as they are.
    let mut magic = Vec::with_capacity(4);
    File::open(path)
        .map_err(|e| format!("can't open {}: {e}", path.display()))?
        .take(4)
        .read_to_end(&mut magic)?;
    if lost::detect(&magic) {
        return Ok(Some(Kind::Lost));
    }

    let mut head = Vec::with_capacity(64 * 1024);
    open_input(path)?.take(64 * 1024).read_to_end(&mut head)?;

    if let Some(flavor) = common_crawl::detect(&head) {
        return Ok(Some(Kind::CommonCrawl(flavor)));
    }
    let text = String::from_utf8_lossy(&head);
    if hacker_news::detect(text.trim_start_matches('\u{feff}')) {
        return Ok(Some(Kind::HackerNews));
    }
    Ok(None)
}

/// A file to import, and whether the user named it directly (rather than it
/// being found inside a folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub explicit: bool,
}

/// Expands folders into the files inside them (recursively, skipping hidden
/// files), keeping the given order otherwise.
pub fn expand(paths: &[PathBuf]) -> Result<Vec<Candidate>> {
    let mut files = Vec::new();
    for path in paths {
        let meta =
            std::fs::metadata(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(path, &mut files)?;
        } else {
            files.push(Candidate {
                path: path.clone(),
                explicit: true,
            });
        }
    }
    Ok(files)
}

fn walk(dir: &Path, out: &mut Vec<Candidate>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("can't open {}: {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(Candidate {
                path,
                explicit: false,
            });
        }
    }
    Ok(())
}

/// The user said no to importing a `.lost` file, and nothing else was
/// left to import. Not an error to show.
#[derive(Debug)]
pub struct Declined;

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not imported")
    }
}

impl std::error::Error for Declined {}

/// What an import reports while it runs.
#[derive(Debug, Clone)]
pub enum Progress {
    /// The files have been sorted into imports (ARCHITECTURE.md §8.2):
    /// these, in the order they'll run.
    Planned(Vec<Planned>),
    /// Import `n` of the plan has started, and is recorded as this row.
    Started(usize, ImportRow),
    /// Records the running import's parser has produced so far.
    Records(u64),
    /// Import `n` of the plan is done: its row, or what went wrong.
    Finished(usize, std::result::Result<ImportRow, String>),
}

/// One import of a run, before it starts.
#[derive(Debug, Clone)]
pub struct Planned {
    /// File names (no folders).
    pub files: Vec<String>,
    /// Data source, as in [`ImportRow::source`].
    pub source: &'static str,
    /// Their size in all, to measure progress by ([`bytes_read`]).
    pub bytes: u64,
}

/// Bytes of dataset files the running import has read: with
/// [`Planned::bytes`], how far along it is. One import runs at a time per
/// process (the busy lock), so one counter does; it starts again at 0 with
/// each import.
static READ: AtomicU64 = AtomicU64::new(0);

/// Bytes of its files the running import has read so far.
pub fn bytes_read() -> u64 {
    READ.load(Ordering::Relaxed)
}

fn count_read(bytes: u64) {
    READ.fetch_add(bytes, Ordering::Relaxed);
}

/// A reader that adds what it reads to [`READ`].
struct Counted<R>(R);

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        count_read(n as u64);
        Ok(n)
    }
}

/// Imports dataset files into the index (ARCHITECTURE.md §8.2), as one
/// import per file where the files stand alone (see [`import_key`]): each
/// `.lost` file, each Common Crawl file (a WET file and its WAT together),
/// and all the Hacker News CSVs, whose stories and comments may sit in
/// different files.
///
/// `.lost` files are checked first (§9.2): `trust` and the trusted signers
/// decide whether each is imported, calling `ask` when the user decides.
/// If the user says no to everything, the error is [`Declined`].
pub fn import(
    data_dir: &Path,
    engine: &Engine,
    paths: &[PathBuf],
    trust: Trust,
    ask: &mut dyn FnMut(&lost::Question) -> lost::Answer,
    progress: &mut dyn FnMut(Progress),
) -> Result<Vec<ImportRow>> {
    // Files named directly must be readable datasets; files found inside a
    // folder that LOST can't read are skipped.
    let mut groups: Vec<Group> = Vec::new();
    let mut skipped = Vec::new();
    let mut declined = 0;
    let mut refused = Vec::new();
    for candidate in expand(paths)? {
        let detected = match detect(&candidate.path) {
            Ok(Some(k)) => k,
            Ok(None) | Err(_) if !candidate.explicit => {
                skipped.push(display_name(&candidate.path));
                continue;
            }
            Ok(None) => {
                return Err(format!(
                    "{}: not a dataset LOST can read (supported so far: {SUPPORTED})",
                    candidate.path.display()
                )
                .into());
            }
            Err(e) => return Err(e),
        };
        if detected == Kind::Lost {
            match lost::approve(&candidate.path, data_dir, trust, ask) {
                Ok(lost::Decision::Import) => {}
                Ok(lost::Decision::Declined) => {
                    declined += 1;
                    continue;
                }
                Ok(lost::Decision::Refused(why)) => {
                    refused.push(why);
                    continue;
                }
                // A damaged file found in a folder doesn't stop the rest.
                Err(e) if !candidate.explicit => {
                    refused.push(e.to_string());
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        let key = import_key(&candidate.path, detected, groups.len());
        match groups.iter_mut().find(|g| g.key == key) {
            Some(group) => group.files.push((candidate.path, detected)),
            None => groups.push(Group {
                key,
                source: detected.name(),
                files: vec![(candidate.path, detected)],
            }),
        }
    }
    if groups.is_empty() {
        if !refused.is_empty() {
            return Err(refused.join("\n").into());
        }
        if declined > 0 {
            return Err(Box::new(Declined));
        }
        return Err(if skipped.is_empty() {
            "nothing to import: the folder is empty".into()
        } else {
            format!(
                "nothing to import: none of the {} files found is a dataset LOST can read",
                skipped.len()
            )
            .into()
        });
    }

    let all_names: Vec<String> = groups
        .iter()
        .flat_map(|g| g.files.iter().map(|(f, _)| display_name(f)))
        .collect();
    let label = match all_names.as_slice() {
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
        [] => unreachable!(),
    };
    // One lock for all groups, so nothing else can start in between.
    let _lock = BusyLock::acquire(data_dir, &format!("import of {label}"))?;
    let store = Store::open(data_dir)?;

    progress(Progress::Planned(
        groups
            .iter()
            .map(|g| Planned {
                files: g.files.iter().map(|(f, _)| display_name(f)).collect(),
                source: g.source,
                bytes: g
                    .files
                    .iter()
                    .map(|(f, _)| std::fs::metadata(f).map_or(0, |m| m.len()))
                    .sum(),
            })
            .collect(),
    ));
    let mut rows = Vec::new();
    let last = groups.len() - 1;
    for (i, Group { source, files, .. }) in groups.into_iter().enumerate() {
        let (skipped_here, refused_here) = if i == last {
            (skipped.as_slice(), refused.as_slice())
        } else {
            (&[][..], &[][..])
        };
        let result = import_group(
            data_dir,
            engine,
            &store,
            (i, source),
            &files,
            skipped_here,
            refused_here,
            progress,
        );
        progress(Progress::Finished(
            i,
            result.as_ref().map(Clone::clone).map_err(|e| e.to_string()),
        ));
        rows.push(result?);
    }
    Ok(rows)
}

/// Files recorded as one import.
struct Group {
    /// What they share: [`import_key`].
    key: String,
    /// Their data source.
    source: &'static str,
    files: Vec<(PathBuf, Kind)>,
}

/// What files of one import share. `.lost` files: nothing (`n`, the number
/// of imports so far, keeps each apart). Hacker News: the data source.
/// Common Crawl: the name they're made from. Each WARC file of a crawl
/// (`…-00000.warc.gz`, the pages as fetched) has a WET file
/// (`…-00000.warc.wet.gz`, their text) and a WAT file
/// (`…-00000.warc.wat.gz`, their metadata and links) made from it, so the
/// three go together (their pages are merged). A file not named like that is
/// an import of its own.
fn import_key(path: &Path, kind: Kind, n: usize) -> String {
    match kind {
        Kind::HackerNews => kind.name().to_string(),
        Kind::Lost => format!("lost #{n}"),
        Kind::CommonCrawl(_) => {
            let name = display_name(path);
            let name = name.strip_suffix(".gz").unwrap_or(&name);
            match name
                .strip_suffix(".warc.wet")
                .or_else(|| name.strip_suffix(".warc.wat"))
                .or_else(|| name.strip_suffix(".warc"))
            {
                Some(stem) if !stem.is_empty() => format!("cc {stem}"),
                _ => format!("cc #{n} {name}"),
            }
        }
    }
}

/// One import: files of a single data source.
#[allow(clippy::too_many_arguments)]
fn import_group(
    data_dir: &Path,
    engine: &Engine,
    store: &Store,
    (n, source): (usize, &str),
    files: &[(PathBuf, Kind)],
    skipped: &[String],
    refused: &[String],
    progress: &mut dyn FnMut(Progress),
) -> Result<ImportRow> {
    let names: Vec<String> = files.iter().map(|(f, _)| display_name(f)).collect();
    let hashes = files
        .iter()
        .map(|(f, _)| hash_file(f))
        .collect::<Result<Vec<_>>>()?;
    let earlier = store
        .imports()?
        .into_iter()
        .rfind(|r| r.status == "complete" && r.hashes == hashes);
    let mut row = store.begin_import(names, hashes, source)?;
    READ.store(0, Ordering::Relaxed);
    progress(Progress::Started(n, row.clone()));

    let result = run(data_dir, engine, files, row.id, progress);
    row.finished = Some(now());
    match result {
        Ok((stats, records, invalid, mut notes)) => {
            row.status = "complete".into();
            row.records = records;
            row.new = stats.new;
            row.updated = stats.updated;
            row.unchanged = stats.unchanged;
            row.repeated = stats.repeated;
            row.invalid = invalid;
            if let Some(e) = earlier {
                notes.insert(
                    0,
                    format!(
                        "The same file{} was already imported as import #{} on {}",
                        if row.files.len() == 1 { "" } else { "s" },
                        e.id,
                        crate::text::format_datetime(e.started)
                    ),
                );
            }
            if !skipped.is_empty() {
                const SHOWN: usize = 5;
                let mut list = skipped
                    .iter()
                    .take(SHOWN)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                if skipped.len() > SHOWN {
                    list.push_str(&format!(" and {} more", skipped.len() - SHOWN));
                }
                notes.push(format!(
                    "Skipped {} file{} LOST can't read: {list}",
                    skipped.len(),
                    if skipped.len() == 1 { "" } else { "s" }
                ));
            }
            for why in refused {
                notes.push(format!("Not imported: {why}"));
            }
            row.notes = notes;
            store.update_import(&row)?;
            Ok(row)
        }
        Err(e) => {
            row.status = "failed".into();
            row.notes.push(e.to_string());
            // Report the original error even if saving the row fails too.
            let _ = store.update_import(&row);
            Err(e)
        }
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// BLAKE3 of a file's contents, hex.
fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("can't open {}: {e}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

type RunOutput = (crate::engine::IndexStats, u64, u64, Vec<String>);

fn run(
    data_dir: &Path,
    engine: &Engine,
    files: &[(PathBuf, Kind)],
    import_id: u64,
    progress: &mut dyn FnMut(Progress),
) -> Result<RunOutput> {
    let mut indexer = engine.indexer()?;
    let mut records = 0u64;
    let mut invalid = 0u64;

    let notes = {
        let mut add = |doc: Option<Doc>| -> Result<()> {
            records += 1;
            match doc {
                Some(doc) => indexer.add(doc)?,
                None => invalid += 1,
            }
            if records.is_multiple_of(1000) {
                progress(Progress::Records(records));
            }
            Ok(())
        };
        if files.iter().all(|(_, k)| *k == Kind::Lost) {
            let mut notes = Vec::new();
            for (path, _) in files {
                notes.extend(lost::import(path, import_id, &mut add)?);
            }
            notes
        } else if files.iter().all(|(_, k)| *k == Kind::HackerNews) {
            let mut sink = |record: Record| add(Doc::from_record(record, import_id));
            // Comments and stories may sit in different files: one pass over all.
            let paths: Vec<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
            hacker_news::import(&paths, data_dir, &mut sink)?
        } else {
            let mut sink = |record: Record| add(Doc::from_record(record, import_id));
            let mut notes = Vec::new();
            for (path, kind) in files {
                if let Kind::CommonCrawl(flavor) = kind {
                    notes.push(common_crawl::import(path, *flavor, &mut sink)?);
                }
            }
            notes
        }
    };
    progress(Progress::Records(records));
    let stats = indexer.finish()?;
    Ok((stats, records, invalid, notes))
}

#[cfg(test)]
mod tests {
    use super::{Kind, import_key};
    use crate::datasources::common_crawl::Flavor;
    use std::path::Path;

    #[test]
    fn one_import_per_file_where_files_stand_alone() {
        let key = |name: &str, kind, n| import_key(Path::new(name), kind, n);
        // Each .lost file on its own.
        assert_ne!(key("a.lost", Kind::Lost, 0), key("b.lost", Kind::Lost, 1));
        // Hacker News CSVs together.
        assert_eq!(
            key("stories.csv", Kind::HackerNews, 0),
            key("comments.csv", Kind::HackerNews, 1)
        );
        // A Common Crawl file's WET and WAT together; other files apart.
        let wet = key(
            "x/CC-MAIN-1-2-00000.warc.wet.gz",
            Kind::CommonCrawl(Flavor::Wet),
            0,
        );
        let wat = key(
            "y/CC-MAIN-1-2-00000.warc.wat.gz",
            Kind::CommonCrawl(Flavor::Wat),
            1,
        );
        let other = key(
            "CC-MAIN-1-2-00001.warc.wet",
            Kind::CommonCrawl(Flavor::Wet),
            2,
        );
        assert_eq!(wet, wat);
        assert_ne!(wet, other);
        // Named some other way: on its own.
        assert_ne!(
            key("crawl.gz", Kind::CommonCrawl(Flavor::Wet), 3),
            key("crawl.gz", Kind::CommonCrawl(Flavor::Wat), 4)
        );
    }
}
