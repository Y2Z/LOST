//! CLI result output in `plain` and `json` formats (ARCHITECTURE.md §3.4).
//!
//! The `json` output is the same object the Web API returns for `/api/find`.

use std::io::{self, Write};

use crate::engine::{CachedPage, SearchResults};
use crate::text::window;

/// Bold red, like grep's matches.
const MATCH_START: &str = "\x1b[1;31m";
const MATCH_END: &str = "\x1b[0m";

/// The first match in a snippet is shown at most this many characters from its start.
const SNIPPET_LEAD: usize = 10;

/// How plain results are drawn.
#[derive(Debug, Clone, Copy, Default)]
pub struct Style {
    /// Highlight the query's words (terminal colour codes).
    pub color: bool,
    /// Terminal width in columns; titles, snippets and URLs are fitted to it,
    /// with the same margin on both sides. `None` (a file or pipe): left whole.
    pub width: Option<usize>,
}

/// One block per result — title, snippet, URL — separated by blank lines. On a terminal every line is fitted to its width,
/// so nothing wraps.
pub fn plain(results: &SearchResults, out: &mut dyn Write, style: Style) -> io::Result<()> {
    const INDENT: &str = "   ";
    // Fits a line's text into what's left of the terminal after `used`
    // columns, keeping the same margin on the right as INDENT on the left.
    let fit = |text: &str, matches: &[[usize; 2]], used: usize| -> String {
        let (text, matches) = match style.width {
            Some(w) => window(
                text,
                matches,
                w.saturating_sub(used + INDENT.len()),
                SNIPPET_LEAD,
            ),
            None => (text.to_string(), matches.to_vec()),
        };
        paint(&text, &matches, style.color)
    };

    for (i, hit) in results.results.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        let number = format!("{}. ", results.offset + i + 1);
        let title = match &hit.title {
            Some(t) => fit(t, &hit.title_matches, number.chars().count()),
            None => fit(&hit.url, &[], number.chars().count()),
        };
        writeln!(out, "{number}{title}")?;
        if !hit.snippet.is_empty() {
            let snippet = fit(&hit.snippet, &hit.snippet_matches, INDENT.len());
            writeln!(out, "{INDENT}{snippet}")?;
        }
        writeln!(out, "{INDENT}{}", fit(&hit.url, &[], INDENT.len()))?;
    }
    Ok(())
}

/// Wraps the given character ranges of `text` in terminal colour codes.
pub fn paint(text: &str, ranges: &[[usize; 2]], color: bool) -> String {
    if !color || ranges.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + ranges.len() * 12);
    let mut ranges = ranges.iter().peekable();
    let mut inside = false;
    for (i, c) in text.chars().enumerate() {
        if inside && ranges.peek().is_some_and(|r| i >= r[1]) {
            out.push_str(MATCH_END);
            inside = false;
            ranges.next();
        }
        if !inside && ranges.peek().is_some_and(|r| i >= r[0] && i < r[1]) {
            out.push_str(MATCH_START);
            inside = true;
        }
        out.push(c);
    }
    if inside {
        out.push_str(MATCH_END);
    }
    out
}

/// A stored page (`cache:URL`): title, URL, capture date and sources, a
/// blank line, then the text. On a terminal, long lines are wrapped at
/// word boundaries to fit (the text is for reading, so nothing is cut);
/// elsewhere the text is written as stored.
pub fn page(page: &CachedPage, out: &mut dyn Write, style: Style) -> io::Result<()> {
    let bold = |text: &str| {
        if style.color {
            format!("\x1b[1m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    if let Some(title) = &page.title {
        writeln!(out, "{}", bold(title))?;
    }
    writeln!(out, "{}", page.url)?;
    let sources: Vec<String> = page
        .sources
        .iter()
        .map(|s| crate::text::source_label(s))
        .collect();
    match &page.crawled {
        Some(date) => writeln!(out, "Captured {date} · {}", sources.join(", "))?,
        None => writeln!(out, "{}", sources.join(", "))?,
    }
    writeln!(out)?;
    for line in page.text.lines() {
        match style.width {
            Some(width) => {
                for part in wrap(line, width.saturating_sub(3).max(20)) {
                    writeln!(out, "{part}")?;
                }
            }
            None => writeln!(out, "{line}")?,
        }
    }
    Ok(())
}

/// Splits a line into lines of at most `width` characters, at spaces where
/// possible (a longer word is broken).
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut length = 0;
    for word in line.split(' ') {
        let mut word = word;
        let mut size = word.chars().count();
        if length > 0 && length + 1 + size > width {
            lines.push(std::mem::take(&mut current));
            length = 0;
        }
        while size > width {
            let cut = word
                .char_indices()
                .nth(width)
                .map_or(word.len(), |(i, _)| i);
            if length > 0 {
                lines.push(std::mem::take(&mut current));
                length = 0;
            }
            lines.push(word[..cut].to_string());
            word = &word[cut..];
            size -= width;
        }
        if length > 0 {
            current.push(' ');
            length += 1;
        }
        current.push_str(word);
        length += size;
    }
    lines.push(current);
    lines
}

/// A Unix time as the user's local date and time: `2026-10-10 16:02`.
/// (Everything underneath is UTC.)
pub fn local_datetime(timestamp: i64) -> String {
    jiff::Timestamp::from_second(timestamp)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| crate::text::format_datetime(timestamp))
}

/// A Unix time as the user's local time of day: `16:02:15`.
pub fn local_time(timestamp: i64) -> String {
    jiff::Timestamp::from_second(timestamp)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| {
            let s = timestamp.rem_euclid(86_400);
            format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
        })
}

/// A Unix time as the user's local date: `2026-10-10`.
pub fn local_date(timestamp: i64) -> String {
    jiff::Timestamp::from_second(timestamp)
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|_| crate::text::format_date(timestamp))
}

/// The style for results written to stdout: colour and width when stdout is a
/// terminal (colour unless `NO_COLOR` is set or `TERM=dumb`), plain otherwise.
pub fn terminal_style() -> Style {
    use std::io::IsTerminal;
    if !io::stdout().is_terminal() {
        return Style::default();
    }
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb");
    let width = terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize);
    Style { color, width }
}

/// The Web API's JSON, pretty-printed.
pub fn json(results: &SearchResults, out: &mut dyn Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, results)?;
    writeln!(out)
}

/// The summary line printed to stderr: "Found 1,234 results, showing 1–50".
pub fn summary(results: &SearchResults) -> String {
    let shown = results.results.len();
    if results.total == 0 {
        return format!("Found no results for \"{}\"", results.query);
    }
    if shown == 0 {
        return format!(
            "Found {} results, none past result {}",
            thousands(results.total as u64),
            thousands(results.offset as u64)
        );
    }
    format!(
        "Found {} result{}, showing {}–{}",
        thousands(results.total as u64),
        if results.total == 1 { "" } else { "s" },
        thousands((results.offset + 1) as u64),
        thousands((results.offset + shown) as u64)
    )
}

pub use crate::text::thousands;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("", 5), vec![""]);
        assert_eq!(wrap("abcdefghij k", 4), vec!["abcd", "efgh", "ij k"]);
        assert_eq!(wrap("a  b", 10), vec!["a  b"]);
    }

    #[test]
    fn painting() {
        assert_eq!(paint("a catfish b", &[[2, 9]], false), "a catfish b");
        assert_eq!(
            paint("a catfish b", &[[2, 9]], true),
            "a \x1b[1;31mcatfish\x1b[0m b"
        );
        assert_eq!(
            paint("ab cd", &[[0, 2], [3, 5]], true),
            "\x1b[1;31mab\x1b[0m \x1b[1;31mcd\x1b[0m"
        );
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
