//! Arweave, "the permaweb": data stored for good, each piece under its
//! transaction ID (43 characters of base64url), fetched over HTTP from a
//! gateway (`https://turbo-gateway.com` by default; `--gateway` names
//! another, a local ar.io node say).
//!
//! Besides links, Arweave can be listed: a gateway's GraphQL endpoint
//! answers for everything stored, so the crawl also walks through every
//! item tagged as HTML, text, Markdown or a site's manifest, newest first,
//! a hundred at a time, a range of blocks at a time (a gateway won't go
//! through all of Arweave for one answer). Those listings are pages that
//! are followed, not saved, under `ar://graphql.ar/?max=BLOCK&size=BLOCKS`
//! (no name or ID has a dot).
//!
//! Pages are saved under `ar://TXID/path`, or `ar://NAME/path` for ArNS
//! names (`ardrive`, undernames `docs_ardrive`), which the gateway looks up.
//! Links to gateways (`https://arweave.net/TXID`, `https://NAME.ar.io/`)
//! are written that way too (ARCHITECTURE.md §15.8).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

use lost::urls::Url;

use crate::crawl::{Blacklist, Link, Parsed, Protocol, Response, Route, mentions, percent_decode};
use crate::http::{self, Answer, Http};
use crate::net::Proxy;

pub const TIMEOUT: Duration = Duration::from_secs(30);
pub const GATEWAY: &str = "https://turbo-gateway.com";

/// The host of the listing pages.
const LISTING: &str = "graphql.ar";
/// What the listing pages say they are.
const LISTING_TYPE: &str = "application/x-lost-spider-listing";
/// Listed: pages, text, and sites (path manifests: a site's files by path).
const LISTED_TYPES: &[&str] = &[
    "text/html",
    "text/plain",
    "text/markdown",
    "application/x.arweave-manifest+json",
];
const LISTING_SIZE: usize = 100;
/// Blocks listed at a time, to begin with and at most; halved when the
/// gateway finds a range too much to go through, doubled when it's sparse.
const START_BLOCKS: u64 = 20;
const MAX_BLOCKS: u64 = 5_000;

/// Where a crawl starts: the listing of everything (from the newest
/// blocks), and a well-known site.
const SEEDS: &[&str] = &["ar://graphql.ar/", "ar://ardrive/"];

/// Gateways, whose links are links to Arweave: the data's ID first in the
/// path, or a name as the subdomain.
const GATEWAYS: &[&str] = &[
    "arweave.net",
    "arweave.dev",
    "ar-io.net",
    "ar-io.dev",
    "ar.io",
    "turbo-gateway.com",
    "permagate.io",
    "g8way.io",
    "perma.online",
];

pub struct Arweave {
    http: Http,
    /// The gateway's scheme, host and port.
    gateway: Url,
    /// Where it is, for telling servers apart (none through a proxy).
    address: Option<SocketAddr>,
    /// It's on this computer: requests to it needn't wait for each other.
    local: bool,
    /// ArNS names looked up so far: their data's ID, or none.
    names: Mutex<HashMap<String, Option<String>>>,
}

impl Arweave {
    pub fn new(
        gateway: Option<String>,
        proxy: Option<Proxy>,
        timeout: Option<Duration>,
    ) -> lost::Result<Arweave> {
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
        Ok(Arweave {
            http: Http::new(proxy, timeout.unwrap_or(TIMEOUT))?,
            gateway,
            address,
            local,
            names: Mutex::new(HashMap::new()),
        })
    }

    /// Asks the gateway (or one of its subdomains) for `url`: a page, its
    /// API's data (JSON), or a GraphQL `query`.
    fn send(&self, url: &Url, ask: Ask) -> Result<Answer, String> {
        let route = if self.http.proxy().is_some() {
            Route::Proxied
        } else {
            Route::Direct(lookup(url)?)
        };
        match ask {
            Ask::Page => self.http.get(route, url),
            Ask::Data => self.http.get_data(route, url),
            Ask::Query(body) => self.http.post(route, url, "application/json", body),
        }
    }

    /// The newest block's height.
    fn height(&self) -> Result<u64, String> {
        let answer = self.send(&self.at("/info".into()), Ask::Data)?;
        if answer.status != 200 {
            return Err(format!("the gateway's /info answered {}", answer.status));
        }
        serde_json::from_slice::<serde_json::Value>(&answer.body)
            .ok()
            .and_then(|v| v.get("height").and_then(|h| h.as_u64()))
            .ok_or_else(|| "the gateway's /info has no height".to_string())
    }

    fn at(&self, path: String) -> Url {
        let mut url = self.gateway.clone();
        url.path = path;
        url
    }

    /// The data an ArNS name points to, asked of the gateway once a crawl.
    fn look_up(&self, name: &str) -> Result<Option<String>, String> {
        if let Some(found) = self
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            return Ok(found.clone());
        }
        let answer = self.send(&self.at(format!("/ar-io/resolver/{name}")), Ask::Data)?;
        let found = match answer.status {
            200..=299 => serde_json::from_slice::<serde_json::Value>(&answer.body)
                .ok()
                .and_then(|v| v.get("txId").and_then(|t| t.as_str()).map(str::to_string))
                .filter(|id| is_id(id)),
            400..=499 => None,
            status => return Err(format!("can't look up {name} ({status})")),
        };
        self.names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), found.clone());
        Ok(found)
    }

    /// One page of the listing: the items of `size` blocks up to `max`,
    /// after `after`, as the gateway's GraphQL answer. The first, without a
    /// range, goes on to the newest blocks.
    fn listing(&self, url: &Url) -> Result<Response, String> {
        let Some(range) = Range::of(url) else {
            return Ok(Response::Redirect(
                Range {
                    max: self.height()?,
                    size: START_BLOCKS,
                    after: None,
                }
                .url(),
            ));
        };
        let query = format!(
            "query($min: Int, $max: Int, $after: String) {{ transactions(\
             tags: [{{name: \"Content-Type\", values: {}}}], block: {{min: $min, max: $max}}, \
             sort: HEIGHT_DESC, first: {LISTING_SIZE}, after: $after) {{ pageInfo {{ hasNextPage }} \
             edges {{ cursor node {{ id tags {{ name value }} }} }} }} }}",
            serde_json::to_string(LISTED_TYPES).expect("strings")
        );
        let body = serde_json::json!({
            "query": query,
            "variables": { "min": range.min(), "max": range.max, "after": range.after },
        });
        let answer = self.send(
            &self.at("/graphql".into()),
            Ask::Query(body.to_string().as_bytes()),
        )?;
        match answer.status {
            200..=299 => Ok(Response::Document {
                mime: LISTING_TYPE.into(),
                body: Some(answer.body),
            }),
            500.. => Err(format!("the gateway's GraphQL answered {}", answer.status)),
            _ => Ok(Response::Gone),
        }
    }

    /// Where a redirect goes: within the gateway (to a sandbox subdomain
    /// for the data's own origin, say) it's followed here; anything else
    /// is a link, as an `ar://` one if it is one.
    fn follow(&self, mut answer: Answer) -> Result<Response, String> {
        for _ in 0..3 {
            let location = match answer.status {
                301 | 302 | 303 | 307 | 308 => answer.header("location").map(str::to_string),
                _ => None,
            };
            let Some(location) = location else {
                break;
            };
            let target = if location.starts_with('/') {
                let mut url = self.gateway.clone();
                let (path, query) = match location.split_once('?') {
                    Some((p, q)) => (p, Some(q.to_string())),
                    None => (location.as_str(), None),
                };
                url.path = path.to_string();
                url.query = query;
                Some(url)
            } else {
                Url::parse(&location)
            };
            let Some(target) = target else {
                return Ok(Response::Redirect(location));
            };
            let ours = target.host == self.gateway.host
                || target
                    .host
                    .strip_suffix(self.gateway.host.as_str())
                    .is_some_and(|sub| sub.ends_with('.'));
            if !ours {
                return Ok(Response::Redirect(canonical(&location).unwrap_or(location)));
            }
            answer = self.send(&target, Ask::Page)?;
        }
        if answer.status >= 500 {
            return Err(format!("unreachable ({})", answer.status));
        }
        Ok(match http::response(answer) {
            Response::Redirect(location) => {
                Response::Redirect(canonical(&location).unwrap_or(location))
            }
            other => other,
        })
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

impl Protocol for Arweave {
    const NAME: &'static str = "arweave";
    /// `--link ardrive` is a name: `ar://ardrive/`.
    const SCHEME: &'static str = "ar";
    const SPACE: &'static str = "the permaweb";
    const HOSTS: &'static str = "items";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    /// Through a gateway on this computer, every item is a server of its
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
        url.scheme == "ar" && (url.host == LISTING || is_id(&url.host) || is_name(&url.host))
    }

    /// Only names are sites that can have a robots.txt; an ID is one piece
    /// of data, mostly a single file, and the listing is the gateway's.
    fn has_robots(origin: &Url) -> bool {
        is_name(&origin.host)
    }

    fn rewrite(link: &str) -> Option<String> {
        canonical(link)
    }

    fn fetch(&self, _route: Route, url: &Url) -> Result<Response, String> {
        if url.host == LISTING {
            return self.listing(url);
        }
        let id = if is_id(&url.host) {
            url.host.clone()
        } else {
            match self.look_up(&url.host)? {
                Some(id) => id,
                None => return Ok(Response::Gone),
            }
        };
        let path = if url.path == "/" {
            format!("/{id}")
        } else {
            format!("/{id}{}", url.path)
        };
        let answer = self.send(&self.at(path), Ask::Page)?;
        self.follow(answer)
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        if mime == LISTING_TYPE {
            return match Range::of(url) {
                Some(range) => listed(&range, body),
                None => Parsed::default(),
            };
        }
        let mut parsed = http::parse(url, mime, body);
        let written = mentions(&parsed.body, "ar");
        let mut links: Vec<Link> = Vec::new();
        for mut link in std::mem::take(&mut parsed.links).into_iter().chain(written) {
            if let Some(url) = canonical(&link.url) {
                link.url = url;
            }
            if !links.iter().any(|l| l.url == link.url) {
                links.push(link);
            }
        }
        parsed.links = links;
        parsed
    }

    /// The gateway must answer (its newest block), and says how listing goes.
    fn prepare(&self, _blacklist: &mut Blacklist) -> lost::Result<()> {
        let height = self.height().map_err(|e| {
            format!(
                "can't reach the Arweave gateway at {} ({e})",
                self.gateway.host
            )
        })?;
        let first = Range {
            max: height,
            size: START_BLOCKS,
            after: None,
        };
        let probe = Url::parse(&first.url()).expect("a valid URL");
        let problem = match self.listing(&probe) {
            Ok(Response::Document {
                body: Some(body), ..
            }) => graphql_error(&body),
            Ok(_) => Some("no GraphQL".to_string()),
            Err(e) => Some(e),
        };
        match problem {
            None => eprintln!("Listing Arweave from block {height}, newest first."),
            Some(problem) => eprintln!(
                "The gateway at {} can't list Arweave ({problem}): only links will be followed.",
                self.gateway.host
            ),
        }
        Ok(())
    }
}

/// How the gateway is asked.
enum Ask<'a> {
    Page,
    Data,
    Query(&'a [u8]),
}

/// Where a listing page is: blocks `max - size + 1 ..= max`, after a cursor.
#[derive(Debug, Clone, PartialEq)]
struct Range {
    max: u64,
    size: u64,
    after: Option<String>,
}

impl Range {
    /// From `ar://graphql.ar/?max=…&size=…&after=…`; `None` for the start.
    fn of(url: &Url) -> Option<Range> {
        let query = url.query.as_deref()?;
        let value = |name: &str| {
            query
                .split('&')
                .find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
        };
        Some(Range {
            max: value("max")?.parse().ok()?,
            size: value("size")?.parse::<u64>().ok()?.clamp(1, MAX_BLOCKS),
            after: value("after").map(percent_decode),
        })
    }

    fn min(&self) -> u64 {
        (self.max + 1).saturating_sub(self.size).max(1)
    }

    fn url(&self) -> String {
        let mut url = format!("ar://{LISTING}/?max={}&size={}", self.max, self.size);
        if let Some(after) = &self.after {
            url.push_str(&format!("&after={}", percent_encode(after)));
        }
        url
    }
}

/// What a GraphQL answer's error says, if it is one.
fn graphql_error(body: &[u8]) -> Option<String> {
    let Ok(answer) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Some("an answer that isn't JSON".into());
    };
    let errors = answer["errors"].as_array()?;
    let first = errors.first()?;
    let message = first["message"].as_str().unwrap_or("an error");
    Some(message.chars().take(200).collect())
}

/// A page of the listing: its items to crawl (each with its title, if it
/// has one), and where the listing goes on — the next page of this range,
/// or the blocks before it. A range the gateway found too much to go
/// through is asked for again in halves; one with few items is followed by
/// a range twice its size. Not saved.
/// Messages between programs, not pages: AO's (most of what's newest on
/// Arweave, as "text/plain") and SmartWeave's, by their tags.
fn is_message(tags: &[serde_json::Value]) -> bool {
    tags.iter().any(|tag| {
        let name = tag["name"].as_str().unwrap_or_default();
        let value = tag["value"].as_str().unwrap_or_default();
        name.eq_ignore_ascii_case("data-protocol")
            || (name.eq_ignore_ascii_case("app-name")
                && value.to_ascii_lowercase().starts_with("smartweave"))
    })
}

fn listed(range: &Range, body: &[u8]) -> Parsed {
    let mut parsed = Parsed {
        skip: true,
        ..Parsed::default()
    };
    let next = |max: u64, size: u64, after: Option<String>| Link {
        url: Range { max, size, after }.url(),
        label: None,
    };
    if graphql_error(body).is_some() {
        let link = if range.size > 1 {
            next(range.max, range.size / 2, None)
        } else if range.max > 1 {
            // A single block too much: it's skipped.
            next(range.max - 1, 1, None)
        } else {
            return parsed;
        };
        parsed.links.push(link);
        return parsed;
    }
    let Ok(answer) = serde_json::from_slice::<serde_json::Value>(body) else {
        return parsed;
    };
    let transactions = &answer["data"]["transactions"];
    let edges = transactions["edges"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut last = None;
    for edge in &edges {
        last = edge["cursor"].as_str().map(str::to_string);
        let Some(id) = edge["node"]["id"].as_str().filter(|id| is_id(id)) else {
            continue;
        };
        let tags = edge["node"]["tags"].as_array();
        if tags.is_some_and(|tags| is_message(tags)) {
            continue;
        }
        let title = tags.and_then(|tags| {
            tags.iter().find_map(|tag| {
                let name = tag["name"].as_str()?.to_ascii_lowercase();
                (name == "title" || name == "page-title")
                    .then(|| tag["value"].as_str().map(str::to_string))
                    .flatten()
            })
        });
        parsed.links.push(Link {
            url: format!("ar://{id}/"),
            label: title,
        });
    }
    let more = transactions["pageInfo"]["hasNextPage"].as_bool() == Some(true);
    if more && let Some(cursor) = last {
        parsed.links.push(next(range.max, range.size, Some(cursor)));
    } else if range.min() > 1 {
        let size = if edges.len() < LISTING_SIZE / 2 {
            (range.size * 2).min(MAX_BLOCKS)
        } else {
            range.size
        };
        parsed.links.push(next(range.min() - 1, size, None));
    }
    parsed
}

/// The address pages of Arweave are saved under, for a link to Arweave:
/// `ar://ID/…` or `ar://NAME/…` (names lowercase), or a gateway's link to
/// data (`https://arweave.net/ID/…`, `https://….arweave.net/ID/…`) or to a
/// name (`https://NAME.ar.io/…`). Queries and fragments go. `None` for
/// anything else, and for the listing's pages, which are left as they are.
pub fn canonical(text: &str) -> Option<String> {
    let text = text.trim();
    let (scheme, rest) = text.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let rest = rest.split(['#', '?']).next().unwrap_or("");
    let address = |host: &str, path: &str| -> Option<String> {
        let path = if path.is_empty() { "/" } else { path };
        if is_id(host) {
            return Some(format!("ar://{host}{path}"));
        }
        let name = host.to_ascii_lowercase();
        is_name(&name).then(|| format!("ar://{name}{path}"))
    };
    match scheme.as_str() {
        "ar" => {
            let end = rest.find('/').unwrap_or(rest.len());
            let (host, path) = rest.split_at(end);
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
            // `/ID/…`, or `/raw/ID`: the data, whatever the subdomain (a
            // sandbox one, for its own origin).
            let path = url.path.strip_prefix("/raw").unwrap_or(&url.path);
            let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
            if is_id(first) {
                return address(first, &path[1 + first.len()..]);
            }
            // `NAME.gateway`: a name.
            let label = url.host.strip_suffix(*gateway)?.strip_suffix('.')?;
            (!label.contains('.'))
                .then(|| address(label, &url.path))
                .flatten()
        }
        _ => None,
    }
}

/// A transaction or data item ID: 32 bytes in base64url, 43 characters.
fn is_id(text: &str) -> bool {
    text.len() == 43
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An ArNS name (or `undername_name`): lowercase letters, digits, `-` and
/// `_`, at most 51 characters (a gateway's 52-letter sandbox subdomains
/// aren't names).
fn is_name(text: &str) -> bool {
    (1..=51).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !text.starts_with('-')
        && !text.ends_with('-')
        && !is_id(text)
}

fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    const TX: &str = "bNbA3TEQVL60xlgCcqdz4ZPHFZ711cZ3hmkpGttDt_U";

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn one_way_to_write_each_address() {
        let c = |s: &str| canonical(s);
        assert_eq!(c(&format!("ar://{TX}")), Some(format!("ar://{TX}/")));
        assert_eq!(
            c(&format!("ar://{TX}/a.html?x#y")),
            Some(format!("ar://{TX}/a.html"))
        );
        assert_eq!(c("ar://ArDrive/about"), Some("ar://ardrive/about".into()));
        assert_eq!(c("ar://docs_ardrive"), Some("ar://docs_ardrive/".into()));
        assert_eq!(
            c(&format!("https://arweave.net/{TX}")),
            Some(format!("ar://{TX}/"))
        );
        assert_eq!(
            c(&format!("https://arweave.net/raw/{TX}")),
            Some(format!("ar://{TX}/"))
        );
        assert_eq!(
            c(&format!(
                "https://ugs6ntfppsulcv3l6ixq2huelmpsr2b56wxjnmtj6iwhfm6qvdyq.turbo-gateway.com/{TX}/b/c"
            )),
            Some(format!("ar://{TX}/b/c"))
        );
        assert_eq!(
            c("https://ardrive.ar.io/files"),
            Some("ar://ardrive/files".into())
        );
        // Not Arweave.
        assert_eq!(c(&format!("https://example.com/{TX}")), None);
        assert_eq!(c("https://ar.io/"), None);
        assert_eq!(
            c("https://ugs6ntfppsulcv3l6ixq2huelmpsr2b56wxjnmtj6iwhfm6qvdyq.ar.io/"),
            None
        );
        assert_eq!(c("ar://graphql.ar/?after=x"), None);
        assert_eq!(c("ar://Not A Name"), None);
        assert!(Arweave::accepts(&url(&format!("ar://{TX}/"))));
        assert!(Arweave::accepts(&url("ar://ardrive/")));
        assert!(Arweave::accepts(&url("ar://graphql.ar/?after=x")));
        assert!(!Arweave::accepts(&url("ar://some.other/")));
        assert!(Arweave::has_robots(&url("ar://ardrive/")));
        assert!(!Arweave::has_robots(&url(&format!("ar://{TX}/"))));
        assert!(!Arweave::has_robots(&url("ar://graphql.ar/")));
    }

    #[test]
    fn listings() {
        let other = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let ao = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sw = "sssssssssssssssssssssssssssssssssssssssssss";
        let range = Range {
            max: 1000,
            size: 20,
            after: None,
        };
        // Items (not programs' messages), and the next page of the same blocks.
        let body = format!(
            "{{\"data\":{{\"transactions\":{{\"pageInfo\":{{\"hasNextPage\":true}},\"edges\":[\
             {{\"cursor\":\"c1\",\"node\":{{\"id\":\"{TX}\",\"tags\":[{{\"name\":\"Title\",\"value\":\"Hello\"}}]}}}},\
             {{\"cursor\":\"c2\",\"node\":{{\"id\":\"{ao}\",\"tags\":[{{\"name\":\"Data-Protocol\",\"value\":\"ao\"}}]}}}},\
             {{\"cursor\":\"c3\",\"node\":{{\"id\":\"{sw}\",\"tags\":[{{\"name\":\"App-Name\",\"value\":\"SmartWeaveAction\"}}]}}}},\
             {{\"cursor\":\"c2+/=\",\"node\":{{\"id\":\"{other}\",\"tags\":[]}}}}]}}}}}}"
        );
        let parsed = listed(&range, body.as_bytes());
        assert!(parsed.skip);
        assert_eq!(
            parsed.links,
            [
                Link {
                    url: format!("ar://{TX}/"),
                    label: Some("Hello".into())
                },
                Link {
                    url: format!("ar://{other}/"),
                    label: None
                },
                Link {
                    url: "ar://graphql.ar/?max=1000&size=20&after=c2%2B%2F%3D".into(),
                    label: None
                },
            ]
        );
        // The last page of those blocks, sparse: the blocks before, twice as many.
        let last =
            "{\"data\":{\"transactions\":{\"pageInfo\":{\"hasNextPage\":false},\"edges\":[]}}}";
        let links: Vec<String> = listed(&range, last.as_bytes())
            .links
            .into_iter()
            .map(|l| l.url)
            .collect();
        assert_eq!(links, ["ar://graphql.ar/?max=980&size=40"]);
        // Too much for the gateway: half as many blocks; one block, skipped.
        let error = "{\"errors\":[{\"message\":\"Limit for rows or bytes to read exceeded\"}],\"data\":null}";
        let links: Vec<String> = listed(&range, error.as_bytes())
            .links
            .into_iter()
            .map(|l| l.url)
            .collect();
        assert_eq!(links, ["ar://graphql.ar/?max=1000&size=10"]);
        let one = Range {
            max: 1000,
            size: 1,
            after: None,
        };
        let links: Vec<String> = listed(&one, error.as_bytes())
            .links
            .into_iter()
            .map(|l| l.url)
            .collect();
        assert_eq!(links, ["ar://graphql.ar/?max=999&size=1"]);
        assert_eq!(
            graphql_error(error.as_bytes()).as_deref(),
            Some("Limit for rows or bytes to read exceeded")
        );
        assert_eq!(graphql_error(last.as_bytes()), None);
    }

    #[test]
    fn ranges() {
        let range = Range {
            max: 1000,
            size: 20,
            after: Some("c2+/=".into()),
        };
        assert_eq!(range.min(), 981);
        let url = range.url();
        assert_eq!(url, "ar://graphql.ar/?max=1000&size=20&after=c2%2B%2F%3D");
        assert_eq!(Range::of(&Url::parse(&url).unwrap()), Some(range));
        assert_eq!(Range::of(&Url::parse("ar://graphql.ar/").unwrap()), None);
        let start = Range {
            max: 5,
            size: 20,
            after: None,
        };
        assert_eq!(start.min(), 1);
        assert_eq!(
            Range::of(&Url::parse("ar://graphql.ar/?max=9&size=999999").unwrap())
                .unwrap()
                .size,
            MAX_BLOCKS
        );
    }

    /// A gateway answering requests in turn with `answers`; returns the
    /// requests (start line and body).
    fn serve(answers: Vec<String>) -> (SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for answer in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let first = head.lines().next().unwrap_or("").to_string();
                requests.push(format!("{first} {}", String::from_utf8_lossy(&body)));
                stream.write_all(answer.as_bytes()).unwrap();
            }
            requests
        });
        (addr, handle)
    }

    #[test]
    fn redirects() {
        let page = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<title>A</title>";
        // Within the gateway: followed here.
        let (addr, server) = serve(vec![
            format!("HTTP/1.1 302 Found\r\nLocation: /{TX}/about/\r\n\r\n"),
            page.to_string(),
        ]);
        let gateway = Some(format!("http://{addr}"));
        let arweave = Arweave::new(gateway, None, Some(Duration::from_secs(5))).unwrap();
        let response = arweave
            .fetch(Route::Proxied, &url(&format!("ar://{TX}/about")))
            .unwrap();
        assert!(
            matches!(&response, Response::Document { mime, .. } if mime == "text/html"),
            "{response:?}"
        );
        assert_eq!(
            server.join().unwrap(),
            [
                format!("GET /{TX}/about HTTP/1.1 "),
                format!("GET /{TX}/about/ HTTP/1.1 "),
            ]
        );
        // Elsewhere: a link, written the one way.
        let (addr, server) = serve(vec![format!(
            "HTTP/1.1 301 Moved\r\nLocation: https://arweave.net/{TX}/x\r\n\r\n"
        )]);
        let gateway = Some(format!("http://{addr}"));
        let arweave = Arweave::new(gateway, None, Some(Duration::from_secs(5))).unwrap();
        assert_eq!(
            arweave.fetch(Route::Proxied, &url(&format!("ar://{TX}/"))),
            Ok(Response::Redirect(format!("ar://{TX}/x")))
        );
        assert_eq!(server.join().unwrap(), [format!("GET /{TX} HTTP/1.1 ")]);
    }

    /// Needs serde_json (the stand-in build here has none).
    #[test]
    fn through_the_gateway() {
        let page = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<title>A</title>";
        // A name (looked up), then its page, which moved within the gateway.
        let (addr, server) = serve(vec![
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{{\"txId\":\"{TX}\"}}"
            ),
            format!("HTTP/1.1 302 Found\r\nLocation: /{TX}/about/\r\n\r\n"),
            page.to_string(),
        ]);
        let arweave = Arweave::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        assert!(
            arweave.proxied(),
            "a gateway here: every item its own server"
        );
        let response = arweave
            .fetch(Route::Proxied, &url("ar://ardrive/about"))
            .unwrap();
        assert!(
            matches!(&response, Response::Document { mime, .. } if mime == "text/html"),
            "{response:?}"
        );
        let requests = server.join().unwrap();
        assert_eq!(
            requests,
            [
                "GET /ar-io/resolver/ardrive HTTP/1.1 ".to_string(),
                format!("GET /{TX}/about HTTP/1.1 "),
                format!("GET /{TX}/about/ HTTP/1.1 "),
            ]
        );

        // The listing: a GraphQL query for the page after the cursor.
        let (addr, server) = serve(vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"data\":{}}".to_string(),
        ]);
        let arweave = Arweave::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        let response = arweave
            .fetch(
                Route::Proxied,
                &url("ar://graphql.ar/?max=1000&size=20&after=c2%2B"),
            )
            .unwrap();
        assert!(matches!(response, Response::Document { mime, .. } if mime == LISTING_TYPE));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("POST /graphql HTTP/1.1 {"),
            "{requests:?}"
        );
        assert!(requests[0].contains("\"after\":\"c2+\""), "{requests:?}");
        assert!(requests[0].contains("text/html"), "{requests:?}");
        assert!(requests[0].contains("\"min\":981"), "{requests:?}");
        assert!(requests[0].contains("\"max\":1000"), "{requests:?}");
        assert!(
            requests[0].contains("block: {min: $min, max: $max}"),
            "{requests:?}"
        );

        // A gateway elsewhere is one server for every item.
        let remote = Arweave::new(Some("http://192.0.2.1:8080".into()), None, None).unwrap();
        assert!(!remote.proxied());
        assert!(Arweave::new(Some("ftp://x".into()), None, None).is_err());
    }
}
