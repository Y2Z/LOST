//! HTTP/1.1, as much as the spider needs, for `.onion` and I2P sites: GET,
//! straight or through a proxy (SOCKS5: a tunnel; HTTP: the whole URL sent
//! to the proxy), chunked or not, gzipped or not. And HTML read as text,
//! title and links.

use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use lost::urls::{Url, resolve_any};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};

use crate::crawl::{Link, Parsed, Response, Route, mentions, port};
use crate::net::{self, Proxy};

/// Longer answers are cut here: LOST's limit for a page's text.
const MAX_BODY: usize = lost::MAX_BODY_BYTES;
/// An error page is read this far (enough to tell a proxy's from a site's).
const MAX_ERROR_BODY: usize = 64 * 1024;
const MAX_HEADERS: usize = 64 * 1024;

pub struct Http {
    proxy: Option<Proxy>,
    timeout: Duration,
    /// Set up the first time an `https://` site is asked.
    tls: OnceLock<Result<Arc<ClientConfig>, String>>,
}

/// An answer, as it came.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    /// Names lowercase.
    pub headers: Vec<(String, String)>,
    /// Read for text, and for errors (the start of it); empty otherwise.
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The type, `text/html` when the server doesn't say (old sites).
    pub fn content_type(&self) -> String {
        self.header("content-type")
            .unwrap_or("text/html")
            .to_string()
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

impl Http {
    pub fn new(proxy: Option<Proxy>, timeout: Duration) -> Result<Http, String> {
        Ok(Http {
            proxy,
            timeout,
            tls: OnceLock::new(),
        })
    }

    pub fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    pub fn get(&self, route: Route, url: &Url) -> Result<Answer, String> {
        self.request(route, url, None, false)
    }

    /// GET, the answer's body read whatever its type (an API's JSON, say).
    pub fn get_data(&self, route: Route, url: &Url) -> Result<Answer, String> {
        self.request(route, url, None, true)
    }

    /// POSTs `body` (of type `content_type`); the answer's body is read
    /// whatever its type (an API's JSON, say).
    pub fn post(
        &self,
        route: Route,
        url: &Url,
        content_type: &str,
        body: &[u8],
    ) -> Result<Answer, String> {
        self.request(route, url, Some((content_type, body)), true)
    }

    fn request(
        &self,
        route: Route,
        url: &Url,
        upload: Option<(&str, &[u8])>,
        any_body: bool,
    ) -> Result<Answer, String> {
        let https = url.scheme == "https";
        let via_http_proxy = route == Route::Proxied && matches!(self.proxy, Some(Proxy::Http(_)));
        if https && via_http_proxy {
            return Err("https:// can't go through an HTTP proxy".into());
        }
        let host = url.host.trim_start_matches('[').trim_end_matches(']');
        let tcp = net::connect(route, self.proxy.as_ref(), host, port(url), self.timeout)?;
        let mut stream: Box<dyn Stream> = if https {
            let name = ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
            let tls = self.tls.get_or_init(net::any_certificate).clone()?;
            let connection = ClientConnection::new(tls, name).map_err(|e| e.to_string())?;
            Box::new(StreamOwned::new(connection, tcp))
        } else {
            Box::new(tcp)
        };

        // Through an HTTP proxy the request names the whole URL; otherwise
        // the path.
        let target = if via_http_proxy {
            url.to_string()
        } else {
            match &url.query {
                Some(query) => format!("{}?{query}", url.path),
                None => url.path.clone(),
            }
        };
        let host_header = match url.port {
            Some(port) => format!("{}:{port}", url.host),
            None => url.host.clone(),
        };
        let (method, extra) = match upload {
            Some((content_type, body)) => (
                "POST",
                format!(
                    "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                    body.len()
                ),
            ),
            None => ("GET", String::new()),
        };
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: lost-spider/{}\r\n\
             Accept: text/html, text/plain;q=0.9, */*;q=0.5\r\nAccept-Encoding: gzip\r\n\
             {extra}Connection: close\r\n\r\n",
            env!("CARGO_PKG_VERSION")
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.write_all(upload.map_or(&[][..], |(_, body)| body)))
            .and_then(|()| stream.flush())
            .map_err(|e| e.to_string())?;

        let started = Instant::now();
        let total = self.timeout * 4;
        let mut data = Vec::with_capacity(32 * 1024);
        let mut buffer = [0u8; 16 * 1024];
        let head_end = loop {
            if let Some(i) = find(&data, b"\r\n\r\n") {
                break i;
            }
            if data.len() > MAX_HEADERS {
                return Err("an answer that makes no sense".into());
            }
            match read(&mut stream, &mut buffer, started, total)? {
                0 => return Err("closed without an answer".into()),
                n => data.extend_from_slice(&buffer[..n]),
            }
        };
        let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.strip_prefix("HTTP/"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or("an answer that isn't HTTP")?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let mut answer = Answer {
            status,
            headers,
            body: Vec::new(),
        };

        let wanted = any_body || is_text(&answer.content_type());
        let limit = if (200..300).contains(&status) && wanted {
            MAX_BODY
        } else if status >= 400 {
            MAX_ERROR_BODY
        } else {
            return Ok(answer);
        };
        let mut raw = data.split_off(head_end + 4);
        while raw.len() < limit {
            match read(&mut stream, &mut buffer, started, total) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buffer[..n]),
                // An error page cut short is still an error page.
                Err(_) if status >= 400 => break,
                Err(e) => return Err(e),
            }
        }
        if answer
            .header("transfer-encoding")
            .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
        {
            raw = dechunk(&raw);
        }
        if answer
            .header("content-encoding")
            .is_some_and(|e| e.eq_ignore_ascii_case("gzip") || e.eq_ignore_ascii_case("x-gzip"))
        {
            let mut unpacked = Vec::new();
            let _ = flate2::read::MultiGzDecoder::new(raw.as_slice())
                .take(limit as u64)
                .read_to_end(&mut unpacked);
            raw = unpacked;
        }
        raw.truncate(limit);
        answer.body = raw;
        Ok(answer)
    }
}

/// What an answer means to the crawl.
pub fn response(answer: Answer) -> Response {
    let retry_after = || {
        answer
            .header("retry-after")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|s| Duration::from_secs(s.clamp(1, 3600)))
    };
    match answer.status {
        200..=299 => {
            let mime = answer.content_type();
            let body = is_text(&mime).then_some(answer.body);
            Response::Document { mime, body }
        }
        301 | 302 | 303 | 307 | 308 => match answer.header("location") {
            Some(location) => Response::Redirect(location.to_string()),
            None => Response::Gone,
        },
        429 => Response::SlowDown(retry_after().unwrap_or(Duration::from_secs(60))),
        503 => match retry_after() {
            Some(wait) => Response::SlowDown(wait),
            None => Response::TryLater,
        },
        500..=599 => Response::TryLater,
        _ => Response::Gone,
    }
}

/// A text document as a page: HTML read for its title, text and links;
/// anything else as text, with the URLs written in it as links.
pub fn parse(url: &Url, mime: &str, body: &[u8]) -> Parsed {
    let kind = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let text = decode(body, mime);
    if kind == "text/html" || kind == "application/xhtml+xml" {
        let body = lost::text::html_to_text(&text);
        // Links, and URLs written in the text that aren't links (lists of
        // sites often are just that).
        let mut links = html_links(url, &text);
        for link in mentions(&body, "http")
            .into_iter()
            .chain(mentions(&body, "https"))
        {
            if !links.iter().any(|l| l.url == link.url) {
                links.push(link);
            }
        }
        Parsed {
            title: html_title(&text),
            body,
            links,
            ..Parsed::default()
        }
    } else {
        let mut links = mentions(&text, "http");
        links.extend(mentions(&text, "https"));
        Parsed {
            title: None,
            body: text,
            links,
            ..Parsed::default()
        }
    }
}

fn is_text(mime: &str) -> bool {
    let kind = mime.trim().to_ascii_lowercase();
    kind.starts_with("text/") || kind.starts_with("application/xhtml+xml")
}

/// UTF-8, unless the type or (for HTML) a `<meta>` near the start says
/// Latin-1 or Windows-1252; anything else is read as UTF-8.
fn decode(body: &[u8], mime: &str) -> String {
    let start = String::from_utf8_lossy(&body[..body.len().min(2048)]).to_ascii_lowercase();
    let says = |name: &str| {
        mime.to_ascii_lowercase()
            .contains(&format!("charset={name}"))
            || start.contains(&format!("charset={name}"))
            || start.contains(&format!("charset=\"{name}"))
    };
    let latin1 = ["iso-8859-1", "latin1", "latin-1", "windows-1252", "cp1252"]
        .iter()
        .any(|n| says(n));
    if latin1 && std::str::from_utf8(body).is_err() {
        windows_1252(body)
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

/// `<title>…</title>`, on one line.
pub fn html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let start = start + lower[start..].find('>')? + 1;
    let end = start + lower[start..].find("</title")?;
    let title = lost::text::single_line(&lost::text::html_to_text(&html[start..end]));
    (!title.is_empty()).then_some(title)
}

/// The links of `<a href>`, `<area href>`, `<frame src>` and `<iframe src>`,
/// resolved against the page, each with its text.
pub fn html_links(base: &Url, html: &str) -> Vec<Link> {
    // ASCII lowercasing keeps every byte where it was.
    let lower = html.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut at = 0;
    while let Some(offset) = lower[at..].find('<') {
        let start = at + offset;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let end = start + len;
        at = end + 1;
        let tag = &lower[start + 1..end];
        let name = tag
            .split(|c: char| c.is_ascii_whitespace())
            .next()
            .unwrap_or("");
        let attribute = match name {
            "a" | "area" => "href",
            "frame" | "iframe" => "src",
            _ => continue,
        };
        let Some(value) = attribute_value(&html[start + 1..end], tag, attribute) else {
            continue;
        };
        let target = value.replace("&amp;", "&").replace("&#38;", "&");
        let Some(url) = resolve_any(base, &target) else {
            continue;
        };
        let label = (name == "a")
            .then(|| {
                let close = lower[at..].find("</a").map_or(at, |i| at + i);
                let text = lost::text::single_line(&lost::text::html_to_text(&html[at..close]));
                lost::text::truncate_at_word(&text, 200).to_string()
            })
            .filter(|text| !text.is_empty());
        links.push(Link {
            url: url.to_string(),
            label,
        });
    }
    links
}

/// `name="value"`, `name='value'` or `name=value` in a tag (`tag` is the
/// same text lowercased, to find the name in).
fn attribute_value<'a>(original: &'a str, tag: &str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    loop {
        let i = from + tag[from..].find(name)?;
        from = i + name.len();
        let before_ok = i == 0 || tag.as_bytes()[i - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let value_start = original.len() - rest.len() + 1;
        let value = original[value_start..].trim_start();
        return Some(match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inner = &value[1..];
                &inner[..inner.find(quote).unwrap_or(inner.len())]
            }
            _ => {
                let end = value
                    .find(|c: char| c.is_ascii_whitespace() || c == '>')
                    .unwrap_or(value.len());
                &value[..end]
            }
        });
    }
}

/// A chunked body (RFC 9112 §7.1) put back together; a cut-short one as far
/// as it goes.
fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        let Some(line_len) = find(&raw[at..], b"\r\n") else {
            break;
        };
        let line = String::from_utf8_lossy(&raw[at..at + line_len]);
        let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
        else {
            break;
        };
        at += line_len + 2;
        if size == 0 {
            break;
        }
        // A size larger than what's left (or one that overflows) means a
        // cut-off body: keep what there is and stop.
        let end = at.saturating_add(size).min(raw.len());
        out.extend_from_slice(&raw[at..end]);
        at = end.saturating_add(2);
    }
    out
}

/// Bytes in Windows-1252, which is also what every browser reads when a
/// page says ISO-8859-1: 0x80–0x9F are curly quotes, dashes, € and so on
/// rather than control characters. The five unassigned bytes stay as the
/// code points of the same number.
pub fn windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9f => HIGH[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read(
    stream: &mut dyn Stream,
    buffer: &mut [u8],
    started: Instant,
    total: Duration,
) -> Result<usize, String> {
    if started.elapsed() > total {
        return Err("too slow".into());
    }
    match stream.read(buffer) {
        Ok(n) => Ok(n),
        // Servers that close without TLS's goodbye.
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(0),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn chunks() {
        assert_eq!(dechunk(b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n"), b"abcde");
        // Cut off in the middle of a chunk.
        assert_eq!(dechunk(b"10\r\nabc"), b"abc");
        // A size that would overflow keeps what's there and stops.
        assert_eq!(dechunk(b"ffffffffffffffff\r\nabc"), b"abc");
        assert_eq!(dechunk(b"2\r\nab\r\nffffffffffffffff\r\ncd"), b"abcd");
        assert_eq!(dechunk(b"nonsense"), b"");
    }

    #[test]
    fn windows_1252_text() {
        assert_eq!(
            windows_1252(b"\x93caf\xe9\x94 \x96 \x80 5 \x85\x81"),
            "\u{201c}café\u{201d} \u{2013} € 5 …\u{81}"
        );
        assert_eq!(
            decode(b"<meta charset=windows-1252>\x92", "text/html"),
            "<meta charset=windows-1252>\u{2019}"
        );
    }

    /// A server answering one request with `answer`; returns what was asked.
    fn serve(answer: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            stream.write_all(&answer).unwrap();
            request
        });
        (addr, handle)
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn posting() {
        // A server that reads the whole request, body too, as servers do.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let length: usize = request
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"data\":1}")
                .unwrap();
            (request, body)
        });
        let http = Http::new(None, Duration::from_secs(5)).unwrap();
        let answer = http
            .post(
                Route::Direct(addr),
                &url("http://gateway.example/graphql"),
                "application/json",
                b"{\"query\":\"x\"}",
            )
            .unwrap();
        // JSON isn't text, but what was asked for is read all the same.
        assert_eq!(answer.body, b"{\"data\":1}");
        let (request, body) = server.join().unwrap();
        assert!(
            request.starts_with("POST /graphql HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request.contains("Content-Type: application/json\r\n"),
            "{request}"
        );
        assert_eq!(body, b"{\"query\":\"x\"}");
    }

    #[test]
    fn plain_chunked_and_through_a_proxy() {
        let http = Http::new(None, Duration::from_secs(5)).unwrap();
        let (addr, server) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
              Transfer-Encoding: chunked\r\n\r\n5\r\n<p>He\r\n6;x=y\r\nllo</p\r\n1\r\n>\r\n0\r\n\r\n"
                .to_vec(),
        );
        let answer = http
            .get(Route::Direct(addr), &url("http://site.onion/a?b=c"))
            .unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, b"<p>Hello</p>");
        let request = server.join().unwrap();
        assert!(
            request.starts_with("GET /a?b=c HTTP/1.1\r\nHost: site.onion\r\n"),
            "{request}"
        );

        // Through an HTTP proxy: the whole URL, and nothing looked up.
        let (addr, server) = serve(b"HTTP/1.1 301 Moved\r\nLocation: /new\r\n\r\n".to_vec());
        let http = Http::new(Some(Proxy::Http(addr.to_string())), Duration::from_secs(5)).unwrap();
        let answer = http
            .get(Route::Proxied, &url("http://site.i2p/old"))
            .unwrap();
        assert_eq!(response(answer), Response::Redirect("/new".into()));
        assert!(
            server
                .join()
                .unwrap()
                .starts_with("GET http://site.i2p/old HTTP/1.1\r\n")
        );

        // Not text: the body isn't read.
        let (addr, server) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\n\r\n\x89PNG"
                .to_vec(),
        );
        let http = Http::new(None, Duration::from_secs(5)).unwrap();
        let answer = http
            .get(Route::Direct(addr), &url("http://site.onion/cat"))
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            response(answer),
            Response::Document {
                mime: "image/png".into(),
                body: None
            }
        );
    }

    #[test]
    fn statuses() {
        let answer = |status: u16, headers: &[(&str, &str)]| Answer {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: b"hi".to_vec(),
        };
        assert_eq!(
            response(answer(200, &[("content-type", "text/plain")])),
            Response::Document {
                mime: "text/plain".into(),
                body: Some(b"hi".to_vec())
            }
        );
        assert_eq!(
            response(answer(429, &[("retry-after", "120")])),
            Response::SlowDown(Duration::from_secs(120))
        );
        assert_eq!(response(answer(503, &[])), Response::TryLater);
        assert_eq!(response(answer(404, &[])), Response::Gone);
        assert_eq!(response(answer(302, &[])), Response::Gone);
    }

    #[test]
    fn html_pages() {
        let base = url("http://site.onion/dir/page.html");
        let html = "<html><head><title>My &amp; site</title></head><body>\
            <A HREF=\"other.html\">Other <b>page</b></a>\
            <a class=x href='/x?a=1&amp;b=2'>X</a>\
            <a href=http://friend.onion/ >Friend</a>\
            <a name=top>no link</a><a href=\"mailto:me@x\">mail</a>\
            <iframe src=\"frame.html\"></iframe><p>Hello http://listed.onion/<br>\
            http://friend.onion/</p></body></html>";
        let page = parse(&base, "text/html", html.as_bytes());
        assert_eq!(page.title.as_deref(), Some("My & site"));
        assert!(page.body.contains("Hello"));
        let links: Vec<(&str, Option<&str>)> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.label.as_deref()))
            .collect();
        assert_eq!(
            links,
            [
                ("http://site.onion/dir/other.html", Some("Other page")),
                ("http://site.onion/x?a=1&b=2", Some("X")),
                ("http://friend.onion/", Some("Friend")),
                ("http://site.onion/dir/frame.html", None),
                ("http://listed.onion/", None),
            ]
        );
        let text = parse(
            &base,
            "text/plain",
            b"see http://a.onion/ and https://b.onion/x.",
        );
        assert_eq!(text.links.len(), 2);
        assert_eq!(
            decode(&[0x63, 0x61, 0x66, 0xe9], "text/html; charset=ISO-8859-1"),
            "café"
        );
    }
}
