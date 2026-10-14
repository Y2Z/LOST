//! Gopher (RFC 1436; its URLs: RFC 4266): plain TCP, port 70. A request is
//! a selector and CRLF; the answer is the item itself, until the server
//! closes the connection. A menu is lines of `TYPE display TAB selector TAB
//! host TAB port`; a URL's path is `/TYPE` and the selector, percent-encoded.

use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use lost::urls::Url;

use crate::crawl::{
    Link, Parsed, Protocol, Response, Route, looks_like_posting, mentions, percent_decode, port,
};
use crate::net::{self, Proxy};

/// Where a crawl starts, besides `--link`: Floodgap (home of Veronica-2,
/// Gopherspace's search engine, and its list of servers), and some of the
/// oldest and busiest public servers.
const SEEDS: &[&str] = &[
    "gopher://gopher.floodgap.com/",
    "gopher://gopher.floodgap.com/1/world",
    "gopher://sdf.org/",
    "gopher://bitreich.org/",
    "gopher://tilde.team/",
    "gopher://gopher.quux.org/",
];

/// Item types read as text: text files, menus, search results, HTML.
const TEXT_TYPES: &[char] = &['0', '1', '7', 'h'];
/// Item types saved by their link alone: BinHex, DOS files, uuencoded,
/// binaries, GIFs, images, PNGs, sounds, video, documents, PDFs.
const FILE_TYPES: &[char] = &['4', '5', '6', '9', 'g', 'I', 'p', 's', ';', '<', 'd', 'P'];

/// How long to wait to connect, or for the next bit of an answer
/// (`--timeout`); a whole answer gets four times that.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Through a proxy (Tor adds a few seconds a hop).
pub const PROXIED_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer answers are cut here: LOST's limit for a page's text.
const MAX_BODY: usize = lost::MAX_BODY_BYTES;

pub struct Gopher {
    /// A SOCKS5 proxy (Gopher can't go through an HTTP one).
    proxy: Option<Proxy>,
    timeout: Duration,
}

impl Gopher {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Gopher> {
        if let Some(Proxy::Http(_)) = proxy {
            return Err("Gopher can only go through a SOCKS5 proxy (socks5://HOST:PORT)".into());
        }
        let timeout = timeout.unwrap_or(match proxy {
            Some(_) => PROXIED_TIMEOUT,
            None => TIMEOUT,
        });
        Ok(Gopher { proxy, timeout })
    }
}

impl Protocol for Gopher {
    const NAME: &'static str = "gopher";
    const SCHEME: &'static str = "gopher";
    const SPACE: &'static str = "Gopherspace";
    const HOSTS: &'static str = "gopherholes";
    /// Veronica-2's convention: a text file with the selector `robots.txt`.
    const ROBOTS: &'static str = "/0robots.txt";

    fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        let (kind, selector) = item(url);
        let search = url.query.as_deref().map(percent_decode);
        let request = match kind {
            // A search box: what to search for comes in the query.
            '7' => match &search {
                Some(terms) => format!("{selector}\t{terms}"),
                None if selector.contains('\t') => selector,
                None => {
                    return Ok(Response::Input {
                        prompt: String::new(),
                        sensitive: false,
                    });
                }
            },
            '0' | '1' | 'h' => match &search {
                Some(rest) => format!("{selector}?{rest}"),
                None => selector,
            },
            // A file (a --link to one, say): saved by its link.
            _ => {
                return Ok(Response::Document {
                    mime: "application/octet-stream".into(),
                    body: None,
                });
            }
        };

        let mut stream = net::connect(
            route,
            self.proxy.as_ref(),
            &url.host,
            port(url),
            self.timeout,
        )?;
        stream
            .write_all(format!("{request}\r\n").as_bytes())
            .map_err(|e| e.to_string())?;

        let started = Instant::now();
        let mut body = Vec::new();
        let mut buffer = [0u8; 16 * 1024];
        while body.len() < MAX_BODY {
            if started.elapsed() > self.timeout * 4 {
                return Err("too slow".into());
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buffer[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        body.truncate(MAX_BODY);
        if body.is_empty() || is_error(&body) {
            return Ok(Response::Gone);
        }
        let mime = match kind {
            '1' | '7' => "application/gopher-menu",
            'h' => "text/html",
            _ => "text/plain",
        };
        Ok(Response::Document {
            mime: mime.into(),
            body: Some(body),
        })
    }

    fn parse(&self, _url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let text = decode(body);
        match mime {
            "application/gopher-menu" => menu(&text),
            "text/html" => Parsed {
                title: crate::http::html_title(&text),
                body: lost::text::html_to_text(&text),
                links: mentions(&text, Self::SCHEME),
                skip: false,
            },
            _ => {
                // A text file ends with a line holding a dot.
                let text = text.replace("\r\n", "\n");
                let text = text.trim_end();
                let text = text.strip_suffix("\n.").unwrap_or(text).trim_end();
                Parsed {
                    title: None,
                    links: mentions(text, Self::SCHEME),
                    body: text.to_string(),
                    skip: false,
                }
            }
        }
    }

    /// Rules are about selectors: `/` and the selector, whether or not it
    /// starts with one.
    fn robots_path(url: &Url) -> String {
        let (_, selector) = item(url);
        format!("/{}", selector.trim_start_matches('/'))
    }

    fn is_file(url: &Url) -> bool {
        let (kind, _) = item(url);
        FILE_TYPES.contains(&kind)
    }

    /// The selector's last part, or the host for a server's main menu.
    fn fallback_title(url: &Url) -> String {
        let (_, selector) = item(url);
        match selector.rsplit('/').find(|s| !s.trim().is_empty()) {
            Some(name) => name.trim().to_string(),
            None => url.host.clone(),
        }
    }

    /// A type 7 item is a search box by definition; only one that says it's
    /// for writing something is left alone.
    fn is_search(prompt: &str, label: Option<&str>, url: &Url) -> bool {
        let (_, selector) = item(url);
        !looks_like_posting(&format!("{prompt} {} {selector}", label.unwrap_or("")))
    }
}

/// A URL's item type and selector: `/1/phlog` → (`1`, `/phlog`); a bare
/// host is its main menu.
fn item(url: &Url) -> (char, String) {
    let path = url.path.strip_prefix('/').unwrap_or(&url.path);
    let mut chars = path.chars();
    match chars.next() {
        Some(kind) => (kind, percent_decode(chars.as_str())),
        None => ('1', String::new()),
    }
}

/// A menu item's URL.
fn item_url(kind: char, selector: &str, host: &str, port: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || host.contains(['/', ' ', '\t']) {
        return None;
    }
    let port = match port.trim() {
        "" | "70" => String::new(),
        p if p.parse::<u16>().is_ok() => format!(":{p}"),
        _ => return None,
    };
    let mut url = format!("gopher://{host}{port}/{kind}");
    for b in selector.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    Some(url)
}

/// A menu as a page: every line's display text (info lines too), and a
/// link for each item there's something to save from. `h` items with a
/// `URL:` selector are links to the web: kept as links, not crawled.
fn menu(text: &str) -> Parsed {
    let mut lines: Vec<&str> = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line == "." {
            break;
        }
        let mut chars = line.chars();
        let Some(kind) = chars.next() else { continue };
        let mut fields = chars.as_str().split('\t');
        let display = fields.next().unwrap_or("");
        let selector = fields.next().unwrap_or("");
        let host = fields.next().unwrap_or("");
        let port = fields.next().unwrap_or("70");
        let label = (!display.trim().is_empty()).then(|| display.trim().to_string());
        lines.push(display.trim_end());
        match kind {
            'i' | '3' => links.extend(mentions(display, Gopher::SCHEME)),
            'h' if selector.starts_with("URL:") || selector.starts_with("/URL:") => {
                let target = selector.trim_start_matches('/').trim_start_matches("URL:");
                if Url::parse(target).is_some() {
                    links.push(Link {
                        url: target.to_string(),
                        label,
                    });
                }
            }
            kind if TEXT_TYPES.contains(&kind) || FILE_TYPES.contains(&kind) => {
                if let Some(url) = item_url(kind, selector, host, port) {
                    links.push(Link { url, label });
                }
            }
            // Telnet, CSO phone books, mirrors, and types no one agrees on.
            _ => {}
        }
    }
    Parsed {
        title: None,
        body: lines.join("\n").trim().to_string(),
        links,
        skip: false,
    }
}

/// A server's "not found" (or other error) instead of the item: a menu
/// whose first line is an error item.
fn is_error(body: &[u8]) -> bool {
    let first = body.split(|&b| b == b'\n').next().unwrap_or(&[]);
    first.first() == Some(&b'3') && first.contains(&b'\t')
}

/// UTF-8, or Latin-1 when it isn't: much of Gopherspace predates UTF-8.
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn gopher() -> Gopher {
        Gopher::new(None, None).unwrap()
    }

    #[test]
    fn items() {
        assert_eq!(item(&url("gopher://x/")), ('1', String::new()));
        assert_eq!(item(&url("gopher://x")), ('1', String::new()));
        assert_eq!(
            item(&url("gopher://x/0/notes/a%20b.txt")),
            ('0', "/notes/a b.txt".into())
        );
        assert_eq!(item(&url("gopher://x/7/v2/vs")), ('7', "/v2/vs".into()));
        assert_eq!(
            item_url('0', "/a b?c.txt", "Example.org", "70").as_deref(),
            Some("gopher://Example.org/0/a%20b%3Fc.txt")
        );
        assert_eq!(
            item_url('1', "", "x.org", "7070").as_deref(),
            Some("gopher://x.org:7070/1")
        );
        assert_eq!(item_url('1', "", "", "70"), None);
        // Round trip: the selector sent is the one in the menu.
        let u = url(&item_url('0', "/a b?c.txt", "x.org", "70").unwrap());
        assert_eq!(item(&u).1, "/a b?c.txt");
        assert_eq!(Gopher::robots_path(&u), "/a b?c.txt");
        assert_eq!(Gopher::robots_path(&url("gopher://x/1")), "/");
        assert_eq!(
            Gopher::robots_path(&url("gopher://x/0robots.txt")),
            "/robots.txt"
        );
        assert!(Gopher::is_file(&url("gopher://x/I/cat.jpg")));
        assert!(!Gopher::is_file(&url("gopher://x/0/cat.txt")));
        assert_eq!(Gopher::fallback_title(&url("gopher://x/1/phlog/")), "phlog");
        assert_eq!(Gopher::fallback_title(&url("gopher://x/1")), "x");
    }

    #[test]
    fn menus() {
        let text = "\
iWelcome to my hole\tfake\t(NULL)\t0\r
i  see gopher://friend.org/1/ too\t\terror.host\t1\r
1Phlog\t/phlog\texample.org\t70\r
0About me\tabout.txt\texample.org\t7070\r
7Search\t/search\texample.org\t70\r
IA picture\t/cat.png\texample.org\t70\r
hMy website\tURL:https://example.com/\texample.org\t70\r
8Telnet\t\tbbs.example.org\t23\r
3Oops\t\terror.host\t1\r
.\r
1After the end\t/x\texample.org\t70\r
";
        let page = menu(text);
        assert_eq!(page.title, None);
        assert_eq!(
            page.body,
            "Welcome to my hole\n  see gopher://friend.org/1/ too\nPhlog\nAbout me\nSearch\nA picture\nMy website\nTelnet\nOops"
        );
        let links: Vec<(&str, Option<&str>)> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.label.as_deref()))
            .collect();
        assert_eq!(
            links,
            [
                ("gopher://friend.org/1/", None),
                ("gopher://example.org/1/phlog", Some("Phlog")),
                ("gopher://example.org:7070/0about.txt", Some("About me")),
                ("gopher://example.org/7/search", Some("Search")),
                ("gopher://example.org/I/cat.png", Some("A picture")),
                ("https://example.com/", Some("My website")),
            ]
        );
    }

    #[test]
    fn asking_a_server() {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for stream in listener.incoming().take(3) {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let selector = line.trim_end_matches(['\r', '\n']).to_string();
                let answer = match selector.as_str() {
                    "" => "iHello\tfake\t(NULL)\t0\r\n0Notes\t/notes.txt\tlocalhost\t70\r\n.\r\n",
                    "/search\tcats dogs" => "0A cat\t/cat.txt\tlocalhost\t70\r\n.\r\n",
                    _ => "3Not found\t\terror.host\t1\r\n.\r\n",
                };
                stream.write_all(answer.as_bytes()).unwrap();
                asked.push(selector);
            }
            asked
        });
        let fetch = |u: &str| gopher().fetch(Route::Direct(addr), &url(u)).unwrap();
        match fetch("gopher://localhost/") {
            Response::Document {
                mime,
                body: Some(body),
            } => {
                assert_eq!(mime, "application/gopher-menu");
                let page = gopher().parse(&url("gopher://localhost/"), &mime, &body);
                assert_eq!(page.links[0].url, "gopher://localhost/0/notes.txt");
            }
            other => panic!("{other:?}"),
        }
        // A search box with nothing to search for isn't asked.
        assert_eq!(
            fetch("gopher://localhost/7/search"),
            Response::Input {
                prompt: String::new(),
                sensitive: false
            }
        );
        assert!(matches!(
            fetch("gopher://localhost/7/search?cats%20dogs"),
            Response::Document { .. }
        ));
        assert_eq!(fetch("gopher://localhost/0/nope"), Response::Gone);
        // A file isn't asked for either.
        assert!(matches!(
            fetch("gopher://localhost/9/x.zip"),
            Response::Document { body: None, .. }
        ));
        assert_eq!(server.join().unwrap(), ["", "/search\tcats dogs", "/nope"]);
    }

    #[test]
    fn texts_and_errors() {
        let page = gopher().parse(
            &url("gopher://x/0/a.txt"),
            "text/plain",
            b"Hello\r\nworld\r\n.\r\n",
        );
        assert_eq!(page.body, "Hello\nworld");
        assert!(is_error(
            b"3'/nope' doesn't exist!\t\terror.host\t1\r\n.\r\n"
        ));
        assert!(!is_error(b"3 little pigs\r\n"));
        assert_eq!(decode(&[0x63, 0x61, 0x66, 0xe9]), "café");
        let page = gopher().parse(
            &url("gopher://x/h/a.html"),
            "text/html",
            b"<html><head><TITLE>My  page</TITLE></head><body><p>Hi</p></body></html>",
        );
        assert_eq!(page.title.as_deref(), Some("My page"));
        assert!(Gopher::is_search(
            "",
            Some("Search Veronica-2"),
            &url("gopher://x/7/v2/vs")
        ));
        assert!(!Gopher::is_search(
            "",
            Some("Sign my guestbook"),
            &url("gopher://x/7/gb")
        ));
    }
}
