//! Spartan (spartan://mozz.us/specification.gmi): Gemini's sibling without
//! TLS, plain TCP on port 300. A request is `HOST PATH LENGTH` and CRLF
//! (the length of data sent after it: 0 here); an answer is a one-digit
//! status, a space, a "meta" line, CRLF and, for status 2, the document,
//! mostly gemtext (ARCHITECTURE.md §15.12).

use std::time::Duration;

use lost::urls::Url;

use crate::crawl::{Parsed, Protocol, Response, Route, mentions, port};
use crate::gemini::{charset, decode, gemtext, mime_type};
use crate::net::{self, Proxy};

/// Where a crawl starts: the protocol's home, and a capsule that lists
/// others.
const SEEDS: &[&str] = &["spartan://mozz.us/", "spartan://spartan.mozz.us/"];

pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Through a proxy (Tor adds a few seconds a hop).
pub const PROXIED_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Spartan {
    /// A SOCKS5 proxy (Spartan can't go through an HTTP one).
    proxy: Option<Proxy>,
    timeout: Duration,
}

impl Spartan {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Spartan> {
        if let Some(Proxy::Http(_)) = proxy {
            return Err("Spartan can only go through a SOCKS5 proxy (socks5://HOST:PORT)".into());
        }
        let timeout = timeout.unwrap_or(match proxy {
            Some(_) => PROXIED_TIMEOUT,
            None => TIMEOUT,
        });
        Ok(Spartan { proxy, timeout })
    }
}

impl Protocol for Spartan {
    const NAME: &'static str = "spartan";
    const SCHEME: &'static str = "spartan";
    const SPACE: &'static str = "Spartan";
    const HOSTS: &'static str = "capsules";

    fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        let host = url.host.trim_start_matches('[').trim_end_matches(']');
        let request = format!("{host} {} 0\r\n", url.path);
        let answer = net::exchange(
            route,
            self.proxy.as_ref(),
            &url.host,
            port(url),
            &request,
            self.timeout,
            lost::MAX_BODY_BYTES,
        )?;
        Ok(response(&answer))
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let text = decode(body, charset(mime).as_deref());
        let mut parsed = if mime_type(mime) == "text/gemini" {
            gemtext(url, &text)
        } else {
            Parsed {
                body: text.clone(),
                ..Parsed::default()
            }
        };
        for link in mentions(&text, Self::SCHEME) {
            if !parsed.links.iter().any(|l| l.url == link.url) {
                parsed.links.push(link);
            }
        }
        parsed
    }
}

/// An answer, read: `2 text/gemini` and the document, `3 /path` (moved,
/// on the same host), `4 …` (no such thing), `5 …` (the server failed).
fn response(answer: &[u8]) -> Response {
    let Some(end) = answer.iter().position(|&b| b == b'\n') else {
        return Response::Gone;
    };
    let line = String::from_utf8_lossy(&answer[..end]);
    let line = line.trim_end_matches('\r');
    let (status, meta) = match line.split_once(' ') {
        Some((status, meta)) => (status, meta.trim()),
        None => (line, ""),
    };
    match status {
        "2" => {
            let mime = if meta.is_empty() {
                "text/gemini; charset=utf-8".to_string()
            } else {
                meta.to_string()
            };
            let body = mime_type(&mime)
                .starts_with("text/")
                .then(|| answer[end + 1..].to_vec());
            Response::Document { mime, body }
        }
        "3" if !meta.is_empty() => Response::Redirect(meta.to_string()),
        "5" => Response::TryLater,
        _ => Response::Gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(
            response(b"2 text/gemini\r\n# Hi\n=> /about About\n"),
            Response::Document {
                mime: "text/gemini".into(),
                body: Some(b"# Hi\n=> /about About\n".to_vec())
            }
        );
        // No type: gemtext.
        assert!(matches!(
            response(b"2\r\nhello"),
            Response::Document { mime, body: Some(_) } if mime.starts_with("text/gemini")
        ));
        // Not text: its type only.
        assert_eq!(
            response(b"2 image/png\r\n\x89PNG"),
            Response::Document {
                mime: "image/png".into(),
                body: None
            }
        );
        assert_eq!(response(b"3 /new/\r\n"), Response::Redirect("/new/".into()));
        assert_eq!(response(b"4 Not found\r\n"), Response::Gone);
        assert_eq!(response(b"5 Oops\r\n"), Response::TryLater);
        assert_eq!(response(b"garbage"), Response::Gone);
    }

    #[test]
    fn a_capsule() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n") {
                s.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            s.write_all(
                b"2 text/gemini\r\n# Mozz\n=> /about.gmi About\n=> gemini://mozz.us/ Gemini\n\
                  See spartan://other.example/ too.\n",
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let spartan = Spartan::new(None, Some(Duration::from_secs(5))).unwrap();
        let url = Url::parse(&format!("spartan://127.0.0.1:{port}/a/")).unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let Ok(Response::Document { mime, body }) = spartan.fetch(Route::Direct(address), &url)
        else {
            panic!("a page");
        };
        let page = spartan.parse(&url, &mime, &body.unwrap());
        assert_eq!(page.title.as_deref(), Some("Mozz"));
        let links: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            links,
            [
                format!("spartan://127.0.0.1:{port}/about.gmi").as_str(),
                "gemini://mozz.us/",
                "spartan://other.example/"
            ]
        );
        // The host (no port), the path, no data.
        assert_eq!(server.join().unwrap(), "127.0.0.1 /a/ 0\r\n");
        assert!(Spartan::accepts(&Url::parse("spartan://mozz.us/").unwrap()));
        assert_eq!(Url::parse("spartan://mozz.us:300/").unwrap().port, None);
    }
}
