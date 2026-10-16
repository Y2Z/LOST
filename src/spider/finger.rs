//! Finger (RFC 1288; its URLs: `finger://HOST/USER`): plain TCP on port
//! 79. A request is a user name (none: the server's own page) and CRLF;
//! the answer is text about them, their `.plan`, until the server closes
//! the connection. Links are the URLs written in it (ARCHITECTURE.md
//! §15.12).

use std::time::Duration;

use lost::urls::Url;

use crate::crawl::{Parsed, Protocol, Response, Route, mentions, percent_decode, port};
use crate::net::{self, Proxy};

/// Where a crawl starts: servers that host people's `.plan`s.
const SEEDS: &[&str] = &["finger://plan.cat/", "finger://happynetbox.com/"];

pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Through a proxy (Tor adds a few seconds a hop).
pub const PROXIED_TIMEOUT: Duration = Duration::from_secs(30);
/// What's written in a `.plan` and kept as a link (only `finger://` ones
/// are followed).
const SCHEMES: &[&str] = &[
    "finger", "gemini", "gopher", "spartan", "nex", "https", "http",
];

pub struct Finger {
    /// A SOCKS5 proxy (Finger can't go through an HTTP one).
    proxy: Option<Proxy>,
    timeout: Duration,
}

impl Finger {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Finger> {
        if let Some(Proxy::Http(_)) = proxy {
            return Err("Finger can only go through a SOCKS5 proxy (socks5://HOST:PORT)".into());
        }
        let timeout = timeout.unwrap_or(match proxy {
            Some(_) => PROXIED_TIMEOUT,
            None => TIMEOUT,
        });
        Ok(Finger { proxy, timeout })
    }
}

/// The user a URL asks about: its path's first part, decoded.
fn user(url: &Url) -> String {
    percent_decode(
        url.path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or(""),
    )
}

impl Protocol for Finger {
    const NAME: &'static str = "finger";
    const SCHEME: &'static str = "finger";
    const SPACE: &'static str = "Fingerspace";
    const HOSTS: &'static str = "servers";

    fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    /// No such thing: a robots.txt would be a user called that.
    fn has_robots(_origin: &Url) -> bool {
        false
    }

    /// `user@host`, or the host for its own page.
    fn fallback_title(url: &Url) -> String {
        match user(url) {
            user if user.is_empty() => url.host.clone(),
            user => format!("{user}@{}", url.host),
        }
    }

    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        let user = user(url);
        // Only a name: no forwarding (`user@other.host`) or odd bytes.
        if user.contains(['@', '\r', '\n', '\t', ' ']) {
            return Ok(Response::Gone);
        }
        let answer = net::exchange(
            route,
            self.proxy.as_ref(),
            &url.host,
            port(url),
            &format!("{user}\r\n"),
            self.timeout,
            lost::MAX_BODY_BYTES,
        )?;
        if answer.iter().all(u8::is_ascii_whitespace) {
            return Ok(Response::Gone);
        }
        Ok(Response::Document {
            mime: "text/plain".into(),
            body: Some(answer),
        })
    }

    fn parse(&self, url: &Url, _mime: &str, body: &[u8]) -> Parsed {
        let text = String::from_utf8_lossy(body).replace('\r', "");
        let mut links = Vec::new();
        for scheme in SCHEMES {
            for link in mentions(&text, scheme) {
                if !links.iter().any(|l: &crate::crawl::Link| l.url == link.url) {
                    links.push(link);
                }
            }
        }
        Parsed {
            title: Some(Self::fallback_title(url)),
            body: text,
            links,
            skip: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in [
                &b"Login: alice\r\nPlan:\r\nWorking on finger://plan.cat/bob and https://example.org/.\r\n"[..],
                b"\r\n",
            ] {
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
        let finger = Finger::new(None, Some(Duration::from_secs(5))).unwrap();
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let url = Url::parse(&format!("finger://127.0.0.1:{port}/alice")).unwrap();
        let Ok(Response::Document { mime, body }) = finger.fetch(Route::Direct(address), &url)
        else {
            panic!("a plan");
        };
        let page = finger.parse(&url, &mime, &body.unwrap());
        assert_eq!(page.title, Some("alice@127.0.0.1".to_string()));
        assert_eq!(
            page.body,
            "Login: alice\nPlan:\nWorking on finger://plan.cat/bob and https://example.org/.\n"
        );
        let links: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(links, ["finger://plan.cat/bob", "https://example.org/"]);
        // Nothing to say: gone.
        let url = Url::parse(&format!("finger://127.0.0.1:{port}/")).unwrap();
        assert_eq!(
            finger.fetch(Route::Direct(address), &url),
            Ok(Response::Gone)
        );
        assert_eq!(server.join().unwrap(), ["alice\r\n", "\r\n"]);
        // No forwarding to other hosts.
        let url = Url::parse("finger://plan.cat/bob@elsewhere.example").unwrap();
        assert_eq!(
            finger.fetch(Route::Direct(address), &url),
            Ok(Response::Gone)
        );
        assert_eq!(
            Finger::fallback_title(&Url::parse("finger://plan.cat/").unwrap()),
            "plan.cat"
        );
        assert!(!Finger::has_robots(
            &Url::parse("finger://plan.cat/").unwrap()
        ));
    }
}
