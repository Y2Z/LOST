//! Records and documents (ARCHITECTURE.md §6.2, §6.4, §8.5).
//!
//! Everything is a page: a URL with a title and plain-text body. A [`Record`]
//! is what a data source produces; a [`Doc`] is what the index holds, exactly
//! one per normalized URL. When a record arrives for a URL that already has a
//! document, [`merge`] decides which version to keep. Dependency-free and tested.

use crate::MAX_BODY_BYTES;
use crate::text::truncate_at_word;
use crate::urls::Url;

/// One page produced by a data source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub url: String,
    /// Data source name, e.g. `hacker_news`.
    pub source: &'static str,
    pub title: Option<String>,
    /// Plain text.
    pub body: String,
    /// Outgoing links.
    pub links: Vec<String>,
    /// When the page was captured (Unix seconds), if known.
    pub crawled: Option<i64>,
}

/// One indexed document: what LOST knows about one URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    /// `blake3(normalized URL)`, hex.
    pub key: String,
    /// The original URL, for display.
    pub url: String,
    pub sources: Vec<String>,
    pub title: Option<String>,
    pub body: String,
    pub links: Vec<String>,
    pub crawled: Option<i64>,
    /// The import this version of the page came from.
    pub import_id: u64,
}

impl Doc {
    /// Builds the document for a record. Returns `None` for an unusable URL.
    pub fn from_record(record: Record, import_id: u64) -> Option<Doc> {
        let key = url_key(&record.url)?;
        Some(Doc {
            key,
            url: record.url,
            sources: vec![record.source.to_string()],
            title: record.title.filter(|t| !t.trim().is_empty()),
            body: truncate_at_word(&record.body, MAX_BODY_BYTES).to_string(),
            links: record.links,
            crawled: record.crawled,
            import_id,
        })
    }
}

/// The key of a URL as stored in the index, or `None` if it isn't a URL.
pub fn url_key(url: &str) -> Option<String> {
    Url::parse(url).map(|u| blake3_hex(u.normalized().as_bytes()))
}

#[cfg(not(test))]
fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

// Tests run without dependencies; any stable function of the input will do.
#[cfg(test)]
fn blake3_hex(bytes: &[u8]) -> String {
    format!("test:{}", String::from_utf8_lossy(bytes))
}

/// Merges an incoming version of a page into the existing one
/// (ARCHITECTURE.md §6.4):
///
/// 1. Same page — the same capture (equal crawl times, e.g. a WET and a WAT
///    record from one Common Crawl segment), or the same title with one body
///    containing the other: keep the fuller one, filling its gaps (title,
///    links, crawl date) from the other.
/// 2. Different content: the newer crawl wins.
/// 3. Crawl date unknown on either side: the one with more text wins.
///
/// The loser is discarded. On a tie the existing version stays, so importing
/// the same data twice changes nothing.
pub fn merge(existing: Doc, incoming: Doc) -> Doc {
    if same_page(&existing, &incoming) {
        let (mut winner, loser) = if incoming.body.len() > existing.body.len() {
            (incoming, existing)
        } else {
            (existing, incoming)
        };
        if winner.title.is_none() {
            winner.title = loser.title;
        }
        if winner.links.is_empty() {
            winner.links = loser.links;
        }
        if winner.crawled.is_none() {
            winner.crawled = loser.crawled;
        }
        for source in loser.sources {
            if !winner.sources.contains(&source) {
                winner.sources.push(source);
            }
        }
        return winner;
    }

    let incoming_wins = match (existing.crawled, incoming.crawled) {
        (Some(old), Some(new)) if old != new => new > old,
        _ => incoming.body.len() > existing.body.len(),
    };
    if incoming_wins { incoming } else { existing }
}

fn same_page(a: &Doc, b: &Doc) -> bool {
    if a.crawled.is_some() && a.crawled == b.crawled {
        return true;
    }
    let titles_match = match (&a.title, &b.title) {
        (Some(x), Some(y)) => x.trim() == y.trim(),
        _ => true,
    };
    titles_match && (a.body.contains(b.body.as_str()) || b.body.contains(a.body.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(title: &str, body: &str, crawled: Option<i64>, import_id: u64) -> Doc {
        Doc::from_record(
            Record {
                url: "https://example.com/a".into(),
                source: "test",
                title: Some(title.into()),
                body: body.into(),
                crawled,
                ..Record::default()
            },
            import_id,
        )
        .unwrap()
    }

    #[test]
    fn identical_reimport_changes_nothing() {
        let a = doc("T", "hello", None, 1);
        let b = doc("T", "hello", None, 2);
        assert_eq!(merge(a.clone(), b), a);
    }

    #[test]
    fn same_page_keeps_fuller_and_fills_gaps() {
        let mut short = doc("T", "hello", None, 1);
        short.links = vec!["https://x".into()];
        let long = doc("T", "hello world", Some(10), 2);
        let merged = merge(short, long);
        assert_eq!(merged.body, "hello world");
        assert_eq!(merged.links, vec!["https://x"]);
        assert_eq!(merged.import_id, 2);
    }

    #[test]
    fn same_capture_combines_text_and_links() {
        // A WAT record (title, description, links) and the WET record (full text)
        // of the same capture.
        let mut wat = doc("Catfish", "Recipes for catfish.", Some(7), 1);
        wat.links = vec!["https://x.com/".into()];
        let mut wet = doc("Catfish", "Fry the fillets. Serve hot.", Some(7), 2);
        wet.title = None;
        let merged = merge(wat, wet);
        assert_eq!(merged.body, "Fry the fillets. Serve hot.");
        assert_eq!(merged.title.as_deref(), Some("Catfish"));
        assert_eq!(merged.links, vec!["https://x.com/"]);
    }

    #[test]
    fn different_page_newer_crawl_wins() {
        let old = doc("Old", "old long text here", Some(10), 1);
        let new = doc("New", "new", Some(20), 2);
        assert_eq!(merge(old.clone(), new.clone()).body, "new");
        assert_eq!(merge(new, old).body, "new");
    }

    #[test]
    fn different_page_unknown_date_more_text_wins() {
        let a = doc("A", "short", None, 1);
        let b = doc("B", "much longer text", Some(1), 2);
        assert_eq!(merge(a.clone(), b.clone()).body, "much longer text");
        assert_eq!(merge(b, a).body, "much longer text");
    }

    #[test]
    fn hn_thread_with_more_comments_replaces_older_copy() {
        let before = doc("Show HN: x", "https://x.com\n\nalice: nice", None, 1);
        let after = doc(
            "Show HN: x",
            "https://x.com\n\nalice: nice\n\nbob: agreed",
            None,
            2,
        );
        assert_eq!(merge(before, after.clone()), after);
    }
}
