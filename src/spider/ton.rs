//! TON Sites: the web over TON's network, through a TON proxy's HTTP proxy
//! (`rldp-http-proxy`, or Tonutils Proxy; `127.0.0.1:8080` by default).
//! The proxy looks up `.ton` names in TON DNS, reaches `.adnl` addresses
//! and `.bag` storage directly, and sends anything else to the ordinary
//! web, so only those three are crawled (ARCHITECTURE.md §15.10).

use std::time::{Duration, Instant};

use lost::urls::Url;

use crate::crawl::{Parsed, Protocol, Response, Route};
use crate::http::{self, Http};
use crate::net::Proxy;

/// Where a crawl starts: the TON Foundation's site, and a few others that
/// link around.
const SEEDS: &[&str] = &[
    "http://foundation.ton/",
    "http://storage.ton/",
    "http://howtorunsite.ton/",
    "http://tondnsdomain.ton/",
];

/// How long to wait for a proxy that has just started (it reads TON's
/// state before it can look up names).
const WARM_UP: Duration = Duration::from_secs(5 * 60);

pub const TIMEOUT: Duration = Duration::from_secs(60);
pub const PROXY: &str = "http://127.0.0.1:8080";

/// What the proxy takes as TON's: anything else would go to the web.
const DOMAINS: &[&str] = &[".ton", ".adnl", ".bag"];

pub struct Ton {
    http: Http,
}

impl Ton {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Ton> {
        let proxy = match proxy {
            Some(proxy) => proxy,
            None => Proxy::parse(PROXY)?,
        };
        Ok(Ton {
            http: Http::new(Some(proxy), timeout.unwrap_or(TIMEOUT))?,
        })
    }
}

impl Protocol for Ton {
    const NAME: &'static str = "ton";
    const SCHEME: &'static str = "http";
    const SPACE: &'static str = "TON Sites";
    const HOSTS: &'static str = "sites";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    /// The proxy answers for sites it can't reach (5xx): those count as the
    /// site being unreachable, not as its pages.
    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        let answer = self.http.get(route, url)?;
        if answer.status >= 500 {
            return Err(format!("unreachable ({})", answer.status));
        }
        Ok(http::response(answer))
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        http::parse(url, mime, body)
    }

    /// Waits for the proxy to reach a site (it can take a while after it
    /// starts).
    fn prepare(&self, _blacklist: &mut crate::crawl::Blacklist) -> lost::Result<()> {
        let url = Url::parse(SEEDS[0]).expect("a valid URL");
        let started = Instant::now();
        let mut said = false;
        loop {
            match self.fetch(Route::Proxied, &url) {
                Ok(_) => {
                    if said {
                        eprintln!("The TON proxy is ready.");
                    }
                    return Ok(());
                }
                Err(_) if started.elapsed() < WARM_UP => {
                    if !said {
                        eprintln!(
                            "Waiting for the TON proxy to be able to reach sites \
                             (a minute or two after it starts)…"
                        );
                        said = true;
                    }
                    std::thread::sleep(Duration::from_secs(10));
                }
                Err(e) => {
                    return Err(format!(
                        "the TON proxy couldn't reach {} in {} minutes ({e}); is it connected?",
                        url.host,
                        WARM_UP.as_secs() / 60
                    )
                    .into());
                }
            }
        }
    }

    fn accepts(url: &Url) -> bool {
        url.scheme == "http" && DOMAINS.iter().any(|d| url.host.ends_with(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let accepts = |s: &str| Ton::accepts(&Url::parse(s).unwrap());
        assert!(accepts("http://foundation.ton/"));
        assert!(accepts("http://blog.foundation.ton/a"));
        assert!(accepts(
            "http://vcqmha5j3ceve35ammfrhqty46rkhi455otydstv66pk2tmf7rl25f3.adnl/"
        ));
        assert!(accepts(
            "http://45061c1d4ec44a937d0318589e13c73d151d1cef5d3c0e53afbcf56a6c2fe2bd.bag/"
        ));
        // The proxy would send these to the ordinary web.
        assert!(!accepts("http://ton.org/"));
        assert!(!accepts("http://foundation.ton.website/"));
        assert!(!accepts("http://example.tonx/"));
        assert!(!accepts("https://foundation.ton/"));
        assert!(!accepts("gemini://foundation.ton/"));
        // A bare name is a .ton site.
        assert_eq!(Ton::SCHEME, "http");
    }

    #[test]
    fn through_the_proxy() {
        use std::io::{Read, Write};
        // A proxy that has the site, then one that can't reach it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Proxy::Http(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            let page = "<title>TON</title><a href=\"http://storage.ton/\">Storage</a>";
            for answer in [
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}",
                    page.len()
                ),
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n".to_string(),
            ] {
                let (mut s, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    s.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                asked.push(String::from_utf8(request).unwrap());
                s.write_all(answer.as_bytes()).unwrap();
            }
            asked
        });
        let ton = Ton::new(Some(proxy), Some(Duration::from_secs(5))).unwrap();
        let url = Url::parse("http://foundation.ton/").unwrap();
        let Ok(Response::Document { mime, body }) = ton.fetch(Route::Proxied, &url) else {
            panic!("a page");
        };
        let parsed = ton.parse(&url, &mime, &body.unwrap());
        assert_eq!(parsed.title.as_deref(), Some("TON"));
        assert_eq!(parsed.links[0].url, "http://storage.ton/");
        assert_eq!(
            ton.fetch(Route::Proxied, &url),
            Err("unreachable (502)".to_string())
        );
        let asked = server.join().unwrap();
        // The whole URL, to the proxy.
        assert!(asked[0].starts_with("GET http://foundation.ton/ HTTP/1.1\r\n"));
    }
}
