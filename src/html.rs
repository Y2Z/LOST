//! The web UI without JavaScript (ARCHITECTURE.md §10.3). The JavaScript
//! UI never asks the server for `/?q=…`: it changes the address itself. So
//! a real request for it — a form sent without JavaScript, Lynx, w3m, a
//! link opened in a new tab, a reload — gets a page of plain HTML and CSS,
//! no scripts: the search box with a Find button, a page of results with
//! "Looooost" links to the others, or a stored page (`cache:`).
//!
//! Everything from the database is HTML-escaped; the only markup added is
//! `<mark>` around matches, placed from the engine's character ranges.

use crate::engine::{CachedPage, Hit, SearchResults};
use crate::text::thousands;

/// Results per page: the same pages as the JavaScript UI's `&p=`.
pub const PAGE: usize = 20;
/// The furthest page a link can open on.
pub const MAX_PAGE: usize = 250;
/// Page links shown around the current one.
const PAGE_LINKS: usize = 10;

/// `text` with `&<>"'` escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `text` escaped, with `<mark>` around `ranges` (`[start, end)` in
/// characters, as the engine gives them).
pub fn marked(text: &str, ranges: &[[usize; 2]]) -> String {
    if ranges.is_empty() {
        return escape(text);
    }
    let chars: Vec<char> = text.chars().collect();
    let piece = |from: usize, to: usize| escape(&chars[from..to].iter().collect::<String>());
    let mut out = String::with_capacity(text.len() + ranges.len() * 13);
    let mut at = 0;
    for &[start, end] in ranges {
        if start < at || end <= start || start >= chars.len() {
            continue;
        }
        let end = end.min(chars.len());
        out.push_str(&piece(at, start));
        out.push_str("<mark>");
        out.push_str(&piece(start, end));
        out.push_str("</mark>");
        at = end;
    }
    out.push_str(&piece(at, chars.len()));
    out
}

/// A query-string value, percent-encoded.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The address of a search (page `page`), as the JavaScript UI writes it.
pub fn search_url(query: &str, page: usize) -> String {
    if page > 1 {
        format!("/?q={}&p={page}", encode(query))
    } else {
        format!("/?q={}", encode(query))
    }
}

/// The address of a stored page, with the words of `highlight` marked.
pub fn cache_url(url: &str, highlight: &str) -> String {
    let mut out = format!("/?q={}", encode(&format!("cache:{url}")));
    if !highlight.is_empty() {
        out.push_str("&hl=");
        out.push_str(&encode(highlight));
    }
    out
}

/// Only web links are clickable; anything else (javascript:, data:, …) isn't.
fn safe_href(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let ok = ["http://", "https://", "gemini://", "gopher://", "ftp://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme));
    if ok { escape(url) } else { "#".to_string() }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn result(hit: &Hit, query: &str) -> String {
    let title = match &hit.title {
        Some(t) => marked(t, &hit.title_matches),
        None => escape(&hit.url),
    };
    let href = safe_href(&hit.url);
    format!(
        "<article class=\"result\">\
         <a class=\"title\" href=\"{href}\" rel=\"noopener noreferrer\">{title}</a>\
         <p class=\"snippet\">{}</p>\
         <div class=\"where\">\
         <a class=\"url\" href=\"{href}\" rel=\"noopener noreferrer\">{}</a>\
         <a class=\"cached\" href=\"{}\">Cached</a>\
         </div></article>\n",
        marked(&hit.snippet, &hit.snippet_matches),
        escape(&hit.url),
        escape(&cache_url(&hit.url, query)),
    )
}

/// Links to the other pages, as Google's "Goooooogle": an "o" for each
/// page around this one, with its number under it, between the "L" (the
/// previous page) and the "st" (the next).
fn pager(query: &str, page: usize, total: usize) -> String {
    let last = total.div_ceil(PAGE).clamp(1, MAX_PAGE);
    if last == 1 {
        return String::new();
    }
    let first_link = page.saturating_sub(PAGE_LINKS / 2).max(1);
    let last_link = (first_link + PAGE_LINKS - 1).min(last);
    let first_link = last_link.saturating_sub(PAGE_LINKS - 1).max(1);
    let cell = |n: Option<usize>, letter: &str, label: &str, extra: &str| match n {
        Some(n) => format!(
            "<a href=\"{}\"{extra}><span class=\"letter\">{letter}</span><span class=\"label\">{label}</span></a>",
            escape(&search_url(query, n))
        ),
        None => format!(
            "<span{extra}><span class=\"letter\">{letter}</span><span class=\"label\">{label}</span></span>"
        ),
    };
    let mut out = String::from("<nav class=\"pager\" aria-label=\"Pages\">");
    out.push_str(&if page > 1 {
        cell(
            Some(page - 1),
            "L",
            "Previous",
            " class=\"end\" rel=\"prev\"",
        )
    } else {
        cell(None, "L", "", " class=\"end\"")
    });
    for n in first_link..=last_link {
        out.push_str(&if n == page {
            cell(
                None,
                "o",
                &n.to_string(),
                " class=\"current\" aria-current=\"page\"",
            )
        } else {
            cell(Some(n), "o", &n.to_string(), "")
        });
    }
    out.push_str(&if page < last {
        cell(Some(page + 1), "st", "Next", " class=\"end\" rel=\"next\"")
    } else {
        cell(None, "st", "", " class=\"end\"")
    });
    out.push_str("</nav>\n");
    out
}

/// A page of results (`page` counts from 1).
pub fn results(r: &SearchResults, page: usize) -> String {
    let mut out = String::new();
    let status = if r.total == 0 {
        format!("Found no results for “{}”", escape(&r.query))
    } else if r.results.is_empty() {
        format!(
            "No more results: “{}” has {} (page {} is past the end)",
            escape(&r.query),
            thousands(r.total as u64),
            page
        )
    } else {
        format!(
            "Found {} result{} ({} ms)",
            thousands(r.total as u64),
            plural(r.total as u64),
            r.took_ms
        )
    };
    out.push_str(&format!("<p class=\"status\">{status}</p>\n"));
    for hit in &r.results {
        out.push_str(&result(hit, &r.query));
    }
    out.push_str(&pager(&r.query, page, r.total));
    out
}

/// A stored page (`cache:URL`), or a note that it isn't stored.
pub fn stored(page: Option<&CachedPage>, url: &str) -> String {
    let Some(page) = page else {
        let host = crate::urls::Url::parse(&if url.contains("://") {
            url.to_string()
        } else {
            format!("https://{url}")
        })
        .map(|u| u.host);
        return match host {
            Some(host) => format!(
                "<p class=\"status\">This page isn't in the database. \
                 <a href=\"{}\">Search site:{}</a></p>\n",
                escape(&search_url(&format!("site:{host}"), 1)),
                escape(&host)
            ),
            None => "<p class=\"status\">This page isn't in the database.</p>\n".to_string(),
        };
    };
    let title = match &page.title {
        Some(t) => marked(t, &page.title_matches),
        None => escape(&page.url),
    };
    let captured = match &page.crawled {
        Some(date) => format!("<span>Captured {}</span>", escape(date)),
        None => String::new(),
    };
    // The empty status line keeps the same space above as the JavaScript view.
    format!(
        "<p class=\"status\"></p>\n<article class=\"stored\">\
         <div class=\"about\"><h1>{title}</h1>\
         <p class=\"meta\"><a class=\"url\" href=\"{}\" rel=\"noopener noreferrer\">{}</a>{captured}</p></div>\
         <div class=\"text\">{}</div>\
         </article>\n",
        safe_href(&page.url),
        escape(&page.url),
        marked(&page.text, &page.text_matches),
    )
}

/// A message in place of results (a bad query, an error).
pub fn message(text: &str) -> String {
    format!("<p class=\"status\">{}</p>\n", escape(text))
}

/// A whole page: the bar (logo, search box, Find) and `content` under it.
/// Plain HTML and the web UI's stylesheet; no scripts.
/// `page` (a whole HTML document) with its "Cached" links shown, or not:
/// they're always in the page, and the stylesheet hides them unless the
/// root element has the `cached-links` class.
pub fn with_cached_links(page: &str, show: bool) -> String {
    if show {
        page.replacen(
            "<html lang=\"en\">",
            "<html lang=\"en\" class=\"cached-links\">",
            1,
        )
    } else {
        page.to_string()
    }
}

pub fn document(query: &str, content: &str) -> String {
    let title = if query.is_empty() {
        "LOST".to_string()
    } else {
        format!("{} – LOST", escape(query))
    };
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>{title}</title>\n\
         <link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\">\n\
         <link rel=\"stylesheet\" href=\"/lost.css\">\n\
         </head>\n\
         <body class=\"basic\">\n\
         <header>\n\
         <a class=\"logo\" href=\"/\" aria-label=\"LOST — start over\">LOST</a>\n\
         <form id=\"search\" role=\"search\" action=\"/\" method=\"get\">\n\
         <div class=\"box\"><input id=\"q\" name=\"q\" type=\"search\" value=\"{}\" placeholder=\"Search offline\" aria-label=\"Search\" spellcheck=\"false\"></div>\n\
         <button class=\"find\" type=\"submit\">Find</button>\n\
         </form>\n\
         </header>\n\
         <main>\n{content}</main>\n\
         </body>\n\
         </html>\n",
        escape(query)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_links_follow_the_setting() {
        let page = "<!doctype html>\n<html lang=\"en\">\n<body></body></html>";
        assert_eq!(with_cached_links(page, false), page);
        assert!(
            with_cached_links(page, true).contains("<html lang=\"en\" class=\"cached-links\">")
        );
        assert!(with_cached_links(&document("x", ""), true).contains("class=\"cached-links\""));
    }

    fn hit(i: usize) -> Hit {
        Hit {
            url: format!("https://example.com/{i}?a=1&b=<2>"),
            title: Some("Fried <b>catfish</b>".into()),
            snippet: "Dredge the catfish & fry".into(),
            sources: vec!["common_crawl".into()],
            crawled: None,
            title_matches: vec![[9, 16]],
            snippet_matches: vec![[11, 18]],
        }
    }

    #[test]
    fn escaping_and_marks() {
        assert_eq!(
            escape("<a href='x'>&\"</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&quot;&lt;/a&gt;"
        );
        assert_eq!(
            marked("ünï catfish <x>", &[[4, 11]]),
            "ünï <mark>catfish</mark> &lt;x&gt;"
        );
        // Out of order, overlapping or past the end: skipped, never a panic.
        assert_eq!(
            marked("abc", &[[1, 2], [0, 1], [2, 9], [7, 9]]),
            "a<mark>b</mark><mark>c</mark>"
        );
        assert_eq!(
            encode("cache:https://x.com/a b"),
            "cache%3Ahttps%3A%2F%2Fx.com%2Fa%20b"
        );
        assert_eq!(search_url("site:x.com", 3), "/?q=site%3Ax.com&p=3");
        assert_eq!(
            cache_url("https://x.com/", "a b"),
            "/?q=cache%3Ahttps%3A%2F%2Fx.com%2F&hl=a%20b"
        );
    }

    #[test]
    fn results_are_escaped() {
        let r = SearchResults {
            query: "catfish".into(),
            offset: 0,
            count: 20,
            total: 45,
            took_ms: 3,
            results: (0..20).map(hit).collect(),
        };
        let html = results(&r, 1);
        assert!(html.contains("Found 45 results (3 ms)"));
        assert!(html.contains("Fried &lt;b&gt;<mark>catfish</mark>&lt;/b&gt;"));
        assert!(html.contains("Dredge the <mark>catfish</mark> &amp; fry"));
        assert!(html.contains("href=\"https://example.com/0?a=1&amp;b=&lt;2&gt;\""));
        assert!(!html.contains("<b>"));
        assert!(html.contains(
            "aria-current=\"page\"><span class=\"letter\">o</span><span class=\"label\">1</span>"
        ));
        assert!(html.contains("href=\"/?q=catfish&amp;p=3\""));
        assert!(!html.contains("Previous"));
        assert!(html.contains(
            "rel=\"next\"><span class=\"letter\">st</span><span class=\"label\">Next</span>"
        ));

        let mut evil = hit(1);
        evil.url = "javascript:alert(1)".into();
        assert!(result(&evil, "x").contains("href=\"#\""));
    }

    #[test]
    fn pager_window() {
        let p = pager("q", 50, 5000);
        assert!(p.contains(">Previous<") && p.contains(">Next<"));
        assert_eq!(p.matches("class=\"letter\">o<").count(), 10);
        assert!(
            p.contains(">45<") && p.contains(">54<") && !p.contains(">44<") && !p.contains(">55<")
        );
        let p = pager("q", 250, 1_000_000);
        assert!(!p.contains(">Next<") && p.contains(">241<"));
        assert_eq!(pager("q", 1, 20), "");
    }

    #[test]
    fn stored_pages() {
        let page = CachedPage {
            url: "https://example.com/a".into(),
            title: None,
            sources: vec!["hacker_news".into()],
            crawled: Some("2024-03-02".into()),
            text: "Line one <script>\n\nLine two".into(),
            title_matches: vec![],
            text_matches: vec![[5, 8]],
        };
        let html = stored(Some(&page), &page.url);
        assert!(html.contains("<h1>https://example.com/a</h1>"));
        assert!(html.contains("<span>Captured 2024-03-02</span>"));
        // Where pages come from stays out of the page view.
        assert!(!html.contains("Hacker News"));
        assert!(html.contains("Line <mark>one</mark> &lt;script&gt;\n\nLine two"));
        let missing = stored(None, "example.org/x");
        assert!(
            missing.contains("Search site:example.org")
                && missing.contains("/?q=site%3Aexample.org")
        );
    }

    #[test]
    fn whole_pages_have_no_scripts() {
        let html = document("a \"b\" <c>", "<p>x</p>\n");
        assert!(html.contains("<title>a &quot;b&quot; &lt;c&gt; – LOST</title>"));
        assert!(html.contains("value=\"a &quot;b&quot; &lt;c&gt;\""));
        assert!(html.contains("<button class=\"find\" type=\"submit\">Find</button>"));
        assert!(html.contains("<main>\n<p>x</p>\n</main>"));
        assert!(!html.contains("<script"));
        assert!(document("", "").contains("<title>LOST</title>"));
    }
}
