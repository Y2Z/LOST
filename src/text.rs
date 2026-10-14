//! Small text helpers shared by data sources: HTML to plain text, truncation,
//! and date formatting. Dependency-free and tested.

/// Converts HTML into plain text: the simple HTML of sources like Hacker
/// News, and whole web pages.
///
/// Paragraphs, headings and other blocks become blank lines, line breaks
/// newlines, table cells spaces; every other tag is dropped (keeping its
/// text), and character references (`&amp;`, `&#x27;`, `&#62;`, …) are
/// decoded. What isn't text — scripts, styles, comments, SVG, templates —
/// is left out. Not a full HTML parser.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(i) = rest.find(['<', '&']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];

        if rest.starts_with("<!--") {
            rest = rest[4..].find("-->").map_or("", |end| &rest[4 + end + 3..]);
            continue;
        }
        if rest.starts_with('<') {
            match rest.find('>') {
                Some(end) => {
                    let tag = tag_name(&rest[1..end]);
                    rest = &rest[end + 1..];
                    match tag.as_str() {
                        "script" | "style" | "svg" | "template" | "noembed" => {
                            // Up to the closing tag, which is all that ends them.
                            let lower = rest.to_ascii_lowercase();
                            let close = format!("</{tag}");
                            rest = match lower.find(&close) {
                                Some(at) => match rest[at..].find('>') {
                                    Some(gt) => &rest[at + gt + 1..],
                                    None => "",
                                },
                                None => "",
                            };
                        }
                        "p" | "/p" | "div" | "/div" | "pre" | "/pre" | "li" | "h1" | "/h1"
                        | "h2" | "/h2" | "h3" | "/h3" | "h4" | "/h4" | "h5" | "/h5" | "h6"
                        | "/h6" | "ul" | "/ul" | "ol" | "/ol" | "table" | "/table" | "tr"
                        | "blockquote" | "/blockquote" | "section" | "/section" | "article"
                        | "/article" | "header" | "/header" | "footer" | "/footer" | "nav"
                        | "/nav" | "main" | "/main" | "aside" | "/aside" | "form" | "/form"
                        | "dl" | "/dl" | "dt" | "dd" | "hr" | "hr/" | "figure" | "/figure"
                        | "title" | "/title" => push_break(&mut out, 2),
                        "br" | "br/" => push_break(&mut out, 1),
                        "td" | "th" if !out.is_empty() && !out.ends_with(char::is_whitespace) => {
                            out.push(' ')
                        }
                        _ => {}
                    }
                }
                None => {
                    // A lone '<' is text.
                    out.push('<');
                    rest = &rest[1..];
                }
            }
        } else {
            // '&'
            match decode_entity(rest) {
                Some((decoded, len)) => {
                    out.push(decoded);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

fn tag_name(inside: &str) -> String {
    let inside = inside.trim();
    let end = inside
        .find(|c: char| c.is_whitespace())
        .unwrap_or(inside.len());
    inside[..end].to_ascii_lowercase()
}

fn push_break(out: &mut String, newlines: usize) {
    if out.is_empty() {
        return;
    }
    while out.ends_with(' ') {
        out.pop();
    }
    let existing = out.chars().rev().take_while(|&c| c == '\n').count();
    for _ in existing..newlines {
        out.push('\n');
    }
}

/// Decodes the character reference at the start of `s` (which starts with `&`).
/// Returns the character and how many bytes it spanned.
fn decode_entity(s: &str) -> Option<(char, usize)> {
    let (end, _) = s.char_indices().take(12).find(|&(_, c)| c == ';')?;
    let name = &s[1..end];
    let c = if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        char::from_u32(code)?
    } else {
        match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => ' ',
            "ndash" => '–',
            "mdash" => '—',
            "hellip" => '…',
            "copy" => '©',
            _ => return None,
        }
    };
    Some((c, end + 1))
}

/// Cuts `text` to at most `max_bytes`, at a word boundary when there is one.
pub fn truncate_at_word(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &text[..end];
    match cut.rfind(char::is_whitespace) {
        Some(i) if i > 0 => cut[..i].trim_end(),
        _ => cut,
    }
}

/// Turns byte ranges into `[start, end)` character ranges, sorted, with
/// overlapping or touching ranges joined. Character offsets are what the Web
/// API reports, so front ends in any language can apply them.
pub fn char_ranges(text: &str, byte_ranges: &[(usize, usize)]) -> Vec<[usize; 2]> {
    let mut ranges: Vec<(usize, usize)> = byte_ranges
        .iter()
        .copied()
        .filter(|&(s, e)| s < e && e <= text.len())
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let to_char = |byte: usize| {
        let mut b = byte;
        while !text.is_char_boundary(b) {
            b -= 1;
        }
        text[..b].chars().count()
    };
    merged
        .into_iter()
        .map(|(s, e)| [to_char(s), to_char(e)])
        .collect()
}

/// Cuts a one-line snippet down to `width` characters for display, moving its
/// start so the first match sits at most `lead` characters from the left —
/// unless that would leave too little text after it. Cuts are marked with `…`.
/// Returns the new text and the match ranges shifted to fit it.
pub fn window(
    text: &str,
    matches: &[[usize; 2]],
    width: usize,
    lead: usize,
) -> (String, Vec<[usize; 2]>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= width || width < 10 {
        return (text.to_string(), matches.to_vec());
    }
    let first = matches.first().map_or(0, |m| m[0].min(n));

    let mut start = first.saturating_sub(lead);
    if start > 0 {
        // Start at a word if there's one between here and the match.
        if let Some(i) = chars[start..first].iter().position(|c| c.is_whitespace()) {
            start += i + 1;
        }
    }
    // Keep the window full: don't start so late that the text runs out.
    let latest = n - (width - 1);
    if start > latest {
        start = latest;
        if let Some(i) = chars[start..first.max(start)]
            .iter()
            .position(|c| c.is_whitespace())
            && start + i < first
        {
            start += i + 1;
        }
    }

    let prefix = usize::from(start > 0);
    let mut end = (start + width - prefix).min(n);
    let suffix = usize::from(end < n);
    if suffix == 1 {
        end -= 1;
        // End at a word boundary when one is close.
        if let Some(i) = chars[start..end].iter().rposition(|c| c.is_whitespace())
            && end - (start + i) <= 15
            && start + i > first
        {
            end = start + i;
        }
    }

    let mut out = String::new();
    if prefix == 1 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    let out_trimmed_end = out.trim_end().chars().count();
    let mut out: String = out.chars().take(out_trimmed_end).collect();
    if suffix == 1 {
        out.push('…');
    }

    let shifted = matches
        .iter()
        .filter_map(|m| {
            let s = m[0].max(start).min(end);
            let e = m[1].max(start).min(end);
            (s < e).then(|| [s - start + prefix, e - start + prefix])
        })
        .collect();
    (out, shifted)
}

/// Collapses all runs of whitespace (including newlines) into single spaces.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Formats a Unix timestamp (seconds, UTC) as `YYYY-MM-DD`.
pub fn format_date(timestamp: i64) -> String {
    let (y, m, d) = civil_from_days(timestamp.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats a Unix timestamp as `YYYY-MM-DD HH:MM UTC`.
pub fn format_datetime(timestamp: i64) -> String {
    let secs = timestamp.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        format_date(timestamp),
        secs / 3600,
        secs % 3600 / 60
    )
}

/// 1234567 → "1,234,567".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An import's files, for a sentence: `hn.csv`, or `a.wet.gz and 3 more`.
pub fn files_summary(files: &[String]) -> String {
    match files {
        [] => "this import".to_string(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    }
}

/// A data source's name for people: `hacker_news` → "Hacker News".
pub fn source_label(source: &str) -> String {
    match source {
        "lost" => "LOST".to_string(),
        "hacker_news" => "Hacker News".to_string(),
        "common_crawl" => "Common Crawl".to_string(),
        other => other
            .split('_')
            .filter(|w| !w.is_empty())
            .map(|w| {
                let mut chars = w.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// A size for people: "512 KB", "126.4 MB", "1.2 GB", "3.1 TB" (powers of 1,000).
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    let (value, unit) = if b >= 1e12 {
        (b / 1e12, "TB")
    } else if b >= 1e9 {
        (b / 1e9, "GB")
    } else if b >= 1e6 {
        (b / 1e6, "MB")
    } else {
        return format!("{} KB", bytes.div_ceil(1000));
    };
    if value >= 100.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

/// Parses a UTC timestamp like `2026-09-04T14:43:16Z` (as used in WARC files)
/// into Unix seconds. Fractional seconds are ignored.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next().flatten().unwrap_or(0));
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// (year, month, day) to days since 1970-01-01. Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_bytes(0), "0 KB");
        assert_eq!(format_bytes(1), "1 KB");
        assert_eq!(format_bytes(512_000), "512 KB");
        assert_eq!(format_bytes(126_400_000), "126 MB");
        assert_eq!(format_bytes(12_640_000), "12.6 MB");
        assert_eq!(format_bytes(1_234_000_000), "1.2 GB");
        assert_eq!(format_bytes(3_100_000_000_000), "3.1 TB");
    }

    #[test]
    fn hacker_news_markup() {
        let html = "That&#x27;s it: <a href=\"https:&#x2F;&#x2F;x.com&#x2F;a\" rel=\"nofollow\">https:&#x2F;&#x2F;x.com&#x2F;a</a><p>Second &quot;para&quot; &gt; 1 &#62; 0 &amp; <i>more</i>.<p><pre><code>  let x = 1;\n</code></pre>";
        assert_eq!(
            html_to_text(html),
            "That's it: https://x.com/a\n\nSecond \"para\" > 1 > 0 & more.\n\n  let x = 1;"
        );
    }

    #[test]
    fn web_pages() {
        let html = "<html><head><title>T</title><style>p { color: red }</style>\
            <script>var a = '<p>no</p>';</script></head><body><!-- hidden <p>no</p> -->\
            <h1>Heading</h1>Text<table><tr><td>a</td><td>b</td></tr></table>\
            <SCRIPT type=x>alert(1)</SCRIPT>end</body></html>";
        assert_eq!(html_to_text(html), "T\n\nHeading\n\nText\n\na b\n\nend");
        assert_eq!(html_to_text("a<!-- never closed"), "a");
        assert_eq!(html_to_text("a<script>never closed"), "a");
    }

    #[test]
    fn stray_characters_survive() {
        assert_eq!(html_to_text("a < b & c"), "a < b & c");
        assert_eq!(html_to_text("&unknown; &#xZZ;"), "&unknown; &#xZZ;");
    }

    #[test]
    fn ranges_in_characters() {
        let text = "Ünïcode catfish and catfish";
        let a = text.find("catfish").unwrap();
        let b = text.rfind("catfish").unwrap();
        assert_eq!(
            char_ranges(text, &[(b, b + 7), (a, a + 7), (a, a + 3)]),
            vec![[8, 15], [20, 27]]
        );
        assert_eq!(char_ranges("ab", &[(0, 1), (1, 2)]), vec![[0, 2]]);
        assert!(char_ranges("ab", &[(1, 9)]).is_empty());
    }

    #[test]
    fn snippet_windows() {
        let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh catfish iiii jjjj kkkk llll mmmm";
        let m = text.find("catfish").unwrap();
        let (out, ranges) = window(text, &[[m, m + 7]], 30, 10);
        assert!(out.starts_with('…'), "{out}");
        let pos = ranges[0][0];
        assert!(pos <= 11, "match at {pos} in {out}");
        assert_eq!(
            out.chars().skip(ranges[0][0]).take(7).collect::<String>(),
            "catfish"
        );
        assert!(out.chars().count() <= 30, "{out}");

        // A match near the end: keep the window full rather than starting at it.
        let m = text.find("mmmm").unwrap();
        let (out, ranges) = window(text, &[[m, m + 4]], 30, 10);
        assert!(
            out.chars().count() >= 25 && out.chars().count() <= 30,
            "{out}"
        );
        assert!(out.ends_with("mmmm"), "{out}");
        assert_eq!(
            out.chars().skip(ranges[0][0]).take(4).collect::<String>(),
            "mmmm"
        );

        // Short text is left alone.
        assert_eq!(window("short", &[[0, 5]], 30, 10).0, "short");

        // No match: start from the beginning.
        let (out, _) = window(text, &[], 30, 10);
        assert!(out.starts_with("aaaa") && out.ends_with('…'), "{out}");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_at_word("hello brave new world", 13), "hello brave");
        assert_eq!(truncate_at_word("short", 100), "short");
        assert_eq!(truncate_at_word("ééééé", 3), "é");
        assert_eq!(truncate_at_word("abcdefgh", 4), "abcd");
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_617_910_855), "2021-04-08");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_datetime(1_617_910_855), "2021-04-08 19:40 UTC");
        assert_eq!(parse_timestamp("2021-04-08T19:40:55Z"), Some(1_617_910_855));
        assert_eq!(parse_timestamp("1970-01-01T00:00:00.123Z"), Some(0));
        assert_eq!(parse_timestamp("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_timestamp("2021-04-08 19:40:55"), None);
        assert_eq!(parse_timestamp("2021-13-08T19:40:55Z"), None);
    }
}
