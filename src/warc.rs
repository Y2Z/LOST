//! A minimal WARC record reader (ARCHITECTURE.md §8.3), enough for Common
//! Crawl's WET and WAT files. Dependency-free and tested; decompression
//! happens before the bytes get here.

use std::io::{self, BufRead, Read};

/// One WARC record: its header fields and its content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Record {
    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What reading the next record gave.
#[derive(Debug)]
pub enum Next {
    Record(Record),
    /// The input ended cleanly between records.
    End,
    /// The input ended in the middle of a record (a cut-off download).
    Truncated,
}

/// Reads WARC records one by one.
pub struct Reader<R> {
    input: R,
    line: Vec<u8>,
}

/// Content blocks larger than this are skipped rather than read into memory.
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

impl<R: BufRead> Reader<R> {
    pub fn new(input: R) -> Reader<R> {
        Reader {
            input,
            line: Vec::new(),
        }
    }

    pub fn next_record(&mut self) -> io::Result<Next> {
        // Find the version line, skipping the blank lines between records.
        loop {
            self.line.clear();
            if self.input.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(Next::End);
            }
            let line = trim_eol(&self.line);
            if line.is_empty() {
                continue;
            }
            if line.starts_with(b"WARC/") {
                break;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a WARC file (expected a WARC/1.x line)",
            ));
        }

        let mut headers = Vec::new();
        loop {
            self.line.clear();
            if self.input.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(Next::Truncated);
            }
            let line = trim_eol(&self.line);
            if line.is_empty() {
                break;
            }
            let line = String::from_utf8_lossy(line);
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_string(), value.trim().to_string()));
            }
        }

        let length: u64 = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.parse().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WARC record without Content-Length",
                )
            })?;

        if length > MAX_RECORD_BYTES {
            let skipped = io::copy(&mut (&mut self.input).take(length), &mut io::sink())?;
            if skipped < length {
                return Ok(Next::Truncated);
            }
            return Ok(Next::Record(Record {
                headers,
                body: Vec::new(),
            }));
        }

        let mut body = Vec::with_capacity(length as usize);
        (&mut self.input).take(length).read_to_end(&mut body)?;
        if (body.len() as u64) < length {
            return Ok(Next::Truncated);
        }
        Ok(Next::Record(Record { headers, body }))
    }
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(input: &str) -> (Vec<Record>, bool) {
        let mut reader = Reader::new(input.as_bytes());
        let mut out = Vec::new();
        loop {
            match reader.next_record().unwrap() {
                Next::Record(r) => out.push(r),
                Next::End => return (out, false),
                Next::Truncated => return (out, true),
            }
        }
    }

    #[test]
    fn reads_records_and_detects_truncation() {
        let input = "WARC/1.0\r\nWARC-Type: conversion\r\nWARC-Target-URI: http://a.com/\r\nContent-Length: 5\r\n\r\nhello\r\n\r\nWARC/1.0\r\nWARC-Type: conversion\r\nContent-Length: 3\r\n\r\nabc\r\n\r\n";
        let (recs, truncated) = records(input);
        assert!(!truncated);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].header("warc-target-uri"), Some("http://a.com/"));
        assert_eq!(recs[0].body, b"hello");
        assert_eq!(recs[1].body, b"abc");

        let cut = &input[..input.len() - 6];
        let (recs, truncated) = records(cut);
        assert_eq!(recs.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn rejects_other_files() {
        let mut reader = Reader::new(&b"id,type\n1,story\n"[..]);
        assert!(reader.next_record().is_err());
    }
}
