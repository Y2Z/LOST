//! I2P sites ("eepsites"): the web, through an I2P router's HTTP proxy
//! (`127.0.0.1:4444` by default; i2pd's SOCKS5 proxy works too). Reaching a
//! site the first time can take a minute or more while the router builds
//! tunnels, so the default timeout is long.
//!
//! The router finds `.i2p` names in its address book, which a router that
//! has just started doesn't have yet. So the spider keeps its own: every
//! address book (`hosts.txt`) it reads gives it sites' destinations, and it
//! asks for those sites by their `.b32.i2p` addresses, worked out from the
//! destinations, while saving their pages under their names.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lost::urls::Url;

use crate::crawl::{Link, Parsed, Protocol, Response, Route};
use crate::http::{self, Http};
use crate::net::Proxy;

/// Where a crawl starts. First, the address books routers start with, by
/// `.b32.i2p` address so the router needn't know any names yet: i2pd's
/// (`addressbook.defaulturl` in its source) and Java I2P's (the I2P
/// project's, per geti2p.net/naming). Then the other address books i2pd's
/// sample configuration subscribes to, notbob.i2p's list of sites that are
/// up, and the I2P project's site.
const SEEDS: &[&str] = &[
    "http://shx5vqsw7usdaunyzr2qmes2fq37oumybpudrd4jjj4e4vk4uusa.b32.i2p/hosts.txt",
    "http://udhdrtrcetjm5sxzskjyr5ztpeszydbh4dpl3pl4utgqqw2v4jna.b32.i2p/hosts.txt",
    "http://reg.i2p/hosts.txt",
    "http://identiguy.i2p/hosts.txt",
    "http://stats.i2p/cgi-bin/newhosts.txt",
    "http://notbob.i2p/",
    "http://i2p-projekt.i2p/",
];

/// How long to wait for a router that has just started.
const WARM_UP: Duration = Duration::from_secs(10 * 60);

pub const TIMEOUT: Duration = Duration::from_secs(120);
pub const PROXY: &str = "http://127.0.0.1:4444";

pub struct I2p {
    http: Http,
    /// `name.i2p` → `….b32.i2p`, from the address books read.
    names: Mutex<HashMap<String, String>>,
}

impl I2p {
    pub fn new(proxy: Option<Proxy>, timeout: Option<Duration>) -> lost::Result<I2p> {
        let proxy = match proxy {
            Some(proxy) => proxy,
            None => Proxy::parse(PROXY)?,
        };
        Ok(I2p {
            http: Http::new(Some(proxy), timeout.unwrap_or(TIMEOUT))?,
            names: Mutex::new(HashMap::new()),
        })
    }

    /// Learns the sites of an address book; returns their links.
    fn learn(&self, text: &str) -> Vec<Link> {
        let entries = hosts_txt(text);
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .into_iter()
            .map(|(name, b32)| {
                let link = Link {
                    url: format!("http://{name}/"),
                    label: None,
                };
                names.insert(name, b32);
                link
            })
            .collect()
    }
}

impl Protocol for I2p {
    const NAME: &'static str = "i2p";
    const SCHEME: &'static str = "http";
    const SPACE: &'static str = "the Invisible Internet";
    const HOSTS: &'static str = "eepsites";

    fn seeds(&self) -> &'static [&'static str] {
        SEEDS
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.http.proxy()
    }

    /// The router answers for sites it can't reach, with pages of its own:
    /// those count as the site being unreachable, not as its pages.
    fn fetch(&self, route: Route, url: &Url) -> Result<Response, String> {
        // A name from an address book: asked for by its .b32.i2p address.
        let b32 = self
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&url.host)
            .cloned();
        let answer = match b32 {
            Some(host) => {
                let mut url = url.clone();
                url.host = host;
                self.http.get(route, &url)?
            }
            None => self.http.get(route, url)?,
        };
        if answer.status >= 500 {
            return Err(format!("unreachable ({})", answer.status));
        }
        if answer.status >= 400 {
            let body = String::from_utf8_lossy(&answer.body).to_ascii_lowercase();
            if [
                "addressbook",
                "address book",
                "jump service",
                "destination not found",
                "i2p router",
            ]
            .iter()
            .any(|w| body.contains(w))
            {
                return Err(format!("not found by the router ({})", answer.status));
            }
        }
        Ok(http::response(answer))
    }

    fn parse(&self, url: &Url, mime: &str, body: &[u8]) -> Parsed {
        let mut parsed = http::parse(url, mime, body);
        if !mime.to_ascii_lowercase().contains("html") && hosts_txt(&parsed.body).len() >= 10 {
            // An address book: every site in it to crawl, the list itself
            // not saved.
            parsed.links = self.learn(&parsed.body);
            parsed.skip = true;
        }
        parsed
    }

    /// Waits for the router to reach sites (it takes a few minutes after it
    /// starts), reading the first address book on the way.
    fn prepare(&self, _blacklist: &mut crate::crawl::Blacklist) -> lost::Result<()> {
        let url = Url::parse(SEEDS[0]).expect("a valid URL");
        let started = Instant::now();
        let mut said = false;
        loop {
            match self.fetch(Route::Proxied, &url) {
                Ok(Response::Document {
                    body: Some(body), ..
                }) => {
                    let found = self.learn(&String::from_utf8_lossy(&body)).len();
                    if said {
                        eprintln!("The router is ready.");
                    }
                    if found > 0 {
                        eprintln!(
                            "{} sites in its address book.",
                            lost::text::thousands(found as u64)
                        );
                    }
                    return Ok(());
                }
                // Reached, but no address book there: carry on without it.
                Ok(_) => return Ok(()),
                Err(_) if started.elapsed() < WARM_UP => {
                    if !said {
                        eprintln!(
                            "Waiting for the I2P router to be able to reach sites \
                             (a few minutes after it starts)…"
                        );
                        said = true;
                    }
                    std::thread::sleep(Duration::from_secs(10));
                }
                Err(e) => {
                    return Err(format!(
                        "the I2P router couldn't reach a site in {} minutes ({e}); is it connected?",
                        WARM_UP.as_secs() / 60
                    )
                    .into());
                }
            }
        }
    }

    fn accepts(url: &Url) -> bool {
        url.scheme == "http" && url.host.ends_with(".i2p")
    }
}

/// The sites of an address book (`name.i2p=destination`, one a line; a
/// `#!` after the destination starts extra fields): each name with its
/// `.b32.i2p` address.
fn hosts_txt(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().split_once('=')?;
            let name = name.trim().to_ascii_lowercase();
            let valid = name.ends_with(".i2p")
                && !name.ends_with(".b32.i2p")
                && !name.starts_with('#')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
            if !valid {
                return None;
            }
            let destination = rest.split("#!").next().unwrap_or("").trim();
            Some((name, b32_address(destination)?))
        })
        .collect()
}

/// A destination's `.b32.i2p` address: the base32 of the SHA-256 of its
/// bytes (I2P's base64 decoded: `-` and `~` for `+` and `/`).
fn b32_address(destination: &str) -> Option<String> {
    let bytes = base64_i2p(destination)?;
    // A destination is 387 bytes at least (keys and a certificate).
    if bytes.len() < 387 {
        return None;
    }
    Some(format!("{}.b32.i2p", base32(&sha256(&bytes))))
}

fn base64_i2p(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-~";
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut count) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = bits << 6 | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Some(out)
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut bits, mut count) = (0u32, 0);
    for &b in bytes {
        bits = bits << 8 | u32::from(b);
        count += 8;
        while count >= 5 {
            count -= 5;
            out.push(ALPHABET[(bits >> count) as usize & 31] as char);
        }
        bits &= (1 << count) - 1;
    }
    if count > 0 {
        out.push(ALPHABET[(bits << (5 - count)) as usize & 31] as char);
    }
    out
}

/// SHA-256 (FIPS 180-4): only to work out `.b32.i2p` addresses.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_encodings() {
        let hex = |b: &[u8]| lost::lostfile::to_hex(b);
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(&[b'a'; 1000])),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
        assert_eq!(base64_i2p("Zm9vYmFy"), Some(b"foobar".to_vec()));
        assert_eq!(base64_i2p("-~-~"), Some(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(base64_i2p("not base64!"), None);
        let address = b32_address(&"A".repeat(516)).unwrap();
        assert_eq!(address.len(), 52 + ".b32.i2p".len());
        assert_eq!(address, format!("{}.b32.i2p", base32(&sha256(&[0u8; 387]))));
    }

    #[test]
    fn address_books() {
        let destination = "A".repeat(516);
        let text = (0..12)
            .map(|i| format!("site{i}.i2p={destination}"))
            .chain(["# a comment".into(), "bad name.i2p=x".into()])
            .collect::<Vec<_>>()
            .join("\n");
        let i2p = I2p::new(None, None).unwrap();
        let url = Url::parse("http://i2p-projekt.i2p/hosts.txt").unwrap();
        let page = i2p.parse(&url, "text/plain", text.as_bytes());
        assert!(page.skip);
        assert_eq!(page.links.len(), 12);
        assert_eq!(page.links[3].url, "http://site3.i2p/");
        // It remembers where they are.
        assert_eq!(
            i2p.names.lock().unwrap().get("site3.i2p"),
            b32_address(&destination).as_ref()
        );
        // An ordinary text file is a page.
        let page = i2p.parse(&url, "text/plain", b"Hello, see http://other.i2p/.");
        assert!(!page.skip);
        assert_eq!(page.links[0].url, "http://other.i2p/");
        let accepts = |u: &str| I2p::accepts(&Url::parse(u).unwrap());
        assert!(accepts("http://stats.i2p/"));
        assert!(accepts(
            "http://udhdrtrcetjm5sxzskjyr5ztpeszydbh4dpl3pl4utgqqw2v4jna.b32.i2p/"
        ));
        assert!(!accepts("https://stats.i2p/"));
        assert!(!accepts("http://example.com/"));
    }
}
