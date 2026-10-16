//! IPFS: sites and files addressed by what's in them, through an IPFS
//! node's HTTP gateway — Kubo's path gateway, `http://127.0.0.1:8080`, by
//! default; `--gateway` names another (a public one, say).
//!
//! Pages are saved under `ipfs://CID/path` (content that never changes) or
//! `ipns://NAME/path` (a name for the latest version of something: an IPNS
//! key, a DNSLink domain such as `docs.ipfs.tech`, or an ENS name such as
//! `vitalik.eth`; the node looks them up). Every way of writing the same
//! address becomes one: CIDs as CIDv1 in base32 (`bafy…`), keys in base36
//! (`k51…`), and links to gateways (`https://ipfs.io/ipfs/…`,
//! `https://….ipfs.dweb.link/…`) as `ipfs://` links (ARCHITECTURE.md §15.7).

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use lost::urls::Url;

use crate::crawl::{Blacklist, Link, Parsed, Protocol, Response, Route, mentions};
use crate::http::{self, Http};
use crate::i2p::base32;
use crate::net::Proxy;

/// Finding content on the network the first time can take a while.
pub const TIMEOUT: Duration = Duration::from_secs(60);
pub const GATEWAY: &str = "http://127.0.0.1:8080";
/// How long to wait for a node that has just started to find content.
const WARM_UP: Duration = Duration::from_secs(10 * 60);

/// Where a crawl starts: the IPFS project's own sites (DNSLink), and an
/// ENS name's.
const SEEDS: &[&str] = &[
    "ipns://docs.ipfs.tech/",
    "ipns://blog.ipfs.tech/",
    "ipns://specs.ipfs.tech/",
    "ipns://ipfs.tech/",
    "ipns://vitalik.eth/",
];

/// Multicodecs: UnixFS files and folders (what every CIDv0 is), and a
/// public key (an IPNS name).
const DAG_PB: u8 = 0x70;
const LIBP2P_KEY: u64 = 0x72;

const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const BASE36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
/// Longer isn't a CID or a key (and isn't worth the arithmetic).
const MAX_ID: usize = 200;

pub struct Ipfs {
    http: Http,
    /// The gateway's scheme, host and port.
    gateway: Url,
    /// Where it is, when it's reached directly (no `--proxy`).
    address: Option<SocketAddr>,
    /// It's on this computer: requests to it needn't wait for each other.
    local: bool,
}

impl Ipfs {
    pub fn new(
        gateway: Option<String>,
        proxy: Option<Proxy>,
        timeout: Option<Duration>,
    ) -> lost::Result<Ipfs> {
        let text = gateway.unwrap_or_else(|| GATEWAY.to_string());
        let mut gateway = Url::parse(text.trim().trim_end_matches('/'))
            .filter(|u| {
                (u.scheme == "http" || u.scheme == "https") && u.path == "/" && u.query.is_none()
            })
            .ok_or_else(|| {
                format!("--gateway takes http://HOST:PORT or https://HOST (not \"{text}\")")
            })?;
        // Kubo's gateway at `localhost` sends every request on to a
        // subdomain (`CID.ipfs.localhost`); at 127.0.0.1 it answers itself.
        if gateway.host == "localhost" {
            gateway.host = "127.0.0.1".into();
        }
        let ip = gateway
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        let local = proxy.is_none() && ip.is_some_and(|ip| ip.is_loopback());
        let address = match proxy {
            Some(_) => None,
            None => {
                let port = gateway
                    .port
                    .unwrap_or(if gateway.scheme == "https" { 443 } else { 80 });
                let host = gateway.host.trim_start_matches('[').trim_end_matches(']');
                Some(
                    (host, port)
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut a| a.next())
                        .ok_or_else(|| format!("can't find the gateway {}", gateway.host))?,
                )
            }
        };
        Ok(Ipfs {
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

    /// `ipfs://CID/path?query` as the gateway has it: `/ipfs/CID/path?query`.
    fn on_gateway(&self, url: &Url) -> Url {
        let mut at = self.gateway.clone();
        at.path = format!("/{}/{}{}", url.scheme, url.host, url.path);
        at.query = url.query.clone();
        at.fragment = None;
        at
    }

    /// Where a redirect from the gateway goes (`/ipfs/CID/dir/`, or a whole
    /// URL on it), as an `ipfs://` link. Other redirects stay as they are.
    fn redirected(&self, location: &str) -> String {
        let path = Url::parse(location)
            .filter(|u| u.host == self.gateway.host && u.port == self.gateway.port)
            .map(|u| u.path);
        from_path(path.as_deref().unwrap_or(location))
            .or_else(|| canonical(location))
            .unwrap_or_else(|| location.to_string())
    }
}

impl Protocol for Ipfs {
    const NAME: &'static str = "ipfs";
    /// `--link docs.ipfs.tech` is a name: `ipns://docs.ipfs.tech/`.
    const SCHEME: &'static str = "ipns";
    const SPACE: &'static str = "IPFS";
    const HOSTS: &'static str = "sites";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    /// Through a gateway on this computer, every site is a server of its
    /// own; through any other, they all are that one server, asked one
    /// thing at a time (`--delay`), as a public gateway should be.
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
        (url.scheme == "ipfs" || url.scheme == "ipns") && !url.host.is_empty()
    }

    fn rewrite(link: &str) -> Option<String> {
        canonical(link)
    }

    fn fetch(&self, _route: Route, url: &Url) -> Result<Response, String> {
        let answer = self.http.get(self.route(), &self.on_gateway(url))?;
        if answer.status >= 500 && missing_path(&answer.body) {
            // The content is there, but not this path in it (some gateways
            // say so with a 500): not found, like a 404.
            return Ok(Response::Gone);
        }
        if answer.status >= 500 {
            // Not found on the network in time, or a name the node can't
            // look up.
            return Err(format!("not found on the network ({})", answer.status));
        }
        Ok(match http::response(answer) {
            Response::Redirect(location) => Response::Redirect(self.redirected(&location)),
            other => other,
        })
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let mut parsed = http::parse(url, mime, body);
        let written = mentions(&parsed.body, "ipfs")
            .into_iter()
            .chain(mentions(&parsed.body, "ipns"));
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

    /// Fails at once, with a message for people, if the gateway isn't there.
    fn prepare(&self, _blacklist: &mut Blacklist) -> lost::Result<()> {
        if let Some(address) = self.address {
            TcpStream::connect_timeout(&address, Duration::from_secs(5)).map_err(|e| {
                let hint = if self.local {
                    "; is your IPFS node running? (ipfs daemon)"
                } else {
                    ""
                };
                format!("can't reach the IPFS gateway at {address} ({e}){hint}")
            })?;
        }
        // A node finds nothing until it's connected to other nodes, a few
        // seconds to minutes after it starts; until then its gateway answers
        // every request with an error (504, after its own timeout). So the
        // first seed is asked for until it comes.
        let probe = Url::parse(SEEDS[0]).expect("a valid URL");
        let started = Instant::now();
        let mut waiting = false;
        loop {
            let problem = match self.http.get(self.route(), &self.on_gateway(&probe)) {
                Ok(answer) if answer.status < 500 => {
                    if waiting {
                        eprintln!("The IPFS node is finding content.");
                    }
                    return Ok(());
                }
                Ok(answer) => format!(
                    "the gateway answered {}{}",
                    answer.status,
                    reason(&answer.body)
                ),
                Err(e) => e,
            };
            if started.elapsed() >= WARM_UP {
                return Err(format!(
                    "the IPFS node couldn't fetch {} in {} minutes ({problem}); \
                     is it connected to other nodes? (ipfs swarm peers)",
                    SEEDS[0],
                    WARM_UP.as_secs() / 60
                )
                .into());
            }
            if !waiting {
                eprintln!("Waiting for the IPFS node to find content ({problem})…");
                waiting = true;
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    }
}

/// What a gateway's error page says, if anything: `: its first line`.
fn reason(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        Some(line) => format!(": {}", line.chars().take(200).collect::<String>()),
        None => String::new(),
    }
}

/// A gateway's error page about a path that isn't in the content (a
/// robots.txt asked of a file, a page a site doesn't have).
fn missing_path(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    ["no link named", "not a directory", "file does not exist"]
        .iter()
        .any(|w| text.contains(w))
}

/// The address pages of IPFS are saved under, for a link to IPFS written
/// any way: `ipfs://`/`ipns://` with any CID or key, a path on a gateway
/// (`https://ipfs.io/ipfs/CID/…`), or a subdomain gateway
/// (`https://CID.ipfs.dweb.link/…`, `https://docs-ipfs-tech.ipns.dweb.link/…`).
/// Queries (the gateways' `?filename=` and such) and fragments go. `None`
/// for anything else.
pub fn canonical(text: &str) -> Option<String> {
    let text = text.trim();
    let (scheme, rest) = text.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let rest = rest.split('#').next().unwrap_or("");
    match scheme.as_str() {
        "ipfs" | "ipns" => {
            let end = rest.find(['/', '?']).unwrap_or(rest.len());
            let (id, after) = rest.split_at(end);
            let path = after.split('?').next().unwrap_or("");
            // `/ipfs/CID/…` resolved against a page, as if it were a site's
            // root (how gateways' folder listings link).
            from_path(path).or_else(|| address(&scheme, id, path))
        }
        "http" | "https" => {
            let url = Url::parse(text)?;
            let labels: Vec<&str> = url.host.split('.').collect();
            if labels.len() >= 3 && (labels[1] == "ipfs" || labels[1] == "ipns") {
                let id = if labels[1] == "ipns" {
                    inline_dnslink(labels[0])
                } else {
                    labels[0].to_string()
                };
                if let Some(found) = address(labels[1], &id, &url.path) {
                    return Some(found);
                }
            }
            from_path(&url.path)
        }
        _ => None,
    }
}

/// `/ipfs/CID/rest` or `/ipns/NAME/rest`, as an address.
fn from_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let (kind, rest) = rest.split_once('/')?;
    if kind != "ipfs" && kind != "ipns" {
        return None;
    }
    let end = rest.find('/').unwrap_or(rest.len());
    let (id, path) = rest.split_at(end);
    address(kind, id, path)
}

/// `ipfs://CID/path` or `ipns://NAME/path`, written the one way. A CID
/// given as a name is content all the same.
fn address(kind: &str, id: &str, path: &str) -> Option<String> {
    let path = if path.is_empty() { "/" } else { path };
    if kind == "ipns"
        && let Some(name) = name(id)
    {
        return Some(format!("ipns://{name}{path}"));
    }
    content_id(id).map(|cid| format!("ipfs://{cid}{path}"))
}

/// An IPNS name: a key (`k51…`), or a domain (DNSLink, ENS), lowercase.
fn name(text: &str) -> Option<String> {
    if let Some(key) = key(text) {
        return Some(key);
    }
    let domain = text.to_ascii_lowercase();
    let valid = domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    valid.then_some(domain)
}

/// `docs-ipfs-tech` (a DNSLink name made one DNS label, as subdomain
/// gateways write it) as `docs.ipfs.tech`: `-` was `.`, `--` was `-`.
fn inline_dnslink(label: &str) -> String {
    label
        .replace("--", "\0")
        .replace('-', ".")
        .replace('\0', "-")
}

/// A content ID as CIDv1 in base32 (`bafy…`), from any CID: CIDv0 (`Qm…`),
/// or CIDv1 in base32, base36, base58 or hex. Keys aren't content.
fn content_id(text: &str) -> Option<String> {
    let bytes = if is_v0(text) {
        let multihash = base_decode(text, BASE58)?;
        if multihash.len() != 34 || multihash[..2] != [0x12, 0x20] {
            return None;
        }
        let mut bytes = vec![1, DAG_PB];
        bytes.extend(multihash);
        bytes
    } else {
        multibase(text)?
    };
    let (version, rest) = varint(&bytes)?;
    let (codec, multihash) = varint(rest)?;
    if version != 1 || codec == LIBP2P_KEY || !is_multihash(multihash) {
        return None;
    }
    Some(format!("b{}", base32(&bytes)))
}

/// An IPNS key as CIDv1 in base36 (`k51…`), from a peer ID (`12D3Koo…`)
/// or a CIDv1 of a key in any base. (Old RSA peer IDs looked like CIDv0,
/// `Qm…`; those are taken as content, which is what `Qm…` almost always is.)
fn key(text: &str) -> Option<String> {
    let bytes = if text.starts_with("12D3Koo") {
        let multihash = base_decode(text, BASE58)?;
        if !is_multihash(&multihash) {
            return None;
        }
        let mut bytes = vec![1, LIBP2P_KEY as u8];
        bytes.extend(multihash);
        bytes
    } else {
        let bytes = multibase(text)?;
        let (version, rest) = varint(&bytes)?;
        let (codec, multihash) = varint(rest)?;
        if version != 1 || codec != LIBP2P_KEY || !is_multihash(multihash) {
            return None;
        }
        bytes
    };
    Some(format!("k{}", base_encode(&bytes, BASE36)))
}

fn is_v0(text: &str) -> bool {
    text.len() == 46 && text.starts_with("Qm")
}

/// A multihash: the hash's code, its length, and that many bytes.
fn is_multihash(bytes: &[u8]) -> bool {
    let Some((_, rest)) = varint(bytes) else {
        return false;
    };
    let Some((length, digest)) = varint(rest) else {
        return false;
    };
    length > 0 && digest.len() as u64 == length
}

/// The bytes of a multibase string: its first character says the base.
fn multibase(text: &str) -> Option<Vec<u8>> {
    if text.len() > MAX_ID || !text.is_ascii() || text.len() < 2 {
        return None;
    }
    let (prefix, rest) = text.split_at(1);
    match prefix {
        "b" | "B" => base32_decode(&rest.to_ascii_lowercase()),
        "k" | "K" => base_decode(&rest.to_ascii_lowercase(), BASE36),
        "z" => base_decode(rest, BASE58),
        "f" | "F" => hex_decode(rest),
        _ => None,
    }
}

/// An unsigned LEB128 number, and what follows it.
pub(crate) fn varint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for (i, &b) in bytes.iter().enumerate().take(9) {
        value |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((value, &bytes[i + 1..]));
        }
    }
    None
}

/// Text in a base of `alphabet`'s length (base58, base36) as bytes; each
/// leading zero digit is a zero byte.
fn base_decode(text: &str, alphabet: &[u8]) -> Option<Vec<u8>> {
    if text.len() > MAX_ID {
        return None;
    }
    let base = alphabet.len() as u32;
    // Little-endian bytes of the number so far.
    let mut bytes: Vec<u8> = Vec::new();
    for c in text.bytes() {
        let mut carry = alphabet.iter().position(|&a| a == c)? as u32;
        for b in bytes.iter_mut() {
            carry += u32::from(*b) * base;
            *b = carry as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push(carry as u8);
            carry >>= 8;
        }
    }
    let zeros = text.bytes().take_while(|&c| c == alphabet[0]).count();
    bytes.extend(std::iter::repeat_n(0, zeros));
    bytes.reverse();
    Some(bytes)
}

/// Bytes as text in a base of `alphabet`'s length.
fn base_encode(bytes: &[u8], alphabet: &[u8]) -> String {
    let base = alphabet.len() as u32;
    // Little-endian digits of the number so far.
    let mut digits: Vec<u8> = Vec::new();
    for &b in bytes {
        let mut carry = u32::from(b);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % base) as u8;
            carry /= base;
        }
        while carry > 0 {
            digits.push((carry % base) as u8);
            carry /= base;
        }
    }
    let zeros = bytes.iter().take_while(|&&b| b == 0).count();
    std::iter::repeat_n(alphabet[0], zeros)
        .chain(digits.iter().rev().map(|&d| alphabet[d as usize]))
        .map(char::from)
        .collect()
}

/// RFC 4648 base32, lowercase, without padding.
pub(crate) fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut bits, mut count) = (0u32, 0);
    for c in text.bytes() {
        let value = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = bits << 5 | u32::from(value);
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    const V0: &str = "QmbWqxBEKC3P8tqsKc98xmWNzrzDtRLMiMPL8wBuTGsMnR";
    const V1: &str = "bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi";
    const PEER: &str = "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN";
    const KEY: &str = "k51qzi5uqu5dhnwe629wdlncpql6frppdpwnz4wtlcw816aysd5wwlk63g4wmh";

    #[test]
    fn content_ids_and_keys() {
        assert_eq!(content_id(V0).as_deref(), Some(V1));
        assert_eq!(content_id(V1).as_deref(), Some(V1));
        assert_eq!(content_id(&V1.to_ascii_uppercase()).as_deref(), Some(V1));
        assert_eq!(
            content_id("zdj7Wic6KcJAfWz1c9o4M6kq9Lwd5BfbxkVafnrojaaGiSFxM").as_deref(),
            Some(V1)
        );
        assert_eq!(
            content_id("f01701220c3c4733ec8affd06cf9e9ff50ffc6bcd2ec85a6170004bb709669c31de94391a")
                .as_deref(),
            Some(V1)
        );
        // A key isn't content; nonsense is neither.
        assert_eq!(content_id(KEY), None);
        assert_eq!(content_id("QmShort"), None);
        assert_eq!(content_id("hello"), None);
        assert_eq!(content_id(&V0.to_ascii_lowercase()), None);
        assert_eq!(key(PEER).as_deref(), Some(KEY));
        assert_eq!(key(KEY).as_deref(), Some(KEY));
        assert_eq!(key(V1), None);
        // Base58 both ways.
        let bytes = base_decode(V0, BASE58).unwrap();
        assert_eq!(base_encode(&bytes, BASE58), V0);
        assert_eq!(base_encode(&[0, 0, 1], BASE58), "112");
        assert_eq!(base_decode("112", BASE58).unwrap(), [0, 0, 1]);
    }

    #[test]
    fn one_way_to_write_each_address() {
        let c = |s: &str| canonical(s);
        let file = format!("ipfs://{V1}/a/b.html");
        assert_eq!(c(&format!("ipfs://{V0}/a/b.html#top")), Some(file.clone()));
        assert_eq!(c(&format!("IPFS://{V1}/a/b.html?x=1")), Some(file.clone()));
        assert_eq!(
            c(&format!(
                "https://ipfs.io/ipfs/{V0}/a/b.html?filename=b.html"
            )),
            Some(file.clone())
        );
        assert_eq!(
            c(&format!("https://{V1}.ipfs.dweb.link/a/b.html")),
            Some(file)
        );
        assert_eq!(c(&format!("ipfs://{V1}")), Some(format!("ipfs://{V1}/")));
        assert_eq!(
            c("https://en-wikipedia--on--ipfs-org.ipns.dweb.link/wiki/"),
            Some("ipns://en.wikipedia-on-ipfs.org/wiki/".into())
        );
        assert_eq!(
            c("http://127.0.0.1:8080/ipns/docs.ipfs.tech/install/"),
            Some("ipns://docs.ipfs.tech/install/".into())
        );
        assert_eq!(
            c("ipns://Docs.IPFS.tech"),
            Some("ipns://docs.ipfs.tech/".into())
        );
        assert_eq!(c(&format!("ipns://{V1}/x")), Some(format!("ipfs://{V1}/x")));
        assert_eq!(
            c(&format!("ipns://{PEER}/x")),
            Some(format!("ipns://{KEY}/x"))
        );
        // A gateway's folder listing links from the root.
        assert_eq!(
            c(&format!("ipfs://{V1}/ipfs/{V0}/x")),
            Some(format!("ipfs://{V1}/x"))
        );
        // Not IPFS.
        assert_eq!(c("https://example.com/ipfs/notacid"), None);
        assert_eq!(c("https://example.com/page"), None);
        assert_eq!(c("gemini://example.com/"), None);
        assert_eq!(c("ipns://nodots"), None);
    }

    /// A gateway answering one request with `answer`; returns the request.
    fn serve(answer: &'static [u8]) -> (SocketAddr, std::thread::JoinHandle<String>) {
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
            stream.write_all(answer).unwrap();
            request
        });
        (addr, handle)
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn waiting_for_the_node() {
        // The gateway's check, then the first seed, which comes.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (check, _) = listener.accept().unwrap();
            drop(check);
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<p>Docs</p>")
                .unwrap();
            first
        });
        let ipfs = Ipfs::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        ipfs.prepare(&mut Blacklist::default()).unwrap();
        assert_eq!(
            server.join().unwrap(),
            "GET /ipns/docs.ipfs.tech/ HTTP/1.1\r\n"
        );
        assert_eq!(
            reason(b"\n  ipfs resolve -r /ipns/x: context deadline exceeded\nmore"),
            ": ipfs resolve -r /ipns/x: context deadline exceeded"
        );
        assert_eq!(reason(b""), "");
    }

    #[test]
    fn through_the_gateway() {
        let answer: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: /ipfs/{V1}/dir/\r\nContent-Length: 0\r\n\r\n"
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let (addr, server) = serve(answer);
        let ipfs = Ipfs::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        assert!(ipfs.proxied(), "a gateway here: every site its own server");
        let response = ipfs
            .fetch(Route::Proxied, &url(&format!("ipfs://{V1}/dir?x=1")))
            .unwrap();
        assert_eq!(response, Response::Redirect(format!("ipfs://{V1}/dir/")));
        let request = server.join().unwrap();
        assert!(
            request.starts_with(&format!("GET /ipfs/{V1}/dir?x=1 HTTP/1.1\r\n")),
            "{request}"
        );
        assert!(request.contains(&format!("Host: {addr}\r\n")), "{request}");

        // Not found in time: the site can't be reached.
        let (addr, _server) = serve(b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\n\r\n");
        let ipfs = Ipfs::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        assert!(
            ipfs.fetch(Route::Proxied, &url("ipns://docs.ipfs.tech/"))
                .is_err()
        );

        // A path the content doesn't have: not found, not unreachable.
        let (addr, _server) = serve(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\n\
              failed to resolve /ipfs/bafy/robots.txt: no link named \"robots.txt\" under bafy",
        );
        let ipfs = Ipfs::new(
            Some(format!("http://{addr}")),
            None,
            Some(Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(
            ipfs.fetch(Route::Proxied, &url(&format!("ipfs://{V1}/robots.txt"))),
            Ok(Response::Gone)
        );

        // A gateway elsewhere is one server for every site; localhost is
        // asked as 127.0.0.1.
        let remote = Ipfs::new(Some("http://192.0.2.1:8080".into()), None, None).unwrap();
        assert!(!remote.proxied());
        let local = Ipfs::new(Some("http://localhost:8080/".into()), None, None).unwrap();
        assert_eq!(local.gateway.host, "127.0.0.1");
        assert!(Ipfs::new(Some("ftp://x".into()), None, None).is_err());
        assert!(Ipfs::new(Some("http://x.example/path".into()), None, None).is_err());
    }

    #[test]
    fn pages_and_their_links() {
        let ipfs = Ipfs::new(None, None, None).unwrap();
        let html = format!(
            "<title>Home</title><a href=\"about.html\">About</a>\
             <a href=\"/ipfs/{V0}/x\">x</a>\
             <a href=\"https://ipfs.io/ipfs/{V0}/y\">y</a>\
             <a href=\"ipns://docs.ipfs.tech\">docs</a>\
             <a href=\"https://example.com/\">web</a>\
             <p>see ipfs://{V0}/z.</p>"
        );
        let parsed = ipfs.parse(
            &url(&format!("ipfs://{V1}/index.html")),
            "text/html",
            html.as_bytes(),
        );
        assert_eq!(parsed.title.as_deref(), Some("Home"));
        let links: Vec<&str> = parsed.links.iter().map(|l| l.url.as_str()).collect();
        for expected in [
            format!("ipfs://{V1}/about.html"),
            format!("ipfs://{V1}/x"),
            format!("ipfs://{V1}/y"),
            "ipns://docs.ipfs.tech/".to_string(),
            "https://example.com/".to_string(),
            format!("ipfs://{V1}/z"),
        ] {
            assert!(
                links.contains(&expected.as_str()),
                "{expected} in {links:?}"
            );
        }
        assert!(Ipfs::accepts(&url(&format!("ipfs://{V1}/"))));
        assert!(Ipfs::accepts(&url("ipns://docs.ipfs.tech/")));
        assert!(!Ipfs::accepts(&url("https://ipfs.io/")));
    }
}
