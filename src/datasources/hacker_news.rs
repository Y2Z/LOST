//! Hacker News, from a CSV export of BigQuery's
//! `bigquery-public-data.hacker_news.full` (ARCHITECTURE.md §8.4).
//!
//! Comments only record their direct parent, so the import works in three
//! passes over a temporary redb file (the full dataset has tens of millions of
//! comments, too many to hold in memory):
//!
//! 1. Read every row: keep live stories, every comment's parent, and the text
//!    of live comments.
//! 2. Walk each live comment up its parent chain to its story.
//! 3. Emit each story as a page: its HN item URL, with the title, link, text,
//!    author and the whole discussion (with commenters' names) as plain text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::record::Record;
use crate::text::html_to_text;

pub const SOURCE: &str = "hacker_news";

const STORIES: TableDefinition<u64, &[u8]> = TableDefinition::new("stories");
const PARENTS: TableDefinition<u64, u64> = TableDefinition::new("parents");
const TEXTS: TableDefinition<u64, &str> = TableDefinition::new("texts");
const THREADS: TableDefinition<(u64, u64), u64> = TableDefinition::new("threads");

/// Rows per write transaction in pass 1.
const CHUNK: usize = 50_000;

/// Columns that identify a BigQuery Hacker News export.
const REQUIRED: &[&str] = &["id", "type", "parent", "time"];

/// Whether the start of a file looks like a Hacker News CSV export.
pub fn detect(head: &str) -> bool {
    let header = head.lines().next().unwrap_or("");
    let columns: Vec<String> = header
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_ascii_lowercase())
        .collect();
    REQUIRED.iter().all(|r| columns.iter().any(|c| c == r)) && columns.iter().any(|c| c == "by")
}

#[derive(Serialize, Deserialize)]
struct StoryRow {
    by: String,
    title: String,
    url: String,
    text: String,
}

#[derive(Default)]
struct Stats {
    rows: u64,
    stories: u64,
    comments: u64,
    attached: u64,
    orphans: u64,
    skipped_dead: u64,
    skipped_other: u64,
}

/// Deletes the temporary database when the import ends, however it ends.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Reads one or more CSV files (BigQuery splits large exports) as one import.
/// `tmp_dir` holds the temporary database. Returns notes for the import log.
pub fn import(
    files: &[PathBuf],
    tmp_dir: &Path,
    sink: &mut dyn FnMut(Record) -> Result<()>,
) -> Result<Vec<String>> {
    let tmp_path = tmp_dir.join(format!("hn-import-{}.redb", std::process::id()));
    let _cleanup = RemoveOnDrop(tmp_path.clone());
    let db = Database::create(&tmp_path)?;
    let mut stats = Stats::default();

    // A file that can't be read to the end doesn't stop the others; what was
    // read before the problem is kept.
    let mut problems = Vec::new();
    for file in files {
        if let Err(e) = read_file(&db, file, &mut stats) {
            problems.push(e.to_string());
        }
    }
    link_threads(&db, &mut stats)?;
    emit(&db, sink)?;

    let mut notes = problems;
    notes.push(format!(
        "{} rows: {} live stories, {} comments ({} attached to their story)",
        stats.rows, stats.stories, stats.comments, stats.attached
    ));
    if stats.orphans > 0 {
        notes.push(format!(
            "{} comments skipped: their story isn't in this import",
            stats.orphans
        ));
    }
    if stats.skipped_dead > 0 {
        notes.push(format!(
            "{} dead or deleted items skipped",
            stats.skipped_dead
        ));
    }
    if stats.skipped_other > 0 {
        notes.push(format!(
            "{} rows skipped (unknown type, missing id or empty)",
            stats.skipped_other
        ));
    }
    Ok(notes)
}

/// Pass 1.
fn read_file(db: &Database, file: &Path, stats: &mut Stats) -> Result<()> {
    // Plain or gzip-compressed CSV (BigQuery can export either).
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(super::open_input(file)?);

    // Invalid UTF-8 is replaced rather than rejected, so one bad byte can't stop an import.
    let headers = csv::StringRecord::from_byte_record_lossy(
        reader
            .byte_headers()
            .map_err(|e| format!("{}: {e}", file.display()))?
            .clone(),
    );
    let column = |name: &str| {
        headers.iter().position(|h| {
            h.trim()
                .trim_start_matches('\u{feff}')
                .eq_ignore_ascii_case(name)
        })
    };
    let (c_id, c_type, c_parent) = (column("id"), column("type"), column("parent"));
    let (c_title, c_url, c_text, c_by) =
        (column("title"), column("url"), column("text"), column("by"));
    let (c_dead, c_deleted) = (column("dead"), column("deleted"));
    if c_id.is_none() || c_type.is_none() || c_parent.is_none() {
        return Err(format!(
            "{} doesn't look like a Hacker News export (needs id, type and parent columns)",
            file.display()
        )
        .into());
    }

    let mut records = reader.byte_records();
    let mut failure = None;
    loop {
        let mut txn = db.begin_write()?;
        txn.set_durability(Durability::None)?;
        let mut done = false;
        {
            let mut stories = txn.open_table(STORIES)?;
            let mut parents = txn.open_table(PARENTS)?;
            let mut texts = txn.open_table(TEXTS)?;

            for _ in 0..CHUNK {
                let Some(row) = records.next() else {
                    done = true;
                    break;
                };
                let row = match row {
                    Ok(row) => csv::StringRecord::from_byte_record_lossy(row),
                    Err(e) => {
                        failure = Some(format!(
                            "{}: stopped reading after row {}: {e}",
                            file.display(),
                            stats.rows
                        ));
                        done = true;
                        break;
                    }
                };
                stats.rows += 1;
                let get = |c: Option<usize>| c.and_then(|i| row.get(i)).unwrap_or("").trim();

                let Ok(id) = get(c_id).parse::<u64>() else {
                    stats.skipped_other += 1;
                    continue;
                };
                let dead = get(c_dead) == "true" || get(c_deleted) == "true";

                match get(c_type) {
                    "comment" | "pollopt" => {
                        stats.comments += 1;
                        if let Ok(parent) = get(c_parent).parse::<u64>() {
                            // Dead comments still link their replies to the story.
                            parents.insert(id, parent)?;
                        }
                        let text = html_to_text(get(c_text));
                        if dead {
                            stats.skipped_dead += 1;
                        } else if !text.is_empty() {
                            let by = get(c_by);
                            let text = if by.is_empty() {
                                text
                            } else {
                                format!("{by}: {text}")
                            };
                            texts.insert(id, text.as_str())?;
                        }
                    }
                    "story" | "job" | "poll" => {
                        let title = html_to_text(get(c_title));
                        if dead {
                            stats.skipped_dead += 1;
                        } else if title.is_empty() {
                            stats.skipped_other += 1;
                        } else {
                            stats.stories += 1;
                            let story = StoryRow {
                                by: get(c_by).to_string(),
                                title,
                                url: get(c_url).to_string(),
                                text: html_to_text(get(c_text)),
                            };
                            stories.insert(id, serde_json::to_vec(&story)?.as_slice())?;
                        }
                    }
                    _ => stats.skipped_other += 1,
                }
            }
        }
        txn.commit()?;
        if done {
            return match failure {
                Some(message) => Err(message.into()),
                None => Ok(()),
            };
        }
    }
}

/// Pass 2: find each live comment's story.
fn link_threads(db: &Database, stats: &mut Stats) -> Result<()> {
    let mut txn = db.begin_write()?;
    txn.set_durability(Durability::None)?;
    {
        let stories = txn.open_table(STORIES)?;
        let parents = txn.open_table(PARENTS)?;
        let texts = txn.open_table(TEXTS)?;
        let mut threads = txn.open_table(THREADS)?;

        for entry in texts.iter()? {
            let (id, _) = entry?;
            let id = id.value();
            let Some(mut parent) = parents.get(id)?.map(|g| g.value()) else {
                stats.orphans += 1;
                continue;
            };
            // Replies to dead or deleted comments hang off the nearest live ancestor.
            let mut live_parent = None;
            let mut root = None;
            for _ in 0..10_000 {
                if stories.get(parent)?.is_some() {
                    root = Some(parent);
                    break;
                }
                if live_parent.is_none() && texts.get(parent)?.is_some() {
                    live_parent = Some(parent);
                }
                match parents.get(parent)?.map(|g| g.value()) {
                    Some(next) => parent = next,
                    None => break,
                }
            }
            match root {
                Some(root) => {
                    threads.insert((root, id), live_parent.unwrap_or(root))?;
                    stats.attached += 1;
                }
                None => stats.orphans += 1,
            }
        }
    }
    txn.commit()?;
    Ok(())
}

/// Pass 3: one record per story.
fn emit(db: &Database, sink: &mut dyn FnMut(Record) -> Result<()>) -> Result<()> {
    let txn = db.begin_read()?;
    let stories = txn.open_table(STORIES)?;
    let threads = txn.open_table(THREADS)?;
    let texts = txn.open_table(TEXTS)?;

    for entry in stories.iter()? {
        let (id, value) = entry?;
        let id = id.value();
        let story: StoryRow = serde_json::from_slice(value.value())?;

        // The thread, in order: children of each item sorted by id (i.e. by time).
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for item in threads.range((id, 0)..=(id, u64::MAX))? {
            let (key, parent) = item?;
            let (_, comment) = key.value();
            children.entry(parent.value()).or_default().push(comment);
        }
        let mut comments = Vec::new();
        let mut stack: Vec<u64> = children.get(&id).cloned().unwrap_or_default();
        stack.reverse();
        while let Some(comment) = stack.pop() {
            if let Some(text) = texts.get(comment)? {
                comments.push(text.value().to_string());
            }
            if let Some(replies) = children.get(&comment) {
                stack.extend(replies.iter().rev());
            }
        }

        // Everything is a page: the HN item page, with the discussion as its text.
        let mut body = Vec::new();
        if !story.url.is_empty() {
            // "Title (example.com)", as on HN; the link itself is in `links`.
            match link_host(&story.url) {
                Some(host) => body.push(format!("{} ({host})", story.title)),
                None => body.push(story.title.clone()),
            }
        }
        if !story.text.is_empty() {
            body.push(story.text);
        }
        if !story.by.is_empty() {
            body.push(format!("Submitted by {}", story.by));
        }
        body.extend(comments);

        let record = Record {
            url: format!("https://news.ycombinator.com/item?id={id}"),
            source: SOURCE,
            title: Some(story.title),
            body: body.join("\n\n"),
            links: if story.url.is_empty() {
                Vec::new()
            } else {
                vec![story.url]
            },
            crawled: None,
        };
        sink(record)?;
    }
    Ok(())
}

/// The host a story links to, without `www.`, as HN shows it beside the title.
fn link_host(url: &str) -> Option<String> {
    let host = crate::urls::Url::parse(url)?.host;
    let host = host.strip_prefix("www.").unwrap_or(&host);
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_hosts() {
        assert_eq!(
            link_host("https://www.Example.com/a?b=c").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            link_host("http://blog.rust-lang.org/").as_deref(),
            Some("blog.rust-lang.org")
        );
        assert_eq!(link_host("not a url"), None);
    }

    #[test]
    fn detects_bigquery_header() {
        assert!(detect(
            "title,url,text,dead,by,score,time,timestamp,type,id,parent,descendants,ranking,deleted\n,,x"
        ));
        assert!(!detect("id,name\n1,x"));
    }
}
