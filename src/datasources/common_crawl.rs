//! Common Crawl WET and WAT files (ARCHITECTURE.md §8.3). WARC is not supported.
//!
//! - **WET**: the plain text of each page. Common Crawl puts the page's
//!   `<title>` on the first line, which becomes the title here.
//! - **WAT**: metadata for each page as JSON: title, description and links.
//!
//! WET and WAT files from the same segment describe the same captures; they
//! merge into one page per URL (§6.4), whichever is imported first.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::Result;
use crate::record::Record;
use crate::text::parse_timestamp;
use crate::urls::{Url, resolve};
use crate::warc::{self, Next};

pub const SOURCE: &str = "common_crawl";

/// A first line longer than this isn't a title.
const MAX_TITLE_CHARS: usize = 300;

/// Outgoing links kept per page.
const MAX_LINKS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Wet,
    Wat,
}

impl Flavor {
    fn label(self) -> &'static str {
        match self {
            Flavor::Wet => "WET",
            Flavor::Wat => "WAT",
        }
    }
}

/// Whether the (decompressed) start of a file is a WET or WAT file.
pub fn detect(head: &[u8]) -> Option<Flavor> {
    let text = String::from_utf8_lossy(head);
    if !text.trim_start().starts_with("WARC/1.") {
        return None;
    }
    for line in text.lines() {
        if let Some(kind) = line.strip_prefix("WARC-Type:") {
            match kind.trim() {
                "conversion" => return Some(Flavor::Wet),
                "metadata" => return Some(Flavor::Wat),
                "warcinfo" => continue,
                // request / response / resource: a full WARC file.
                _ => return None,
            }
        }
    }
    // Only the file's info record fit in the sample: go by the file name it gives.
    if text.contains(".warc.wet.gz") {
        Some(Flavor::Wet)
    } else if text.contains(".warc.wat.gz") {
        Some(Flavor::Wat)
    } else {
        None
    }
}

/// Reads one WET or WAT file. Problems reading the file end it early with a
/// note rather than failing the import; errors from `sink` (the index) do fail it.
pub fn import(
    path: &Path,
    flavor: Flavor,
    sink: &mut dyn FnMut(Record) -> Result<()>,
) -> Result<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut reader = warc::Reader::new(super::open_input(path)?);
    let mut pages = 0u64;
    let mut skipped = 0u64;

    let ending = loop {
        match reader.next_record() {
            Ok(Next::Record(record)) => {
                let page = match flavor {
                    Flavor::Wet => from_wet(&record),
                    Flavor::Wat => from_wat(&record),
                };
                match page {
                    Some(page) => {
                        pages += 1;
                        sink(page)?;
                    }
                    None => skipped += 1,
                }
            }
            Ok(Next::End) => break String::new(),
            Ok(Next::Truncated) => {
                break "; the file ends in the middle of a record (cut-off download?)".to_string();
            }
            Err(e) => break format!("; stopped reading: {e}"),
        }
    };
    Ok(format!(
        "{name}: {pages} pages from {} records ({skipped} other records skipped){ending}",
        flavor.label()
    ))
}

/// A WET `conversion` record: the page's text, title on the first line.
fn from_wet(record: &warc::Record) -> Option<Record> {
    if record.header("WARC-Type") != Some("conversion") {
        return None;
    }
    let url = record.header("WARC-Target-URI")?.to_string();
    let crawled = record.header("WARC-Date").and_then(parse_timestamp);
    let text = String::from_utf8_lossy(&record.body);
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');

    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let first = first.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (title, body) = if !first.is_empty() && first.chars().count() <= MAX_TITLE_CHARS {
        (Some(first.to_string()), rest.trim().to_string())
    } else {
        (None, text.to_string())
    };
    if title.is_none() && body.is_empty() {
        return None;
    }
    Some(Record {
        url,
        source: SOURCE,
        title,
        body,
        links: Vec::new(),
        crawled,
    })
}

/// A WAT `metadata` record describing an HTML response: title, description, links.
fn from_wat(record: &warc::Record) -> Option<Record> {
    if record.header("WARC-Type") != Some("metadata") {
        return None;
    }
    let json: Value = serde_json::from_slice(&record.body).ok()?;
    let envelope = json.get("Envelope")?;
    let warc = envelope.get("WARC-Header-Metadata")?;
    if warc.get("WARC-Type").and_then(Value::as_str) != Some("response") {
        return None;
    }
    let url = warc
        .get("WARC-Target-URI")
        .and_then(Value::as_str)?
        .to_string();
    let crawled = warc
        .get("WARC-Date")
        .and_then(Value::as_str)
        .and_then(parse_timestamp);

    let response = envelope.pointer("/Payload-Metadata/HTTP-Response-Metadata")?;
    let status = response
        .pointer("/Response-Message/Status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !status.starts_with('2') {
        return None;
    }
    let html = response.get("HTML-Metadata")?;

    let title = html
        .pointer("/Head/Title")
        .and_then(Value::as_str)
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty());

    let description = html
        .pointer("/Head/Metas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|meta| {
            ["name", "property"].into_iter().any(|attr| {
                meta.get(attr).and_then(Value::as_str).is_some_and(|n| {
                    n.eq_ignore_ascii_case("description")
                        || n.eq_ignore_ascii_case("og:description")
                })
            })
        })
        .and_then(|meta| meta.get("content"))
        .and_then(Value::as_str)
        .map(|d| d.trim().to_string())
        .unwrap_or_default();

    let base = Url::parse(&url)?;
    let mut seen = HashSet::new();
    let links: Vec<String> = html
        .get("Links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|link| link.get("path").and_then(Value::as_str) == Some("A@/href"))
        .filter_map(|link| link.get("url").and_then(Value::as_str))
        .filter_map(|href| resolve(&base, href))
        .filter(|link| seen.insert(link.clone()))
        .take(MAX_LINKS)
        .collect();

    Some(Record {
        url,
        source: SOURCE,
        title,
        body: description,
        links,
        crawled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_flavors() {
        let wet = b"WARC/1.0\r\nWARC-Type: warcinfo\r\n\r\nx\r\n\r\nWARC/1.0\r\nWARC-Type: conversion\r\n";
        let wat =
            b"WARC/1.0\r\nWARC-Type: warcinfo\r\n\r\nx\r\n\r\nWARC/1.0\r\nWARC-Type: metadata\r\n";
        let warc =
            b"WARC/1.0\r\nWARC-Type: warcinfo\r\n\r\nx\r\n\r\nWARC/1.0\r\nWARC-Type: request\r\n";
        assert_eq!(detect(wet), Some(Flavor::Wet));
        assert_eq!(detect(wat), Some(Flavor::Wat));
        assert_eq!(detect(warc), None);
        assert_eq!(detect(b"id,type,parent"), None);
    }

    #[test]
    fn wet_title_comes_from_the_first_line() {
        let record = warc::Record {
            headers: vec![
                ("WARC-Type".into(), "conversion".into()),
                ("WARC-Target-URI".into(), "http://a.com/x".into()),
                ("WARC-Date".into(), "2026-09-04T14:43:16Z".into()),
            ],
            body: "\u{feff} Catfish recipes\nFry them.\nEat them."
                .as_bytes()
                .to_vec(),
        };
        let page = from_wet(&record).unwrap();
        assert_eq!(page.title.as_deref(), Some("Catfish recipes"));
        assert_eq!(page.body, "Fry them.\nEat them.");
        assert_eq!(page.crawled, Some(1_788_532_996));
    }
}
