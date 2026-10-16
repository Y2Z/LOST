//! Nex (nex://nightfall.city/nex/info/specification.txt): plain TCP on
//! port 1900. A request is a path and CRLF; the answer is the file itself,
//! until the server closes the connection, with no status or type. A path
//! ending in `/` is a directory: text whose `=> URL label` lines are links,
//! as in gemtext (ARCHITECTURE.md §15.12).

use std::time::Duration;

use lost::urls::Url;

use crate::crawl::{Parsed, Protocol, Response, Route, mentions, percent_decode, port};
use crate::gemini::gemtext;
use crate::net::{self, Proxy};

/// Where a crawl starts: the protocol's home, Nightfall City.
const SEEDS: &[&str] = &["nex://nightfall.city/"];

pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Through a proxy (Tor adds a few seconds a hop).
pub const PROXIED_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Nex {
    /// A SOCKS5 proxy (Nex can't go through an HTTP one).
    proxy: Option<Proxy>,
    timeout: Duration,
}

impl Nex {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Nex> {
        if let Some(Proxy::Http(_)) = proxy {
            return Err("Nex can only go through a SOCKS5 proxy (socks5://HOST:PORT)".into());
        }
        let timeout = timeout.unwrap_or(match proxy {
            Some(_) => PROXIED_TIMEOUT,
            None => TIMEOUT,
        });
        Ok(Nex { proxy, timeout })
    }
}

impl Protocol for Nex {
    const NAME: &'static str = "nex";
    const SCHEME: &'static str = "nex";
    const SPACE: &'static str = "Nexspace";
    const HOSTS: &'static str = "servers";

    fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        let request = format!("{}\r\n", percent_decode(&url.path));
        let answer = net::exchange(
            route,
            self.proxy.as_ref(),
            &url.host,
            port(url),
            &request,
            self.timeout,
            lost::MAX_BODY_BYTES,
        )?;
        if answer.is_empty() {
            return Ok(Response::Gone);
        }
        // Nothing says what it is: a directory by its `/`, gemtext by its
        // name, anything with a NUL in it isn't text.
        let mime = if url.path.ends_with('/') {
            "text/nex-directory"
        } else if url.path.ends_with(".gmi") || url.path.ends_with(".gemini") {
            "text/gemini"
        } else if answer.contains(&0) {
            return Ok(Response::Document {
                mime: "application/octet-stream".into(),
                body: None,
            });
        } else {
            "text/plain"
        };
        Ok(Response::Document {
            mime: mime.into(),
            body: Some(answer),
        })
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let text = String::from_utf8_lossy(body).into_owned();
        let mut parsed = match mime {
            "text/nex-directory" | "text/gemini" => gemtext(url, &text),
            _ => Parsed {
                body: text.clone(),
                ..Parsed::default()
            },
        };
        for link in mentions(&text, Self::SCHEME) {
            if !parsed.links.iter().any(|l| l.url == link.url) {
                parsed.links.push(link);
            }
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server giving `answers` in turn; what it was asked comes back.
    fn serve(answers: Vec<&'static [u8]>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in answers {
                let (mut s, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n") {
                    s.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                asked.push(String::from_utf8(request).unwrap());
                s.write_all(answer).unwrap();
            }
            asked
        });
        (port, server)
    }

    #[test]
    fn directories_and_files() {
        let (port, server) = serve(vec![
            b"Welcome to my nex\n=> notes/ My notes\n=> about.txt\n=> nex://other.example/\n",
            b"Just text, mentioning nex://third.example/ once.\n",
            b"\x89PNG\x00\x00",
            b"",
        ]);
        let nex = Nex::new(None, Some(Duration::from_secs(5))).unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let fetch = |path: &str| {
            let url = Url::parse(&format!("nex://127.0.0.1:{port}{path}")).unwrap();
            (nex.fetch(Route::Direct(address), &url).unwrap(), url)
        };
        let (Response::Document { mime, body }, url) = fetch("/") else {
            panic!("a directory");
        };
        let page = nex.parse(&url, &mime, &body.unwrap());
        let links: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        let base = format!("nex://127.0.0.1:{port}");
        assert_eq!(
            links,
            [
                format!("{base}/notes/").as_str(),
                format!("{base}/about.txt").as_str(),
                "nex://other.example/"
            ]
        );
        assert_eq!(page.links[0].label.as_deref(), Some("My notes"));
        let (Response::Document { mime, body }, url) = fetch("/about%20me.txt") else {
            panic!("a file");
        };
        assert_eq!(mime, "text/plain");
        let page = nex.parse(&url, &mime, &body.unwrap());
        assert_eq!(page.links[0].url, "nex://third.example/");
        assert_eq!(
            fetch("/cat.png").0,
            Response::Document {
                mime: "application/octet-stream".into(),
                body: None
            }
        );
        assert_eq!(fetch("/missing").0, Response::Gone);
        // The path, decoded, and CRLF.
        assert_eq!(
            server.join().unwrap(),
            ["/\r\n", "/about me.txt\r\n", "/cat.png\r\n", "/missing\r\n"]
        );
    }
}
