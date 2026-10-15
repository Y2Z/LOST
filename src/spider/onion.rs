//! `.onion` sites: the web, through Tor's SOCKS5 proxy (`127.0.0.1:9050` by
//! default). Tor resolves the names; only v3 addresses (56 letters and
//! digits) are crawled, the older ones having been switched off in 2021.
//!
//! Before crawling, the spider fetches Ahmia's list of the onion sites it
//! bans for abuse material, and skips them; without that list it doesn't
//! start (ARCHITECTURE.md §15.6).

use std::time::{Duration, Instant};

use lost::urls::Url;

use crate::crawl::{Blacklist, Parsed, Protocol, Response, Route};
use crate::http::{self, Http};
use crate::net::Proxy;

/// Ahmia, the onion search engine, and its list of the onion sites it knows
/// (`/onions/`; its `/address/` page is the same list, but robots.txt
/// keeps crawlers out of that one).
const AHMIA: &str = "juhanurmihxlp77nkq76byazcldy2hlmovfu2epvl5ankdibsot4csyd.onion";
const SEEDS: &[&str] = &[
    "http://juhanurmihxlp77nkq76byazcldy2hlmovfu2epvl5ankdibsot4csyd.onion/",
    "http://juhanurmihxlp77nkq76byazcldy2hlmovfu2epvl5ankdibsot4csyd.onion/onions/",
];

pub const TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for Tor to connect, and for Ahmia's list.
const WARM_UP: Duration = Duration::from_secs(5 * 60);
pub const PROXY: &str = "socks5://127.0.0.1:9050";

pub struct Onion {
    http: Http,
}

impl Onion {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<Onion> {
        let proxy = match proxy {
            Some(proxy @ Proxy::Socks5(_)) => proxy,
            Some(Proxy::Http(_)) => {
                return Err("onion sites need Tor's SOCKS5 proxy (socks5://HOST:PORT)".into());
            }
            None => Proxy::parse(PROXY)?,
        };
        Ok(Onion {
            http: Http::new(Some(proxy), timeout.unwrap_or(TIMEOUT))?,
        })
    }
}

impl Protocol for Onion {
    const NAME: &'static str = "onion";
    const SCHEME: &'static str = "http";
    const SPACE: &'static str = "Onionspace";
    const HOSTS: &'static str = "onion sites";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        Ok(http::response(self.http.get(route, url)?))
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        http::parse(url, mime, body)
    }

    fn accepts(url: &Url) -> bool {
        (url.scheme == "http" || url.scheme == "https") && is_v3(&url.host)
    }

    /// Ahmia's list of banned sites: MD5 hashes of their names, one a line.
    fn prepare(&self, blacklist: &mut Blacklist) -> lost::Result<()> {
        eprintln!("Getting Ahmia's list of onion sites banned for abuse material…");
        let url = Url::parse(&format!("http://{AHMIA}/blacklist/banned/")).expect("a valid URL");
        let started = Instant::now();
        let mut waiting = false;
        loop {
            let last = match self.http.get(Route::Proxied, &url) {
                Ok(answer) if answer.status == 200 => {
                    let text = String::from_utf8_lossy(&answer.body);
                    let added = blacklist.ban_hashed(text.lines().map(str::to_string));
                    if added > 0 {
                        if waiting {
                            eprintln!("Connected.");
                        }
                        eprintln!(
                            "{} sites will be skipped.",
                            lost::text::thousands(added as u64)
                        );
                        return Ok(());
                    }
                    "it came back empty".to_string()
                }
                Ok(answer) => format!("Ahmia answered {}", answer.status),
                Err(e) => e,
            };
            // Tor reaches nothing until it has connected to the network, a
            // few seconds to a few minutes after it starts.
            if started.elapsed() >= WARM_UP {
                return Err(format!(
                    "couldn't get Ahmia's list of banned onion sites in {} minutes ({last}), \
                     so not crawling; is Tor connected?",
                    WARM_UP.as_secs() / 60
                )
                .into());
            }
            if !waiting {
                eprintln!("Waiting for Tor to connect…");
                waiting = true;
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    }
}

/// `…56 base32 letters….onion`, subdomains allowed.
fn is_v3(host: &str) -> bool {
    let Some(rest) = host.strip_suffix(".onion") else {
        return false;
    };
    let name = rest.rsplit('.').next().unwrap_or("");
    name.len() == 56
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        let url = |u: &str| Url::parse(u).unwrap();
        assert!(Onion::accepts(&url(&format!("http://{AHMIA}/"))));
        assert!(Onion::accepts(&url(&format!("https://www.{AHMIA}/x"))));
        assert!(!Onion::accepts(&url("http://expyuzz4wqqyqhjn.onion/")));
        assert!(!Onion::accepts(&url("http://example.com/")));
        assert!(!Onion::accepts(&url(&format!("gopher://{AHMIA}/"))));
        assert!(Onion::new(Some(Proxy::Http("127.0.0.1:4444".into())), None).is_err());
    }
}
