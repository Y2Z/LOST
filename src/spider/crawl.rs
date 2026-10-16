//! What every spider shares: the queue of URLs, one request at a time per
//! server with a delay between them, robots.txt, the blacklist, the checks
//! against endless generated pages, and turning responses into pages.
//! A protocol (gemini.rs, …) only fetches and parses.

use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use lost::lostfile::Page;
use lost::urls::Url;

pub use crate::net::Route;

/// Requests can't be longer (Gemini's limit; a sane one for the others).
const MAX_URL: usize = 1024;
/// Redirects followed in a row.
const MAX_HOPS: u8 = 5;
/// A host that fails this many requests in a row is left alone for the rest
/// of the crawl.
const MAX_FAILURES: u8 = 3;
/// More path segments than this, or one segment this many times, is a URL
/// that generates itself (a calendar, a loop of relative links, …).
const MAX_SEGMENTS: usize = 30;
const MAX_REPEATS: usize = 3;
/// The names whose robots.txt rules the spider obeys: everyone, the kinds
/// of bot it is (Gemini's "virtual user agents"), and its own.
const AGENTS: &[&str] = &["*", "indexer", "archiver", "lost-spider"];
/// Files that are saved by their link alone, without asking the server.
const FILE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "ico", "tif", "tiff", "heic", "avif", "mp3", "ogg",
    "oga", "opus", "flac", "wav", "m4a", "aac", "mid", "mp4", "webm", "mkv", "avi", "mov", "m4v",
    "pdf", "epub", "mobi", "djvu", "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "tar",
    "iso", "img", "exe", "msi", "deb", "rpm", "apk", "dmg", "jar", "woff", "woff2", "ttf", "otf",
    "psd", "xcf", "sqlite", "db", "bin",
];

/// One network the spider can crawl.
pub trait Protocol: Send + Sync + 'static {
    /// The network's name, as `lost-spider NAME` takes it; also the pages'
    /// source, and part of the file's default name: `gemini`.
    const NAME: &'static str;
    /// The scheme a bare `--link example.org` gets: `gemini`.
    const SCHEME: &'static str;
    /// The network's name, for "Crawling …": `Geminispace`.
    const SPACE: &'static str;
    /// What its hosts are called: `capsules`.
    const HOSTS: &'static str;

    /// Where a crawl starts, besides `--link`.
    fn seeds(&self) -> &'static [&'static str];

    /// Asks the server (at `route`) for `url`. `Err` when the server can't
    /// be reached or stops answering.
    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String>;

    /// A text document as a page.
    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed;

    /// The proxy requests go through, if any (`--proxy`).
    fn proxy(&self) -> Option<&crate::net::Proxy>;

    /// Whether requests go through a proxy. Host names are then never
    /// looked up here, and each host counts as a server of its own.
    fn proxied(&self) -> bool {
        self.proxy().is_some()
    }

    /// Anything to do before crawling, such as fetching a list of sites to
    /// skip (`.onion`).
    fn prepare(&self, _blacklist: &mut Blacklist) -> lost::Result<()> {
        Ok(())
    }

    /// The server's address, when not proxied.
    fn resolve(&self, url: &Url) -> Option<SocketAddr> {
        let host = url.host.trim_start_matches('[').trim_end_matches(']');
        (host, port(url)).to_socket_addrs().ok()?.next()
    }

    /// URLs of this network: by default, those with its scheme.
    fn accepts(url: &Url) -> bool {
        url.scheme == Self::SCHEME
    }

    /// The way pages are saved under for a link written another way (IPFS's
    /// CIDs and gateway links, say); `None` to take it as it is.
    fn rewrite(_link: &str) -> Option<String> {
        None
    }

    /// Where a host's robots.txt is.
    const ROBOTS: &'static str = "/robots.txt";

    /// Whether a host (its `/` URL) can have a robots.txt at all; one that
    /// can't is crawled without asking (Arweave's single files, say).
    fn has_robots(_origin: &Url) -> bool {
        true
    }

    /// What robots.txt rules are matched against: the path and query.
    fn robots_path(url: &Url) -> String {
        match &url.query {
            Some(query) => format!("{}?{query}", url.path),
            None => url.path.clone(),
        }
    }

    /// A file that's saved by its link alone, without asking for it.
    fn is_file(url: &Url) -> bool {
        url.extension()
            .is_some_and(|e| FILE_EXTENSIONS.contains(&e.as_str()))
    }

    /// A title for a page that has none of its own and no link text: the
    /// file's name, or the host.
    fn fallback_title(url: &Url) -> String {
        match url.path.rsplit('/').find(|s| !s.is_empty()) {
            Some(name) => percent_decode(name),
            None => url.host.clone(),
        }
    }

    /// Whether a box to type into (its prompt, the link's text) is a search
    /// box the phrases of `--search` may go into.
    fn is_search(prompt: &str, label: Option<&str>, url: &Url) -> bool {
        looks_like_search(&format!("{prompt} {}", label.unwrap_or("")), url)
    }
}

/// A server's answer, in the terms every protocol shares.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// A document: its type, and its bytes if it's text.
    Document { mime: String, body: Option<Vec<u8>> },
    /// Moved: the new URL, maybe relative.
    Redirect(String),
    /// A box to type into. `sensitive` ones are for passwords.
    Input { prompt: String, sensitive: bool },
    /// Too many requests: wait this long.
    SlowDown(Duration),
    /// Not now; maybe later.
    TryLater,
    /// Not found, not allowed, or an answer that makes no sense.
    Gone,
}

/// A text document, read.
#[derive(Debug, Default)]
pub struct Parsed {
    pub title: Option<String>,
    pub body: String,
    pub links: Vec<Link>,
    /// A list of addresses (an I2P hosts.txt): its links are followed, the
    /// page isn't saved.
    pub skip: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Absolute.
    pub url: String,
    /// The link's text, if it has one.
    pub label: Option<String>,
}

/// A URL's port: its own, or its scheme's.
pub fn port(url: &Url) -> u16 {
    url.port.unwrap_or(match url.scheme.as_str() {
        "gemini" => 1965,
        "gopher" => 70,
        "https" => 443,
        _ => 80,
    })
}

/// `scheme://` URLs written in text, as links without labels; punctuation
/// right after them (`.`, `,`, `)`, …) isn't part of them.
pub fn mentions(text: &str, scheme: &str) -> Vec<Link> {
    let prefix = format!("{scheme}://");
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&prefix) {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "\"'<>[]{}|\\^`".contains(c))
            .unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if url.len() > prefix.len() && Url::parse(url).is_some() {
            links.push(Link {
                url: url.to_string(),
                label: None,
            });
        }
        rest = &tail[end.max(prefix.len())..];
    }
    links
}

// ── Settings ────────────────────────────────────────────────────────────

pub struct Settings {
    /// Between two requests to the same server.
    pub delay: Duration,
    /// Pages saved per host at most.
    pub host_limit: Option<u64>,
    /// Phrases to type into search boxes.
    pub searches: Vec<String>,
    pub blacklist: Blacklist,
    /// `--dont-crawl`: only the `--link` URLs are fetched (and where they
    /// redirect); no link on them is followed.
    pub dont_crawl: bool,
    /// Requests running at once (to different servers).
    pub workers: usize,
}

/// URLs and pages to skip (`--blacklist`), in the format LOST's import
/// blocklist will use (ARCHITECTURE.md §9.2):
///
/// ```text
/// # comment
/// casino.example                 # a domain, with all its subdomains
/// gemini://example.net/spam/     # URLs starting with this
/// /\b(casino|viagra)\b/          # a regular expression on the URL
/// title:/free bitcoin/           # … on the title
/// body:/some phrase/             # … on the text
/// 0.0.0.0 tracker.example        # hosts-file lines work too
/// ```
#[derive(Default)]
pub struct Blacklist {
    domains: HashSet<String>,
    prefixes: Vec<String>,
    urls: Vec<regex::Regex>,
    titles: Vec<regex::Regex>,
    bodies: Vec<regex::Regex>,
    /// Sites known only by the MD5 of their name (`example.onion`), as
    /// Ahmia lists the onion sites it bans.
    hashed: HashSet<[u8; 16]>,
}

impl Blacklist {
    /// Skips the sites whose names (the last two labels: `xyz.onion`) have
    /// these MD5 hashes, hex.
    pub fn ban_hashed(&mut self, hashes: impl IntoIterator<Item = String>) -> usize {
        let before = self.hashed.len();
        for hex in hashes {
            if let Some(hash) = lost::lostfile::from_hex(hex.trim()).and_then(|b| b.try_into().ok())
            {
                self.hashed.insert(hash);
            }
        }
        self.hashed.len() - before
    }

    pub fn load(path: &Path) -> lost::Result<Blacklist> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("can't read {}: {e}", path.display()))?;
        Blacklist::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    pub fn parse(text: &str) -> Result<Blacklist, String> {
        let mut list = Blacklist::default();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            let regex = |pattern: &str| {
                regex::Regex::new(pattern).map_err(|e| format!("line {}: {e}", n + 1))
            };
            if let Some(pattern) = line.strip_prefix("title:/").and_then(regex_body) {
                list.titles.push(regex(pattern)?);
            } else if let Some(pattern) = line.strip_prefix("body:/").and_then(regex_body) {
                list.bodies.push(regex(pattern)?);
            } else if let Some(pattern) = line.strip_prefix('/').and_then(regex_body) {
                list.urls.push(regex(pattern)?);
            } else {
                let line = without_comment(line);
                let mut words = line.split_whitespace();
                let Some(first) = words.next() else { continue };
                if first.contains("://") {
                    // Written the way the spider writes URLs, so they compare.
                    let prefix = match Url::parse(first) {
                        Some(url) if first.ends_with('/') || url.path != "/" => url.to_string(),
                        Some(url) => url.to_string().trim_end_matches('/').to_string(),
                        None => first.to_string(),
                    };
                    list.prefixes.push(prefix);
                } else {
                    let domain = match words.next() {
                        Some(second) if first.parse::<IpAddr>().is_ok() => second,
                        _ => first,
                    };
                    // As written and in punycode, so either spelling matches.
                    let domain = domain.trim_end_matches('.').to_lowercase();
                    if let Some(ascii) = lost::urls::ascii_host(&domain) {
                        list.domains.insert(ascii);
                    }
                    list.domains.insert(domain);
                }
            }
        }
        Ok(list)
    }

    fn blocks_url(&self, url: &Url, text: &str) -> bool {
        let ascii = lost::urls::ascii_host(&url.host);
        if !self.hashed.is_empty() {
            let host = ascii.as_deref().unwrap_or(&url.host);
            let labels: Vec<&str> = host.rsplitn(3, '.').collect();
            let site = match labels.as_slice() {
                [tld, name, ..] => format!("{name}.{tld}"),
                _ => host.to_string(),
            };
            if self.hashed.contains(&md5(site.as_bytes())) {
                return true;
            }
        }
        for name in [Some(url.host.as_str()), ascii.as_deref()]
            .into_iter()
            .flatten()
        {
            let mut host = name;
            loop {
                if self.domains.contains(host) {
                    return true;
                }
                match host.split_once('.') {
                    Some((_, parent)) => host = parent,
                    None => break,
                }
            }
        }
        self.prefixes.iter().any(|p| text.starts_with(p.as_str()))
            || self.urls.iter().any(|r| r.is_match(text))
    }

    fn blocks_page(&self, title: Option<&str>, body: &str) -> bool {
        title.is_some_and(|t| self.titles.iter().any(|r| r.is_match(t)))
            || self.bodies.iter().any(|r| r.is_match(body))
    }
}

/// `pattern/   # comment` → `pattern`: the regular expression ends at the
/// last `/` that only has a comment (or nothing) after it.
fn regex_body(rest: &str) -> Option<&str> {
    rest.match_indices('/').rev().find_map(|(i, _)| {
        let after = rest[i + 1..].trim();
        (after.is_empty() || after.starts_with('#')).then(|| &rest[..i])
    })
}

fn without_comment(line: &str) -> &str {
    if line.starts_with('#') {
        return "";
    }
    match line.find(" #").or_else(|| line.find("\t#")) {
        Some(i) => &line[..i],
        None => line,
    }
}

/// MD5 (RFC 1321): only to match lists published as MD5 hashes.
pub fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for block in message.chunks(64) {
        let m: Vec<u32> = block
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, word) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

// ── robots.txt ──────────────────────────────────────────────────────────

/// A host's robots.txt, as far as it concerns the spider: the rules of every
/// group naming one of [`AGENTS`]. The longest matching rule decides
/// (`Allow` if two are as long); `*` matches anything and `$` the end.
#[derive(Debug, Clone, Default)]
pub struct Robots {
    rules: Vec<(bool, String)>,
}

impl Robots {
    pub fn parse(text: &str) -> Robots {
        let mut rules = Vec::new();
        let mut in_agents = false;
        let mut applies = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match field.trim().to_ascii_lowercase().as_str() {
                "user-agent" => {
                    if !in_agents {
                        in_agents = true;
                        applies = false;
                    }
                    applies |= AGENTS.iter().any(|a| a.eq_ignore_ascii_case(value));
                }
                rule @ ("allow" | "disallow") => {
                    in_agents = false;
                    if applies && !value.is_empty() {
                        rules.push((rule == "allow", value.to_string()));
                    }
                }
                _ => {}
            }
        }
        Robots { rules }
    }

    /// Whether `path` (see [`Protocol::robots_path`]) may be fetched.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if glob(pattern, path) {
                let len = pattern.len();
                if best.is_none_or(|(l, a)| len > l || (len == l && *allow && !a)) {
                    best = Some((len, *allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Does `path` start with `pattern`, where `*` is anything and a final `$`
/// means "and ends there"?
fn glob(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some(mut rest) = path.strip_prefix(parts[0]) else {
        return false;
    };
    for (i, part) in parts.iter().enumerate().skip(1) {
        if anchored && i == parts.len() - 1 {
            return rest.ends_with(part);
        }
        match rest.find(part) {
            Some(j) => rest = &rest[j + part.len()..],
            None => return false,
        }
    }
    !anchored || rest.is_empty()
}

// ── URLs ────────────────────────────────────────────────────────────────

/// Spaces, controls and non-ASCII characters in the path and query,
/// percent-encoded, as they're sent. An internationalized host stays as it's
/// written (lowercased), so stored URLs read well; it's sent in punycode
/// ([`on_the_wire`]).
fn clean(mut url: Url) -> Url {
    fn encode(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if c.is_ascii_graphic() {
                out.push(c);
            } else {
                let mut bytes = [0u8; 4];
                for b in c.encode_utf8(&mut bytes).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
        out
    }
    if !url.host.is_ascii() {
        url.host = url.host.to_lowercase();
    }
    url.path = encode(&url.path);
    url.query = url.query.as_deref().map(encode);
    url.fragment = None;
    url
}

/// What a search box is sent: everything but letters, digits and `-._~`
/// percent-encoded.
fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `%XX` escapes decoded; bytes that aren't UTF-8 replaced.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A URL that keeps generating more of itself.
fn looks_like_trap(url: &Url) -> bool {
    let segments: Vec<&str> = url.path.split('/').filter(|s| !s.is_empty()).collect();
    segments.len() > MAX_SEGMENTS
        || segments
            .iter()
            .any(|s| segments.iter().filter(|t| *t == s).count() >= MAX_REPEATS)
}

/// The prompt of a box to type into looks like it's for searching, not for
/// posting: guestbooks, comments and replies look the same in Gemini, and
/// the spider never writes anything anywhere.
pub fn looks_like_search(prompt: &str, url: &Url) -> bool {
    const SEARCH: &[&str] = &["search", "query", "find", "look up", "lookup", "keyword"];
    let text = format!("{prompt} {}", url.path).to_lowercase();
    SEARCH.iter().any(|w| text.contains(w)) && !looks_like_posting(&text)
}

/// Words of boxes for writing something (a comment, a guestbook entry, a
/// name, a password), which the spider never types into.
pub fn looks_like_posting(text: &str) -> bool {
    const NOT: &[&str] = &[
        "comment",
        "guestbook",
        "guest book",
        "message",
        "reply",
        "post",
        "password",
        "passphrase",
        "sign",
        "say",
        "write",
        "name",
        "mail",
        "token",
        "captcha",
        "secret",
    ];
    let text = text.to_lowercase();
    NOT.iter().any(|w| text.contains(w))
}

/// A host, as robots.txt and politeness see it: `scheme://host`, with
/// `:port` when it isn't the scheme's (so `http://` and `https://` on one
/// name are two, each with its own robots.txt); punycode for an
/// internationalized host, so both spellings are one host.
pub fn host_key(url: &Url) -> String {
    let host = lost::urls::wire_host(url).unwrap_or_else(|| url.host.clone());
    match url.port {
        Some(port) => format!("{}://{host}:{port}", url.scheme),
        None => format!("{}://{host}", url.scheme),
    }
}

/// The URL as it's sent: its host in punycode (but an Arweave ID or an IPFS
/// CID as it is: lowercased, it's another one, or none).
fn on_the_wire(url: &Url) -> Url {
    let mut url = url.clone();
    if let Some(ascii) = lost::urls::wire_host(&url) {
        url.host = ascii;
    }
    url
}

fn hash(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// URLs that are the same page to LOST are fetched once.
fn seen_key(url: &Url) -> u64 {
    hash(&on_the_wire(url).normalized())
}

/// A title for a page that has none of its own: the link's text, or what
/// the protocol makes of the URL.
fn fallback_title(url: &Url, label: Option<&str>, untitled: fn(&Url) -> String) -> String {
    match label {
        Some(label) => label.to_string(),
        None => untitled(url),
    }
}

// ── What's already in the file ──────────────────────────────────────────

/// What a crawl being continued (`-c`) already has: its pages' URLs, texts
/// and links. The queue is rebuilt from the links (ARCHITECTURE.md §15.3).
#[derive(Default)]
pub struct Known {
    seen: HashSet<u64>,
    contents: HashSet<u64>,
    saved: HashMap<String, u64>,
    links: Vec<String>,
}

impl Known {
    /// A page of the file; `accepts` says which URLs are the network's.
    pub fn add(&mut self, page: Page, accepts: fn(&Url) -> bool) {
        let Some(url) = Url::parse(&page.url) else {
            return;
        };
        self.seen.insert(seen_key(&url));
        if !page.body.is_empty() {
            self.contents.insert(hash(&page.body));
        }
        if accepts(&url) {
            *self.saved.entry(host_key(&url)).or_default() += 1;
        }
        self.links.extend(page.links);
    }
}

// ── The crawl ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// A host's robots.txt, asked for before anything else there.
    Robots,
    /// A page to save.
    Page,
    /// A search's results: their links are followed, the page isn't saved.
    Results,
}

#[derive(Debug, Clone)]
struct Job {
    url: Url,
    label: Option<String>,
    kind: Kind,
    tries: u8,
    hops: u8,
}

/// A host (and port).
struct Host {
    origin: Url,
    queue: VecDeque<Job>,
    robots: Option<Robots>,
    /// How it's reached, once known.
    route: Option<Route>,
    /// Its server's key: the IP address, or the host itself when proxied.
    server: String,
    /// In its server's round of hosts.
    listed: bool,
    failures: u8,
    /// Unreachable, or out of `--host-limit`: nothing more is asked of it.
    done: bool,
    saved: u64,
}

/// One IP address (or, through a proxy, one host): one request at a time,
/// `delay` apart.
struct Server {
    hosts: VecDeque<String>,
    busy: bool,
    /// In `ready`.
    scheduled: bool,
    next: Instant,
}

struct State {
    /// The protocol's.
    name: &'static str,
    accepts: fn(&Url) -> bool,
    proxied: bool,
    robots_url: &'static str,
    has_robots: fn(&Url) -> bool,
    robots_path: fn(&Url) -> String,
    is_file: fn(&Url) -> bool,
    untitled: fn(&Url) -> String,
    hosts: HashMap<String, Host>,
    servers: HashMap<String, Server>,
    /// Servers with something to ask, by when they may be asked.
    ready: BinaryHeap<Reverse<(Instant, u64, String)>>,
    order: u64,
    unresolved: VecDeque<String>,
    seen: HashSet<u64>,
    contents: HashSet<u64>,
    saved_before: HashMap<String, u64>,
    /// Jobs queued.
    waiting: u64,
    /// Tasks being worked on.
    active: usize,
    /// Of which, host names being looked up.
    resolving: usize,
    skipped: Skipped,
}

enum Task {
    Resolve(String, Url),
    /// A file saved by its link alone.
    Save(Page),
    Fetch {
        server: String,
        key: String,
        route: Route,
        job: Job,
    },
}

enum Next {
    Task(Box<Task>),
    Wait(Option<Instant>),
    Done,
}

/// What a fetch came to, worked out without holding the lock.
enum Outcome {
    Robots(Robots),
    Page {
        page: Page,
        content: Option<u64>,
        blocked: bool,
        links: Vec<Link>,
    },
    Follow(Vec<Link>),
    Redirect(Url),
    Search(Vec<Url>),
    SlowDown(Duration),
    Retry,
    Nothing,
    /// Not there (any more).
    Gone,
    Failed,
}

fn schedule(
    server: &mut Server,
    name: &str,
    ready: &mut BinaryHeap<Reverse<(Instant, u64, String)>>,
    order: &mut u64,
) {
    if !server.busy && !server.scheduled && !server.hosts.is_empty() {
        *order += 1;
        ready.push(Reverse((server.next, *order, name.to_string())));
        server.scheduled = true;
    }
}

fn new_server() -> Server {
    Server {
        hosts: VecDeque::new(),
        busy: false,
        scheduled: false,
        next: Instant::now(),
    }
}

impl State {
    fn enqueue(
        &mut self,
        url: Url,
        label: Option<String>,
        kind: Kind,
        hops: u8,
        settings: &Settings,
    ) -> bool {
        if !(self.accepts)(&url) || lost::urls::wire_host(&url).is_none() {
            return false;
        }
        let url = clean(url);
        let text = url.to_string();
        if text.len() > MAX_URL
            || looks_like_trap(&url)
            || settings.blacklist.blocks_url(&url, &text)
        {
            return false;
        }
        let key = host_key(&url);
        if !self.seen.insert(seen_key(&url)) {
            return false;
        }
        let host = match self.hosts.entry(key.clone()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                // Asked in punycode: looked up, robots.txt.
                let mut origin = on_the_wire(&url);
                origin.path = "/".into();
                origin.query = None;
                let mut robots = origin.clone();
                robots.path = self.robots_url.into();
                let saved = self.saved_before.get(&key).copied().unwrap_or(0);
                let done = settings.host_limit.is_some_and(|limit| saved >= limit);
                let mut queue = VecDeque::new();
                // Through a proxy, nothing is looked up: the host is its
                // own server.
                let (route, server) = if self.proxied {
                    self.servers.entry(key.clone()).or_insert_with(new_server);
                    (Some(Route::Proxied), key.clone())
                } else {
                    (None, String::new())
                };
                let wants_robots = (self.has_robots)(&origin);
                if !done {
                    if wants_robots {
                        queue.push_back(Job {
                            url: robots,
                            label: None,
                            kind: Kind::Robots,
                            tries: 0,
                            hops: 0,
                        });
                        self.waiting += 1;
                    }
                    if route.is_none() {
                        self.unresolved.push_back(key.clone());
                    }
                }
                e.insert(Host {
                    origin,
                    queue,
                    // None to ask for: no rules.
                    robots: (!wants_robots).then(Robots::default),
                    route,
                    server,
                    listed: false,
                    failures: 0,
                    done,
                    saved,
                })
            }
        };
        if host.done {
            return false;
        }
        host.queue.push_back(Job {
            url,
            label,
            kind,
            tries: 0,
            hops,
        });
        self.waiting += 1;
        if host.route.is_some() && !host.listed {
            host.listed = true;
            let server = self
                .servers
                .get_mut(&host.server)
                .expect("a reachable host has a server");
            server.hosts.push_back(key);
            schedule(server, &host.server, &mut self.ready, &mut self.order);
        }
        true
    }

    /// Puts a job back in its host's queue (it's been seen already).
    fn requeue(&mut self, key: &str, job: Job, front: bool) {
        let Some(host) = self.hosts.get_mut(key) else {
            return;
        };
        if host.done {
            return;
        }
        if front {
            host.queue.push_front(job);
        } else {
            host.queue.push_back(job);
        }
        self.waiting += 1;
        if host.route.is_some() && !host.listed {
            host.listed = true;
            if let Some(server) = self.servers.get_mut(&host.server) {
                server.hosts.push_back(key.to_string());
            }
        }
    }

    /// Nothing more is asked of this host.
    fn retire(&mut self, key: &str) {
        if let Some(host) = self.hosts.get_mut(key) {
            host.done = true;
            self.waiting -= host.queue.len() as u64;
            host.queue.clear();
        }
    }

    fn next(&mut self, now: Instant, settings: &Settings) -> Next {
        // At most half the workers look up host names while there are
        // servers to ask: a crawl that starts with thousands of new hosts
        // (as a continued one does) mustn't spend its first minutes only
        // looking them up, many of them dead.
        if self.resolving < settings.workers.div_ceil(2)
            && let Some(task) = self.resolve_task()
        {
            return task;
        }
        let wait = match self.fetch_task(now, settings) {
            Ok(task) => return task,
            Err(wait) => wait,
        };
        // No server to ask right now: an idle worker may as well look one up.
        if let Some(task) = self.resolve_task() {
            return task;
        }
        // Done only when nothing runs, nothing waits to be looked up, and no
        // server waits out its delay with something still to ask.
        if self.active == 0 && self.unresolved.is_empty() && self.ready.is_empty() {
            Next::Done
        } else {
            Next::Wait(wait)
        }
    }

    fn resolve_task(&mut self) -> Option<Next> {
        let key = self.unresolved.pop_front()?;
        let origin = self.hosts[&key].origin.clone();
        self.active += 1;
        self.resolving += 1;
        Some(Next::Task(Box::new(Task::Resolve(key, origin))))
    }

    /// The next request (or file saved by its link) a server is ready for;
    /// otherwise when one will be, if one has anything to ask.
    fn fetch_task(&mut self, now: Instant, settings: &Settings) -> Result<Next, Option<Instant>> {
        while let Some(Reverse((at, _, _))) = self.ready.peek() {
            if *at > now {
                return Err(Some(*at));
            }
            let Some(Reverse((_, _, name))) = self.ready.pop() else {
                break;
            };
            let Some(server) = self.servers.get_mut(&name) else {
                continue;
            };
            server.scheduled = false;
            while let Some(key) = server.hosts.pop_front() {
                let host = self.hosts.get_mut(&key).expect("listed hosts exist");
                let Some(job) = host.queue.pop_front() else {
                    host.listed = false;
                    continue;
                };
                self.waiting -= 1;
                if host.queue.is_empty() {
                    host.listed = false;
                } else {
                    server.hosts.push_back(key.clone());
                }
                if job.kind != Kind::Robots {
                    // No rules read (yet): nothing is asked without them.
                    let path = (self.robots_path)(&job.url);
                    if host.robots.as_ref().is_none_or(|r| !r.allows(&path)) {
                        continue;
                    }
                    if settings.host_limit.is_some_and(|limit| host.saved >= limit) {
                        host.done = true;
                        self.waiting -= host.queue.len() as u64;
                        host.queue.clear();
                        continue;
                    }
                    if job.kind == Kind::Page && (self.is_file)(&job.url) {
                        let title = fallback_title(&job.url, job.label.as_deref(), self.untitled);
                        if settings.blacklist.blocks_page(Some(&title), "") {
                            continue;
                        }
                        host.saved += 1;
                        schedule(server, &name, &mut self.ready, &mut self.order);
                        self.active += 1;
                        return Ok(Next::Task(Box::new(Task::Save(Page {
                            title: Some(title),
                            url: job.url.to_string(),
                            sources: vec![self.name.to_string()],
                            crawled: Some(lost::store::now()),
                            ..Page::default()
                        }))));
                    }
                }
                let route = host.route.expect("listed hosts are reachable");
                server.busy = true;
                self.active += 1;
                return Ok(Next::Task(Box::new(Task::Fetch {
                    server: name,
                    key,
                    route,
                    job,
                })));
            }
        }
        Err(None)
    }

    fn resolved(&mut self, key: &str, addr: Option<SocketAddr>) {
        self.active -= 1;
        self.resolving -= 1;
        let Some(addr) = addr else {
            self.retire(key);
            return;
        };
        let name = addr.ip().to_string();
        let host = self.hosts.get_mut(key).expect("resolved hosts exist");
        host.route = Some(Route::Direct(addr));
        host.server = name.clone();
        if host.done || host.queue.is_empty() {
            return;
        }
        host.listed = true;
        let server = self.servers.entry(name.clone()).or_insert_with(new_server);
        server.hosts.push_back(key.to_string());
        schedule(server, &name, &mut self.ready, &mut self.order);
    }

    /// Takes in what a fetch came to; returns the page to save, if any.
    fn fetched(
        &mut self,
        server: &str,
        key: &str,
        job: Job,
        outcome: Outcome,
        settings: &Settings,
    ) -> Option<Page> {
        self.active -= 1;
        let now = Instant::now();
        let mut next = now + settings.delay;
        let mut page_out = None;

        let failed = matches!(outcome, Outcome::Failed);
        if let Some(host) = self.hosts.get_mut(key) {
            host.failures = if failed { host.failures + 1 } else { 0 };
        }
        match outcome {
            Outcome::Failed => {
                let failures = self
                    .hosts
                    .get(key)
                    .map_or(MAX_FAILURES, |host| host.failures);
                if failures >= MAX_FAILURES {
                    self.retire(key);
                    self.skipped.unreachable += 1;
                } else if job.kind == Kind::Robots {
                    // Asked again before anything else: nothing is fetched
                    // there without its rules.
                    self.requeue(key, job, true);
                } else if job.tries == 0 {
                    self.requeue(key, Job { tries: 1, ..job }, false);
                } else {
                    self.skipped.unreachable += 1;
                }
            }
            Outcome::Robots(robots) => {
                if let Some(host) = self.hosts.get_mut(key) {
                    host.robots = Some(robots);
                }
            }
            Outcome::Page {
                page,
                content,
                blocked,
                links,
            } => {
                let new = !blocked && content.is_none_or(|c| self.contents.insert(c));
                if blocked {
                    self.skipped.blocked += 1;
                } else if !new {
                    self.skipped.copies += 1;
                }
                if new {
                    if let Some(host) = self.hosts.get_mut(key) {
                        host.saved += 1;
                    }
                    if !settings.dont_crawl {
                        for link in links {
                            if let Some(url) = Url::parse(&link.url) {
                                self.enqueue(url, link.label, Kind::Page, 0, settings);
                            }
                        }
                    }
                    page_out = Some(page);
                }
            }
            Outcome::Follow(links) if !settings.dont_crawl => {
                for link in links {
                    if let Some(url) = Url::parse(&link.url) {
                        self.enqueue(url, link.label, Kind::Page, 0, settings);
                    }
                }
            }
            Outcome::Redirect(url) => {
                if job.hops < MAX_HOPS {
                    // A redirect to a spelling of the same URL (`/dir` to
                    // `/dir/`, a host to its `/`) counts as seen already:
                    // let it through, the hop limit stops loops.
                    let target = seen_key(&url);
                    if target == seen_key(&job.url) {
                        self.seen.remove(&target);
                    }
                    self.enqueue(url, job.label, job.kind, job.hops + 1, settings);
                }
            }
            Outcome::Search(urls) => {
                // With --dont-crawl, a search's results are what was asked for:
                // saved, not followed.
                let kind = if settings.dont_crawl {
                    Kind::Page
                } else {
                    Kind::Results
                };
                for url in urls {
                    self.enqueue(url, None, kind, 0, settings);
                }
            }
            Outcome::Follow(_) => {}
            Outcome::SlowDown(wait) => {
                next = now + wait.max(settings.delay);
                if job.tries >= 3 && job.kind == Kind::Robots {
                    self.retire(key);
                } else if job.tries < 3 {
                    self.requeue(
                        key,
                        Job {
                            tries: job.tries + 1,
                            ..job
                        },
                        true,
                    );
                }
            }
            Outcome::Retry => {
                let robots = job.kind == Kind::Robots;
                if job.tries < 2 {
                    self.requeue(
                        key,
                        Job {
                            tries: job.tries + 1,
                            ..job
                        },
                        robots,
                    );
                } else if robots {
                    // Its rules never came: nothing more is asked there.
                    self.retire(key);
                }
            }
            Outcome::Gone => self.skipped.gone += 1,
            Outcome::Nothing => {}
        }

        if let Some(entry) = self.servers.get_mut(server) {
            entry.busy = false;
            entry.next = next;
            schedule(entry, server, &mut self.ready, &mut self.order);
        }
        page_out
    }
}

/// How a crawl is going, for the status line.
pub struct Stats {
    pub hosts: usize,
    pub waiting: u64,
    pub skipped: Skipped,
}

/// Pages fetched (or tried) but not saved, by why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// The same text as a page saved already.
    pub copies: u64,
    /// Not found, or not there any more.
    pub gone: u64,
    /// No answer, or errors only, after trying again.
    pub unreachable: u64,
    /// Kept out by the blacklist.
    pub blocked: u64,
}

impl Skipped {
    pub fn total(&self) -> u64 {
        self.copies + self.gone + self.unreachable + self.blocked
    }
}

pub struct Crawler<P: Protocol> {
    protocol: P,
    settings: Settings,
    state: Mutex<State>,
    wake: Condvar,
    stop: AtomicBool,
}

impl<P: Protocol> Crawler<P> {
    /// A crawl starting at `links` (first), the protocol's seeds and the
    /// links of the pages it `known`s already.
    pub fn new(protocol: P, settings: Settings, known: Known, links: &[String]) -> Arc<Crawler<P>> {
        let mut state = State {
            name: P::NAME,
            accepts: P::accepts,
            proxied: protocol.proxied(),
            robots_url: P::ROBOTS,
            has_robots: P::has_robots,
            robots_path: P::robots_path,
            is_file: P::is_file,
            untitled: P::fallback_title,
            hosts: HashMap::new(),
            servers: HashMap::new(),
            ready: BinaryHeap::new(),
            order: 0,
            unresolved: VecDeque::new(),
            seen: known.seen,
            contents: known.contents,
            saved_before: known.saved,
            waiting: 0,
            active: 0,
            resolving: 0,
            skipped: Skipped::default(),
        };
        // With --dont-crawl, the links are all there is to fetch: no seeds,
        // and nothing from the file's pages.
        let seeds: &[&str] = if settings.dont_crawl {
            &[]
        } else {
            protocol.seeds()
        };
        let known_links: &[String] = if settings.dont_crawl {
            &[]
        } else {
            &known.links
        };
        let start = links
            .iter()
            .map(String::as_str)
            .chain(seeds.iter().copied())
            // Newest first: links from the file's last pages are where the
            // crawl stopped. Older ones still not crawled are mostly ones
            // that failed before (dead hosts, errors), so they come last.
            .chain(known_links.iter().rev().map(String::as_str));
        for link in start {
            if let Some(url) = Url::parse(link) {
                state.enqueue(url, None, Kind::Page, 0, &settings);
            }
        }
        Arc::new(Crawler {
            protocol,
            settings,
            state: Mutex::new(state),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    /// Starts the workers. Pages go to `out`; when the crawl is over (or
    /// stopped), every worker has dropped its copy of it.
    pub fn start(self: &Arc<Self>, out: SyncSender<Page>) {
        for _ in 0..self.settings.workers.max(1) {
            let crawler = self.clone();
            let out = out.clone();
            std::thread::spawn(move || crawler.work(out));
        }
    }

    /// Asks the workers to stop; requests under way are left to time out.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
    }

    pub fn stats(&self) -> Stats {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Stats {
            hosts: state.hosts.len(),
            waiting: state.waiting,
            skipped: state.skipped,
        }
    }

    fn work(&self, out: SyncSender<Page>) {
        loop {
            let task = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    match state.next(now, &self.settings) {
                        Next::Task(task) => break task,
                        Next::Done => {
                            self.wake.notify_all();
                            return;
                        }
                        Next::Wait(until) => {
                            let wait = until
                                .map_or(Duration::from_secs(1), |t| t - now)
                                .min(Duration::from_secs(1));
                            state = self
                                .wake
                                .wait_timeout(state, wait)
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                    }
                }
            };
            match *task {
                Task::Resolve(key, origin) => {
                    let addr = self.protocol.resolve(&origin);
                    self.state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .resolved(&key, addr);
                }
                Task::Save(page) => {
                    self.state.lock().unwrap_or_else(|e| e.into_inner()).active -= 1;
                    if out.send(page).is_err() {
                        return;
                    }
                }
                Task::Fetch {
                    server,
                    key,
                    route,
                    job,
                } => {
                    let response = self.protocol.fetch(route, &on_the_wire(&job.url));
                    let outcome = self.outcome(&job, response);
                    let page = self
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .fetched(&server, &key, job, outcome, &self.settings);
                    if let Some(page) = page
                        && out.send(page).is_err()
                    {
                        return;
                    }
                }
            }
            self.wake.notify_all();
        }
    }

    fn outcome(&self, job: &Job, response: Result<Response, String>) -> Outcome {
        let Ok(response) = response else {
            return Outcome::Failed;
        };
        if job.kind == Kind::Robots {
            return match response {
                Response::Document {
                    body: Some(body), ..
                } => Outcome::Robots(Robots::parse(&String::from_utf8_lossy(&body))),
                // Not now: asked again first, nothing else meanwhile.
                Response::TryLater => Outcome::Retry,
                Response::SlowDown(wait) => Outcome::SlowDown(wait),
                // None there (not found, moved, not text…): no rules.
                _ => Outcome::Robots(Robots::default()),
            };
        }
        match response {
            Response::Document {
                mime,
                body: Some(body),
            } => {
                let parsed = self.protocol.parse(&job.url, &mime, &body);
                if job.kind == Kind::Results || parsed.skip {
                    // Followed, not saved — unless the blacklist says no.
                    if self
                        .settings
                        .blacklist
                        .blocks_page(parsed.title.as_deref(), &parsed.body)
                    {
                        return Outcome::Nothing;
                    }
                    return Outcome::Follow(parsed.links);
                }
                let title = parsed.title.unwrap_or_else(|| {
                    fallback_title(&job.url, job.label.as_deref(), P::fallback_title)
                });
                // Decoding can make text longer than the bytes it came as
                // (Latin-1 to UTF-8): cut to LOST's limit, as an import would.
                let body = lost::text::truncate_at_word(&parsed.body, lost::MAX_BODY_BYTES);
                let body = if body.len() < parsed.body.len() {
                    body.to_string()
                } else {
                    parsed.body
                };
                let blocked = self.settings.blacklist.blocks_page(Some(&title), &body);
                Outcome::Page {
                    content: (!body.is_empty()).then(|| hash(&body)),
                    blocked,
                    page: Page {
                        url: job.url.to_string(),
                        sources: vec![P::NAME.to_string()],
                        title: Some(title),
                        body,
                        links: parsed.links.iter().map(|l| l.url.clone()).collect(),
                        crawled: Some(lost::store::now()),
                    },
                    links: parsed.links,
                }
            }
            Response::Document { body: None, .. } => {
                let title = fallback_title(&job.url, job.label.as_deref(), P::fallback_title);
                Outcome::Page {
                    blocked: self.settings.blacklist.blocks_page(Some(&title), ""),
                    page: Page {
                        url: job.url.to_string(),
                        sources: vec![P::NAME.to_string()],
                        title: Some(title),
                        crawled: Some(lost::store::now()),
                        ..Page::default()
                    },
                    content: None,
                    links: Vec::new(),
                }
            }
            Response::Redirect(target) => match lost::urls::resolve_any(&job.url, &target) {
                Some(url) => Outcome::Redirect(url),
                None => Outcome::Nothing,
            },
            Response::Input { prompt, sensitive } => {
                if sensitive
                    || self.settings.searches.is_empty()
                    || !P::is_search(&prompt, job.label.as_deref(), &job.url)
                {
                    return Outcome::Nothing;
                }
                let urls = self
                    .settings
                    .searches
                    .iter()
                    .map(|phrase| {
                        let mut url = job.url.clone();
                        url.query = Some(encode_query(phrase));
                        url
                    })
                    .collect();
                Outcome::Search(urls)
            }
            Response::SlowDown(wait) => Outcome::SlowDown(wait),
            Response::TryLater => Outcome::Retry,
            Response::Gone => Outcome::Gone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_rules() {
        let robots = Robots::parse(
            "User-agent: googlebot\nDisallow: /\n\n\
             User-agent: indexer\nUser-agent: researcher\nDisallow: /private/\nAllow: /private/ok\n\n\
             User-agent: *\nDisallow: /cgi-bin/*.cgi$\nDisallow: /search # no searching\nDisallow:\n",
        );
        let allows = |path: &str| robots.allows(path);
        assert!(allows("/"));
        assert!(allows("/gemlog/"));
        assert!(!allows("/private/diary.gmi"));
        assert!(allows("/private/ok.gmi"));
        assert!(!allows("/cgi-bin/calc.cgi"));
        assert!(allows("/cgi-bin/calc.cgi/more"));
        assert!(!allows("/search?cats"));
        assert!(Robots::default().allows("/a"));
    }

    #[test]
    fn md5_hashes() {
        let hex = |data: &[u8]| lost::lostfile::to_hex(&md5(data));
        assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(hex(&[b'a'; 1000]), "cabe45dcc9ae5b66ba86600cca6b8ba8");
        let mut list = Blacklist::default();
        assert_eq!(list.ban_hashed([hex(b"bad.onion"), "not hex".into()]), 1);
        let blocks = |u: &str| {
            let url = Url::parse(u).unwrap();
            list.blocks_url(&url, &url.to_string())
        };
        assert!(blocks("http://bad.onion/x"));
        assert!(blocks("http://www.bad.onion/"));
        assert!(!blocks("http://good.onion/"));
    }

    #[test]
    fn globs() {
        assert!(glob("/a", "/abc"));
        assert!(!glob("/b", "/abc"));
        assert!(glob("/a*c", "/abc"));
        assert!(glob("/*.gmi$", "/x/y.gmi"));
        assert!(!glob("/*.gmi$", "/x/y.gmi?q"));
        assert!(glob("/x$", "/x"));
        assert!(!glob("/x$", "/xy"));
    }

    #[test]
    fn blacklists() {
        let list = Blacklist::parse(
            "# a comment\n\
             casino.example   # a domain\n\
             0.0.0.0 tracker.example\n\
             bücher.example\n\
             gemini://example.net/spam/\n\
             /\\/commits?\\/[0-9a-f]{40}/   # git\n\
             title:/(?i)free bitcoin/\n\
             body:/buy now/\n",
        )
        .unwrap();
        let blocks = |u: &str| {
            let url = Url::parse(u).unwrap();
            list.blocks_url(&url, &url.to_string())
        };
        assert!(blocks("gemini://casino.example/"));
        assert!(blocks("gemini://www.casino.example/x"));
        assert!(!blocks("gemini://notcasino.example/"));
        assert!(blocks("gemini://tracker.example/"));
        assert!(blocks("gemini://bücher.example/"));
        assert!(blocks("gemini://shop.xn--bcher-kva.example/"));
        assert!(blocks("gemini://example.net/spam/1.gmi"));
        assert!(!blocks("gemini://example.net/ham/"));
        assert!(blocks(&format!("gemini://git.x/commit/{}", "a".repeat(40))));
        assert!(list.blocks_page(Some("FREE Bitcoin here"), ""));
        assert!(list.blocks_page(None, "please buy now"));
        assert!(!list.blocks_page(Some("Bitcoin"), "hello"));
        assert!(Blacklist::parse("/(unclosed/").is_err());
    }

    #[test]
    fn urls() {
        let url = clean(Url::parse("gemini://x/a b/é?q r").unwrap());
        assert_eq!(url.to_string(), "gemini://x/a%20b/%C3%A9?q%20r");
        assert_eq!(encode_query("cats & dogs"), "cats%20%26%20dogs");
        assert_eq!(percent_decode("caf%C3%A9%20menu.gmi"), "café menu.gmi");
        assert!(looks_like_trap(
            &Url::parse("gemini://x/a/b/a/b/a/b").unwrap()
        ));
        assert!(!looks_like_trap(&Url::parse("gemini://x/a/b/c/d").unwrap()));
        let at = |path: &str| Url::parse(&format!("gemini://x{path}")).unwrap();
        assert!(looks_like_search("Search the archive", &at("/")));
        assert!(looks_like_search("Enter terms", &at("/search")));
        assert!(!looks_like_search("Leave a comment", &at("/post")));
        assert!(!looks_like_search("Search posts", &at("/")));
        assert!(!looks_like_search("Your name?", &at("/guestbook")));
        assert_eq!(
            mentions("see gemini://a.b/c.gmi, and (gemini://d.e/).", "gemini")
                .into_iter()
                .map(|l| l.url)
                .collect::<Vec<_>>(),
            ["gemini://a.b/c.gmi", "gemini://d.e/"]
        );
        assert_eq!(
            fallback_title(&at("/files/My%20cat.jpg"), None, Fake::fallback_title),
            "My cat.jpg"
        );
        assert_eq!(fallback_title(&at("/"), None, Fake::fallback_title), "x");
        assert_eq!(
            fallback_title(&at("/a"), Some("Link"), Fake::fallback_title),
            "Link"
        );
        let idn = clean(Url::parse("gemini://Café.example/x").unwrap());
        assert_eq!(idn.host, "café.example");
        assert_eq!(host_key(&idn), "gemini://xn--caf-dma.example");
        assert_ne!(
            host_key(&Url::parse("http://x.onion/").unwrap()),
            host_key(&Url::parse("https://x.onion/").unwrap())
        );
        assert_eq!(
            on_the_wire(&idn).to_string(),
            "gemini://xn--caf-dma.example/x"
        );
        assert_eq!(
            seen_key(&idn),
            seen_key(&Url::parse("gemini://xn--caf-dma.example/x").unwrap())
        );
    }

    /// A tiny Geminispace in memory.
    struct Fake {
        pages: HashMap<String, Response>,
        asked: Mutex<Vec<String>>,
        proxied: bool,
    }

    impl Protocol for Fake {
        const NAME: &'static str = "gemini";
        const SCHEME: &'static str = "gemini";
        const SPACE: &'static str = "Fakespace";
        const HOSTS: &'static str = "hosts";

        fn seeds(&self) -> &'static [&'static str] {
            &[]
        }

        fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
            assert_eq!(matches!(route, Route::Proxied), self.proxied);
            let url = url.to_string();
            let mut asked = self.asked.lock().unwrap();
            let before = asked.iter().filter(|u| **u == url).count();
            asked.push(url.clone());
            drop(asked);
            if url.contains("down.test") {
                return Err("unreachable".into());
            }
            // flaky.test's rules fail twice before they come; shy.test's
            // never do.
            if url == "gemini://flaky.test/robots.txt" && before < 2
                || url == "gemini://shy.test/robots.txt"
            {
                return Err("timed out".into());
            }
            Ok(self.pages.get(&url).cloned().unwrap_or(Response::Gone))
        }

        fn parse(&self, url: &Url, _: &str, body: &[u8]) -> Parsed {
            let text = String::from_utf8_lossy(body);
            let links = text
                .split_whitespace()
                .filter_map(|w| w.strip_prefix("=>"))
                .filter_map(|target| lost::urls::resolve_any(url, target))
                .map(|u| Link {
                    url: u.to_string(),
                    label: None,
                })
                .collect();
            Parsed {
                title: None,
                body: text.into_owned(),
                links,
                skip: false,
            }
        }

        fn has_robots(origin: &Url) -> bool {
            !origin.host.starts_with("norobots")
        }

        fn resolve(&self, url: &Url) -> Option<SocketAddr> {
            assert!(!self.proxied, "nothing is looked up through a proxy");
            // Every *.test host lives on one server, except nowhere.test.
            (!url.host.starts_with("nowhere"))
                .then(|| SocketAddr::from(([127, 0, 0, 1], port(url))))
        }

        fn proxy(&self) -> Option<&crate::net::Proxy> {
            None
        }

        fn proxied(&self) -> bool {
            self.proxied
        }
    }

    fn doc(text: &str) -> Response {
        Response::Document {
            mime: "text/gemini".into(),
            body: Some(text.as_bytes().to_vec()),
        }
    }

    fn crawl(
        pages: Vec<(&str, Response)>,
        settings: Settings,
        known: Known,
        links: &[&str],
    ) -> (Vec<Page>, Vec<String>) {
        crawl_via(false, pages, settings, known, links)
    }

    fn crawl_via(
        proxied: bool,
        pages: Vec<(&str, Response)>,
        settings: Settings,
        known: Known,
        links: &[&str],
    ) -> (Vec<Page>, Vec<String>) {
        let fake = Fake {
            pages: pages.into_iter().map(|(u, r)| (u.to_string(), r)).collect(),
            asked: Mutex::new(Vec::new()),
            proxied,
        };
        let links: Vec<String> = links.iter().map(|l| l.to_string()).collect();
        let crawler = Crawler::new(fake, settings, known, &links);
        let (tx, rx) = std::sync::mpsc::sync_channel(100);
        crawler.start(tx);
        let mut saved: Vec<Page> = rx.iter().collect();
        saved.sort_by(|a, b| a.url.cmp(&b.url));
        let asked = crawler.protocol.asked.lock().unwrap().clone();
        (saved, asked)
    }

    fn settings() -> Settings {
        Settings {
            delay: Duration::ZERO,
            host_limit: None,
            searches: Vec::new(),
            blacklist: Blacklist::default(),
            dont_crawl: false,
            workers: 4,
        }
    }

    #[test]
    fn ids_are_sent_as_they_are() {
        // An Arweave ID lowercased is another ID (one that isn't there).
        let tx = "bNbA3TEQVL60xlgCcqdz4ZPHFZ711cZ3hmkpGttDt_U";
        let url = Url::parse(&format!("ar://{tx}/")).unwrap();
        assert_eq!(on_the_wire(&url).host, tx);
        assert_eq!(host_key(&url), format!("ar://{tx}"));
        let other = Url::parse(&format!("ar://{}/", tx.to_lowercase())).unwrap();
        assert_ne!(seen_key(&url), seen_key(&other));
        // Other hosts still go out lowercase, in punycode.
        let mut url = Url::parse("gemini://café.example/").unwrap();
        url.host = "Café.Example".into();
        assert_eq!(on_the_wire(&url).host, "xn--caf-dma.example");
    }

    #[test]
    fn what_wasnt_saved_and_why() {
        let fake = Fake {
            pages: [
                (
                    "gemini://a.test/",
                    doc("Home =>/copy =>/missing =>gemini://down.test/"),
                ),
                (
                    "gemini://a.test/copy",
                    doc("Home =>/copy =>/missing =>gemini://down.test/"),
                ),
                ("gemini://a.test/robots.txt", doc("")),
            ]
            .into_iter()
            .map(|(u, r)| (u.to_string(), r))
            .collect(),
            asked: Mutex::new(Vec::new()),
            proxied: false,
        };
        let crawler = Crawler::new(
            fake,
            settings(),
            Known::default(),
            &["gemini://a.test/".into()],
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(100);
        crawler.start(tx);
        assert_eq!(rx.iter().count(), 1);
        let skipped = crawler.stats().skipped;
        assert_eq!(
            skipped,
            Skipped {
                copies: 1,
                gone: 1,
                unreachable: 1,
                blocked: 0,
            }
        );
        assert_eq!(skipped.total(), 3);
    }

    #[test]
    fn a_small_crawl() {
        let pages = vec![
            (
                "gemini://a.test/",
                doc(
                    "Home =>/about =>gemini://b.test/ =>/pic.png =>/secret/x =>gemini://down.test/ =>gemini://nowhere.test/",
                ),
            ),
            (
                "gemini://a.test/robots.txt",
                doc("User-agent: indexer\nDisallow: /secret/"),
            ),
            ("gemini://a.test/about", doc("About =>/ =>/old")),
            ("gemini://a.test/old", Response::Redirect("/new".into())),
            ("gemini://a.test/new", doc("Moved here")),
            ("gemini://b.test/", doc("Home")),
            ("gemini://b.test/copy", doc("Home")),
            (
                "gemini://b.test/search",
                Response::Input {
                    prompt: "Search".into(),
                    sensitive: false,
                },
            ),
            ("gemini://b.test/search?cats", doc("Results =>/cats")),
            ("gemini://b.test/cats", doc("Meow")),
            (
                "gemini://b.test/login",
                Response::Input {
                    prompt: "Search".into(),
                    sensitive: true,
                },
            ),
            (
                "gemini://b.test/comment",
                Response::Input {
                    prompt: "Leave a comment".into(),
                    sensitive: false,
                },
            ),
        ];
        let settings = Settings {
            searches: vec!["cats".into()],
            ..settings()
        };
        let links = [
            "gemini://a.test/",
            "gemini://b.test/search",
            "gemini://b.test/login",
            "gemini://b.test/comment",
            "gemini://b.test/copy",
        ];
        let (saved, asked) = crawl(pages, settings, Known::default(), &links);
        // b.test's home page and its copy have the same text: one is saved,
        // whichever came first.
        let copies = saved
            .iter()
            .filter(|p| p.url == "gemini://b.test/" || p.url == "gemini://b.test/copy")
            .count();
        assert_eq!(copies, 1);
        let urls: Vec<&str> = saved
            .iter()
            .map(|p| p.url.as_str())
            .filter(|u| !u.starts_with("gemini://b.test/") || u.ends_with("cats"))
            .collect();
        assert_eq!(
            urls,
            [
                "gemini://a.test/",
                "gemini://a.test/about",
                "gemini://a.test/new",
                "gemini://a.test/pic.png",
                "gemini://b.test/cats",
            ]
        );
        // The picture was saved without asking for it; the secret wasn't asked
        // for; the search results weren't saved; the copy of b.test's home
        // page wasn't saved twice; nothing was typed into the other boxes.
        assert!(
            !asked
                .iter()
                .any(|u| u.ends_with("pic.png") || u.contains("secret"))
        );
        assert!(asked.contains(&"gemini://b.test/search?cats".to_string()));
        assert!(
            !asked
                .iter()
                .any(|u| u.contains("login?") || u.contains("comment?"))
        );
        assert_eq!(saved[3].title.as_deref(), Some("pic.png"));
        assert_eq!(saved[0].sources, ["gemini"]);
        // An unreachable host is asked a few times, then left alone.
        assert!(asked.iter().filter(|u| u.contains("down.test")).count() <= MAX_FAILURES as usize);
        assert!(asked.iter().all(|u| !u.contains("nowhere.test")));
    }

    #[test]
    fn internationalized_hosts() {
        // Asked for in punycode, saved as written; one host, either spelling.
        let pages = vec![
            (
                "gemini://xn--caf-dma.test/",
                doc("Menu =>gemini://xn--caf-dma.test/ =>/tea"),
            ),
            ("gemini://xn--caf-dma.test/tea", doc("Tea")),
        ];
        let (saved, asked) = crawl(
            pages,
            settings(),
            Known::default(),
            &["gemini://Café.test/"],
        );
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["gemini://café.test/", "gemini://café.test/tea"]);
        assert!(asked.iter().all(|u| u.is_ascii()));
        assert_eq!(asked.iter().filter(|u| u.ends_with(".test/")).count(), 1);
    }

    #[test]
    fn through_a_proxy() {
        // Nothing is looked up; every host is its own server, and an
        // unreachable one (down.test) is left alone after a few tries.
        let pages = vec![
            (
                "gemini://a.test/",
                doc("A =>gemini://b.test/ =>gemini://down.test/ =>gemini://nowhere.test/"),
            ),
            ("gemini://b.test/", doc("B")),
            ("gemini://nowhere.test/", doc("Found through the proxy")),
        ];
        let (saved, asked) = crawl_via(
            true,
            pages,
            settings(),
            Known::default(),
            &["gemini://a.test/"],
        );
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "gemini://a.test/",
                "gemini://b.test/",
                "gemini://nowhere.test/"
            ]
        );
        assert!(asked.iter().filter(|u| u.contains("down.test")).count() <= MAX_FAILURES as usize);
    }

    #[test]
    fn not_crawling() {
        let pages = vec![
            (
                "gemini://a.test/",
                doc("Home =>/1 =>gemini://b.test/ =>gemini://c.test/"),
            ),
            ("gemini://a.test/1", doc("One")),
            ("gemini://b.test/", doc("B =>/2")),
            ("gemini://b.test/2", doc("Two")),
            ("gemini://c.test/", doc("C")),
        ];
        let only = Settings {
            dont_crawl: true,
            searches: vec!["cats".into()],
            ..settings()
        };
        let mut pages = pages;
        pages.push(("gemini://b.test/old", Response::Redirect("/2".into())));
        pages.push((
            "gemini://c.test/find",
            Response::Input {
                prompt: "Search".into(),
                sensitive: false,
            },
        ));
        pages.push(("gemini://c.test/find?cats", doc("Results =>/cat")));
        let links = [
            "gemini://a.test/",
            "gemini://b.test/old",
            "gemini://c.test/find",
        ];
        let (saved, asked) = crawl(pages, only, Known::default(), &links);
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "gemini://a.test/",
                "gemini://b.test/2",
                "gemini://c.test/find?cats"
            ]
        );
        // Only those links, where one redirects, and the search's results;
        // nothing they link to.
        assert!(
            !asked
                .iter()
                .any(|u| u.ends_with("/1") || u.ends_with("/cat") || *u == "gemini://b.test/")
        );
    }

    #[test]
    fn continuing_and_limits() {
        let pages = vec![
            ("gemini://a.test/", doc("Home =>/1 =>/2 =>/3")),
            ("gemini://a.test/1", doc("One")),
            ("gemini://a.test/2", doc("Two")),
            ("gemini://a.test/3", doc("Three")),
        ];
        // The file already has the home page and page 1: only 2 and 3 are new.
        let mut known = Known::default();
        for (url, body, links) in [
            (
                "gemini://a.test/",
                "Home",
                vec![
                    "gemini://a.test/1",
                    "gemini://a.test/2",
                    "gemini://a.test/3",
                ],
            ),
            ("gemini://a.test/1", "One", vec![]),
        ] {
            known.add(
                Page {
                    url: url.into(),
                    body: body.into(),
                    links: links.into_iter().map(String::from).collect(),
                    ..Page::default()
                },
                Fake::accepts,
            );
        }
        let (saved, asked) = crawl(pages.clone(), settings(), known, &[]);
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["gemini://a.test/2", "gemini://a.test/3"]);
        assert!(!asked.contains(&"gemini://a.test/".to_string()));

        // At most two pages from a host.
        let limited = Settings {
            host_limit: Some(2),
            ..settings()
        };
        let (saved, _) = crawl(pages, limited, Known::default(), &["gemini://a.test/"]);
        assert_eq!(saved.len(), 2);
    }

    #[test]
    fn waiting_out_the_delay() {
        // One host and a delay between asks: the crawl waits for it rather
        // than ending while the only server is resting.
        let pages = vec![
            ("gemini://a.test/", doc("Home =>/1 =>/2")),
            ("gemini://a.test/1", doc("One")),
            ("gemini://a.test/2", doc("Two")),
        ];
        let slow = Settings {
            delay: Duration::from_millis(150),
            ..settings()
        };
        let (saved, _) = crawl(pages, slow, Known::default(), &["gemini://a.test/"]);
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(
            urls,
            ["gemini://a.test/", "gemini://a.test/1", "gemini://a.test/2"]
        );
    }

    #[test]
    fn redirects_to_another_spelling() {
        let pages = vec![
            ("gemini://a.test/", doc("Home =>/dir")),
            ("gemini://a.test/dir", Response::Redirect("/dir/".into())),
            ("gemini://a.test/dir/", doc("Inside")),
        ];
        let (saved, asked) = crawl(pages, settings(), Known::default(), &["gemini://a.test/"]);
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["gemini://a.test/", "gemini://a.test/dir/"]);
        assert_eq!(asked.iter().filter(|u| u.ends_with("/dir/")).count(), 1);
    }

    #[test]
    fn rules_that_fail() {
        let pages = vec![
            ("gemini://flaky.test/", doc("Flaky =>/private/x")),
            (
                "gemini://flaky.test/robots.txt",
                doc("User-agent: *\nDisallow: /private/"),
            ),
            ("gemini://flaky.test/private/x", doc("Hidden")),
            ("gemini://shy.test/", doc("Shy")),
        ];
        let (saved, asked) = crawl(
            pages,
            settings(),
            Known::default(),
            &["gemini://flaky.test/", "gemini://shy.test/"],
        );
        let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
        // flaky.test's rules were asked for again until they came, and
        // kept; shy.test's never came, so nothing there was asked for.
        assert_eq!(urls, ["gemini://flaky.test/"]);
        assert!(!asked.iter().any(|u| u.contains("private")));
        assert_eq!(
            asked
                .iter()
                .filter(|u| *u == "gemini://flaky.test/robots.txt")
                .count(),
            3
        );
        assert!(!asked.contains(&"gemini://shy.test/".to_string()));
        assert!(
            asked
                .iter()
                .filter(|u| *u == "gemini://shy.test/robots.txt")
                .count()
                <= MAX_FAILURES as usize
        );
    }

    #[test]
    fn hosts_without_robots_txt() {
        // Not asked for a robots.txt it can't have; crawled all the same,
        // looked up or through a proxy.
        for proxied in [false, true] {
            let pages = vec![
                ("gemini://norobots.test/", doc("Home =>/a")),
                ("gemini://norobots.test/a", doc("A")),
            ];
            let (saved, asked) = crawl_via(
                proxied,
                pages,
                settings(),
                Known::default(),
                &["gemini://norobots.test/"],
            );
            let urls: Vec<&str> = saved.iter().map(|p| p.url.as_str()).collect();
            assert_eq!(
                urls,
                ["gemini://norobots.test/", "gemini://norobots.test/a"]
            );
            assert!(
                !asked.iter().any(|u| u.ends_with("robots.txt")),
                "{asked:?}"
            );
        }
    }
}
