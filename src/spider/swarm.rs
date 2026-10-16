//! Swarm: Ethereum's storage network, where a site is a manifest (its files
//! by path) under the hash of its content, fetched over HTTP from a gateway
//! (`https://api.gateway.ethswarm.org` by default; `--gateway` names
//! another, a Bee node on this computer say: `http://127.0.0.1:1633`).
//!
//! Pages are saved under `bzz://HASH/path` (64 hex digits), or
//! `bzz://NAME.eth/path` for ENS names, which the gateway looks up. Links to
//! gateways (`https://NAME.bzz.link/`, `https://CID.bzz.link/`,
//! `…/bzz/HASH/…`) and CIDs (`bah5…`) are written that way too. Swarm has no
//! index to list, so the crawl follows links only (ARCHITECTURE.md §15.11).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use lost::urls::Url;

use crate::crawl::{Blacklist, Parsed, Protocol, Response, Route, mentions};
use crate::http::{self, Answer, Http};
use crate::ipfs::{base32_decode, varint};
use crate::net::Proxy;

pub const TIMEOUT: Duration = Duration::from_secs(60);
pub const GATEWAY: &str = "https://api.gateway.ethswarm.org";

/// Where a crawl starts: Swarm's own site, the Fair Data Society's, and a
/// site its gateway guide links to by CID.
const SEEDS: &[&str] = &[
    "bzz://swarm.eth/",
    "bzz://fairdataprotocol.eth/",
    "bzz://fdfd170f73953bc262d936d3a5329b787980335dc0547032bb2a6239ebe95a76/",
];

/// Gateways, whose links are links to Swarm: `/bzz/HASH/…` on them, or a
/// name or CID as a `bzz.link` subdomain.
const GATEWAYS: &[&str] = &["bzz.link", "gateway.ethswarm.org"];

/// Multicodecs of Swarm's CIDs: a manifest, a feed.
const MANIFEST: u64 = 0xfa;
const FEED: u64 = 0xfb;
/// Keccak-256, as a multihash.
const KECCAK_256: u64 = 0x1b;

pub struct Swarm {
    http: Http,
    /// The gateway's scheme, host and port.
    gateway: Url,
    /// Where it is, for telling servers apart (none through a proxy).
    address: Option<SocketAddr>,
    /// It's on this computer: requests to it needn't wait for each other.
    local: bool,
}

impl Swarm {
    pub fn new(
        gateway: Option<String>,
        proxy: Option<Proxy>,
        timeout: Option<Duration>,
    ) -> lost::Result<Swarm> {
        let text = gateway.unwrap_or_else(|| GATEWAY.to_string());
        let gateway = Url::parse(text.trim().trim_end_matches('/'))
            .filter(|u| {
                (u.scheme == "http" || u.scheme == "https") && u.path == "/" && u.query.is_none()
            })
            .ok_or_else(|| {
                format!("--gateway takes http://HOST:PORT or https://HOST (not \"{text}\")")
            })?;
        let ip = gateway
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        let local = proxy.is_none()
            && (gateway.host == "localhost" || ip.is_some_and(|ip| ip.is_loopback()));
        let address = match proxy {
            Some(_) => None,
            None => Some(lookup(&gateway)?),
        };
        Ok(Swarm {
            http: Http::new(proxy, timeout.unwrap_or(TIMEOUT))?,
            gateway,
            address,
            local,
        })
    }

    fn route(&self) -> Route {
        match self.address {
            Some(address) => Route::Direct(address),
            None => Route::Proxied,
        }
    }

    /// The gateway's answer for `path`, its own redirects followed (a
    /// directory's `/` added, say).
    fn get(&self, path: &str) -> Result<Answer, String> {
        let mut url = self.gateway.clone();
        url.path = path.to_string();
        let mut answer = self.http.get(self.route(), &url)?;
        for _ in 0..3 {
            let location = match answer.status {
                301 | 302 | 303 | 307 | 308 => answer.header("location").map(str::to_string),
                _ => None,
            };
            let Some(location) = location else {
                break;
            };
            // Only the gateway's own paths: anywhere else is a link.
            let path = match Url::parse(&location) {
                Some(target) if target.host == self.gateway.host => target.path,
                Some(_) => break,
                None if location.starts_with('/') => {
                    location.split('?').next().unwrap_or("/").to_string()
                }
                None => break,
            };
            // The gateway refusing the site: said as it is, not followed.
            if path.starts_with("/forbidden") {
                break;
            }
            url.path = path;
            answer = self.http.get(self.route(), &url)?;
        }
        Ok(answer)
    }
}

/// The address of a host, looked up here.
fn lookup(url: &Url) -> Result<SocketAddr, String> {
    let port = url
        .port
        .unwrap_or(if url.scheme == "https" { 443 } else { 80 });
    let host = url.host.trim_start_matches('[').trim_end_matches(']');
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| format!("can't find {}", url.host))
}

impl Protocol for Swarm {
    const NAME: &'static str = "swarm";
    /// `--link swarm.eth` is a name: `bzz://swarm.eth/`.
    const SCHEME: &'static str = "bzz";
    const SPACE: &'static str = "Swarm";
    const HOSTS: &'static str = "sites";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    /// Through a gateway on this computer, every site is a server of its
    /// own; through any other, they all are that one server, asked one
    /// thing at a time (`--delay`).
    fn proxied(&self) -> bool {
        self.local
    }

    fn resolve(&self, _url: &Url) -> Option<SocketAddr> {
        Some(
            self.address
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
        )
    }

    fn accepts(url: &Url) -> bool {
        url.scheme == "bzz" && (is_hash(&url.host) || is_ens(&url.host))
    }

    fn rewrite(link: &str) -> Option<String> {
        canonical(link)
    }

    /// `GET /bzz/HASH/path`. A gateway that won't serve a site (public ones
    /// only serve the sites they allow by hash) sends the crawler to its
    /// `/forbidden` page: the site is as good as gone.
    fn fetch(&self, _route: Route, url: &Url) -> Result<Response, String> {
        let answer = self.get(&format!("/bzz/{}{}", url.host, url.path))?;
        if answer.status >= 500 {
            return Err(format!("unreachable ({})", answer.status));
        }
        Ok(match http::response(answer) {
            Response::Redirect(location) => {
                let forbidden = Url::parse(&location)
                    .map_or(location.clone(), |u| u.path)
                    .starts_with("/forbidden");
                if forbidden {
                    Response::Gone
                } else {
                    Response::Redirect(canonical(&location).unwrap_or(location))
                }
            }
            other => other,
        })
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let mut parsed = http::parse(url, mime, body);
        let written = mentions(&parsed.body, "bzz");
        let mut links = Vec::new();
        for mut link in std::mem::take(&mut parsed.links).into_iter().chain(written) {
            if let Some(url) = canonical(&link.url) {
                link.url = url;
            }
            if !links.iter().any(|l: &crate::crawl::Link| l.url == link.url) {
                links.push(link);
            }
        }
        parsed.links = links;
        parsed
    }

    /// The gateway must answer; a public one serves only some sites.
    fn prepare(&self, _blacklist: &mut Blacklist) -> lost::Result<()> {
        self.get("/bzz/swarm.eth/").map_err(|e| {
            format!(
                "can't reach the Swarm gateway at {} ({e})",
                self.gateway.host
            )
        })?;
        if !self.local {
            eprintln!(
                "Through {}: public gateways serve only the sites they allow; a Bee node \
                 of your own reaches all of Swarm (--gateway http://127.0.0.1:1633).",
                self.gateway.host
            );
        }
        Ok(())
    }
}

/// The one way a Swarm address is saved: `bzz://HASH/path` (lowercase) or
/// `bzz://NAME.eth/path`, from a `bzz://` link (with a hash, a CID or a
/// name) or a gateway's (`https://NAME.bzz.link/…`, `https://CID.bzz.link/…`,
/// `https://GATEWAY/bzz/HASH/…`). Queries and fragments go.
pub fn canonical(text: &str) -> Option<String> {
    let text = text.trim();
    let (scheme, rest) = text.split_once("://")?;
    let rest = rest.split(['#', '?']).next().unwrap_or("");
    let address = |host: &str, path: &str| -> Option<String> {
        let path = if path.is_empty() { "/" } else { path };
        let host = host.to_ascii_lowercase();
        let host = if is_hash(&host) || is_ens(&host) {
            host
        } else {
            cid_hash(&host)?
        };
        Some(format!("bzz://{host}{path}"))
    };
    match scheme.to_ascii_lowercase().as_str() {
        "bzz" => {
            let end = rest.find('/').unwrap_or(rest.len());
            let (host, path) = rest.split_at(end);
            // `/bzz/HASH/…` from a page made for gateways, read as if it
            // were on one.
            if let Some(inner) = path.strip_prefix("/bzz/")
                && let Some(link) = canonical(&format!("bzz://{inner}"))
            {
                return Some(link);
            }
            address(host, path)
        }
        "http" | "https" => {
            let url = Url::parse(text)?;
            let gateway = GATEWAYS.iter().find(|g| {
                url.host == **g
                    || url
                        .host
                        .strip_suffix(**g)
                        .is_some_and(|sub| sub.ends_with('.'))
            })?;
            // `/bzz/HASH/…`, on the gateway or a subdomain of it.
            if let Some(rest) = url.path.strip_prefix("/bzz/") {
                let end = rest.find('/').unwrap_or(rest.len());
                let (host, path) = rest.split_at(end);
                return address(host, path);
            }
            // `NAME.bzz.link` (`NAME.eth`), `CID.bzz.link`.
            let label = url.host.strip_suffix(*gateway)?.strip_suffix('.')?;
            if label.contains('.') || label == "api" || label == "www" {
                return None;
            }
            match cid_hash(label) {
                Some(hash) => address(&hash, &url.path),
                None => address(&format!("{label}.eth"), &url.path),
            }
        }
        _ => None,
    }
}

/// A reference: 32 bytes, as 64 hex digits.
fn is_hash(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An ENS name: `.eth`, with labels of letters, digits and `-`.
fn is_ens(text: &str) -> bool {
    let Some(name) = text.strip_suffix(".eth") else {
        return false;
    };
    !name.is_empty()
        && text.len() <= 253
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// A Swarm CID's hash, hex: base32 (`b…`), version 1, a manifest's or a
/// feed's codec, a Keccak-256 multihash.
fn cid_hash(text: &str) -> Option<String> {
    let bytes = base32_decode(text.strip_prefix('b')?)?;
    let (1, rest) = varint(&bytes)? else {
        return None;
    };
    let (codec, rest) = varint(rest)?;
    let (hash, rest) = varint(rest)?;
    let (length, digest) = varint(rest)?;
    if !matches!(codec, MANIFEST | FEED) || hash != KECCAK_256 || length != 32 || digest.len() != 32
    {
        return None;
    }
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "6c45eae389b3bffce21443316d0bd47c4101545092b7c72c313a33ee7d003475";

    #[test]
    fn one_way_to_write_each_address() {
        let c = |s: &str| canonical(s);
        let site = format!("bzz://{HASH}/");
        assert_eq!(c(&format!("bzz://{HASH}")), Some(site.clone()));
        assert_eq!(
            c(&format!("bzz://{}/a.html?x=1#y", HASH.to_uppercase())),
            Some(format!("bzz://{HASH}/a.html"))
        );
        assert_eq!(
            c("bzz://Swarm.ETH/build"),
            Some("bzz://swarm.eth/build".into())
        );
        // Gateways' links.
        assert_eq!(
            c(&format!(
                "https://api.gateway.ethswarm.org/bzz/{HASH}/docs/"
            )),
            Some(format!("bzz://{HASH}/docs/"))
        );
        assert_eq!(
            c(&format!("https://gateway.ethswarm.org/bzz/{HASH}")),
            Some(site.clone())
        );
        assert_eq!(
            c("https://swarm.bzz.link/build"),
            Some("bzz://swarm.eth/build".into())
        );
        // CIDs: a feed's and a manifest's, as their hashes.
        assert_eq!(
            c("https://bah5qcgza7x6rod3tsu54eywzg3j2kmu3pb4yam25ybkhamv3fjrdt27jlj3a.bzz.link/"),
            Some("bzz://fdfd170f73953bc262d936d3a5329b787980335dc0547032bb2a6239ebe95a76/".into())
        );
        assert_eq!(
            c("bzz://bah5acgzafqsrk5aadnqrurohacfyn6hayqnunelko275sxj2zaf4quhogvia/x"),
            Some("bzz://2c251574001b611a45c7008b86f8e0c41b46916a76bfd95d3ac80bc850ee3550/x".into())
        );
        // Not Swarm's.
        assert_eq!(c("https://example.org/bzz/x"), None);
        assert_eq!(c("https://bzz.link/"), None);
        assert_eq!(c("https://a.b.bzz.link/"), None);
        assert_eq!(c("bzz://not-a-hash/"), None);
        assert_eq!(c("bzz://-bad.eth/"), None);
        // An IPFS CID isn't a Swarm one.
        assert_eq!(
            c("bzz://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/"),
            None
        );
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(Swarm::accepts(&url(&site)));
        assert!(Swarm::accepts(&url("bzz://swarm.eth/")));
        assert!(Swarm::accepts(&url("bzz://docs.swarm.eth/a")));
        assert!(!Swarm::accepts(&url("bzz://example.org/")));
        assert!(!Swarm::accepts(&url(&format!("https://{HASH}/"))));
        assert_eq!(
            Swarm::rewrite("https://swarm.bzz.link/"),
            Some("bzz://swarm.eth/".into())
        );
    }

    /// A gateway on this computer giving `answers` in turn; what it was
    /// asked comes back at the end.
    fn serve(answers: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in answers {
                let (mut s, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    s.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                asked.push(request.lines().next().unwrap().to_string());
                s.write_all(answer.as_bytes()).unwrap();
            }
            asked
        });
        (address, server)
    }

    #[test]
    fn through_the_gateway() {
        let page = format!(
            "<title>Swarm</title><a href=\"https://fairdataprotocol.bzz.link/\">FDP</a> \
             <a href=\"/bzz/{HASH}/about\">About</a>"
        );
        let (gateway, server) = serve(vec![
            // A directory without its `/`: the gateway adds it.
            format!("HTTP/1.1 308 Permanent Redirect\r\nLocation: /bzz/{HASH}/docs/\r\n\r\n"),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}",
                page.len()
            ),
            // A site the gateway won't serve.
            "HTTP/1.1 302 Found\r\nLocation: /forbidden?hash=abc\r\n\r\n".into(),
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n".into(),
        ]);
        let swarm = Swarm::new(Some(gateway), None, Some(Duration::from_secs(5))).unwrap();
        assert!(swarm.proxied());
        let url = Url::parse(&format!("bzz://{HASH}/docs")).unwrap();
        let Ok(Response::Document { mime, body }) = swarm.fetch(Route::Proxied, &url) else {
            panic!("a page");
        };
        let parsed = swarm.parse(&url, &mime, &body.unwrap());
        assert_eq!(parsed.title.as_deref(), Some("Swarm"));
        let links: Vec<&str> = parsed.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            links,
            [
                "bzz://fairdataprotocol.eth/".to_string(),
                format!("bzz://{HASH}/about")
            ]
        );
        assert_eq!(swarm.fetch(Route::Proxied, &url), Ok(Response::Gone));
        assert_eq!(swarm.fetch(Route::Proxied, &url), Ok(Response::Gone));
        assert_eq!(
            swarm.fetch(Route::Proxied, &url),
            Err("unreachable (502)".to_string())
        );
        let asked = server.join().unwrap();
        assert_eq!(
            asked,
            [
                format!("GET /bzz/{HASH}/docs HTTP/1.1"),
                format!("GET /bzz/{HASH}/docs/ HTTP/1.1"),
                format!("GET /bzz/{HASH}/docs HTTP/1.1"),
                format!("GET /bzz/{HASH}/docs HTTP/1.1"),
                format!("GET /bzz/{HASH}/docs HTTP/1.1"),
            ]
        );
    }
}
