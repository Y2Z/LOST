//! URL normalization (ARCHITECTURE.md §6.5) and the fields derived from URLs.
//!
//! Deliberately dependency-free: the rules here decide which records merge into
//! the same document, so they're kept small, explicit and well tested.

/// Query parameters that only track visitors and never change what a page shows.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "igshid", "mc_cid",
    "mc_eid", "_hsenc", "_hsmi", "ref", "ref_src", "ref_url", "spm",
];

/// A URL split into the parts LOST cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Lowercase scheme, e.g. `https`.
    pub scheme: String,
    /// Lowercase host, without port or credentials. Except for `ipfs://`,
    /// `ipns://` and `ar://`, whose "hosts" are content IDs, keys and
    /// Arweave transaction IDs, which mean something else in another case
    /// (`Qm…`, `12D3Koo…` in base58; Arweave's in base64). For a Nostr URI
    /// (`nostr:npub1…`, NIP-21), what follows `nostr:`.
    pub host: String,
    /// Port, if one was given and it isn't the scheme's default.
    pub port: Option<u16>,
    /// Path, always starting with `/`.
    pub path: String,
    /// Query string without the `?`, if any.
    pub query: Option<String>,
    /// Fragment without the `#`, if any.
    pub fragment: Option<String>,
}

impl Url {
    /// Parses an absolute URL. Returns `None` for anything that isn't one.
    ///
    /// Nostr URIs have no `//` (`nostr:npub1…`): what follows `nostr:` is
    /// kept as the host, lowercase (it's bech32, which has one case), with
    /// `/` as the path. `nostr://npub1…` is taken as the same.
    pub fn parse(input: &str) -> Option<Url> {
        let input = input.trim();
        let Some((scheme, rest)) = input.split_once("://") else {
            let (scheme, rest) = input.split_once(':')?;
            return if scheme.eq_ignore_ascii_case("nostr") {
                nostr(rest)
            } else {
                None
            };
        };
        if scheme.eq_ignore_ascii_case("nostr") {
            return nostr(rest);
        }
        if scheme.is_empty()
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();

        let (before_fragment, fragment) = match rest.split_once('#') {
            Some((a, f)) => (a, Some(f.to_string())),
            None => (rest, None),
        };
        let (before_query, query) = match before_fragment.split_once('?') {
            Some((a, q)) => (a, Some(q.to_string())),
            None => (before_fragment, None),
        };
        let (authority, path) = match before_query.find('/') {
            Some(i) => (&before_query[..i], &before_query[i..]),
            None => (before_query, "/"),
        };
        // Drop credentials.
        let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);

        let keep_case = keeps_case(&scheme);
        let fold = |host: &str| {
            if keep_case {
                host.to_string()
            } else {
                host.to_ascii_lowercase()
            }
        };
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            // IPv6 literal: [::1]:8080
            let (h, after) = stripped.split_once(']')?;
            let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
            (format!("[{}]", h.to_ascii_lowercase()), port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                    (fold(h), p.parse().ok())
                }
                Some((h, "")) => (fold(h), None),
                _ => (fold(authority), None),
            }
        };
        let host = host.trim_end_matches('.').to_string();
        if host.is_empty() {
            return None;
        }
        let port = port.filter(|p| Some(*p) != default_port(&scheme));

        Some(Url {
            scheme,
            host,
            port,
            path: path.to_string(),
            query: query.filter(|q| !q.is_empty()),
            fragment: fragment.filter(|f| !f.is_empty()),
        })
    }

    /// The normalized form used to decide whether two URLs are the same page.
    ///
    /// - `http` and `https` are the same page; so are `www.example.com` and `example.com`.
    /// - Default ports, tracking parameters and plain `#anchors` are removed.
    /// - A trailing slash is removed from the path (except for the root `/`).
    /// - Route-style fragments of JavaScript apps (`#!/…`, `#/…`) are kept.
    pub fn normalized(&self) -> String {
        if self.scheme == "nostr" {
            return format!("nostr:{}", self.host);
        }
        let scheme = match self.scheme.as_str() {
            "http" | "https" => "web",
            other => other,
        };
        let host = self.host.strip_prefix("www.").unwrap_or(&self.host);

        let mut out = format!("{scheme}://{host}");
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }

        let path = if self.path.len() > 1 {
            self.path.trim_end_matches('/')
        } else {
            "/"
        };
        out.push_str(&uppercase_escapes(if path.is_empty() { "/" } else { path }));

        if let Some(query) = &self.query {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let name = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
                    !name.is_empty()
                        && !name.starts_with("utm_")
                        && !TRACKING_PARAMS.contains(&name.as_str())
                })
                .collect();
            if !kept.is_empty() {
                out.push('?');
                out.push_str(&uppercase_escapes(&kept.join("&")));
            }
        }

        if let Some(fragment) = &self.fragment
            && (fragment.starts_with('!') || fragment.starts_with('/'))
        {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }

    /// The host's labels reversed and joined, for every level of the domain:
    /// `news.example.com` gives `com`, `com.example`, `com.example.news`.
    /// `site:example.com` is then a single term lookup for `com.example`.
    pub fn host_suffixes(&self) -> Vec<String> {
        host_suffixes(&self.host)
    }

    /// The file extension of the last path segment (`pdf`, `html`, …), if it has one.
    pub fn extension(&self) -> Option<String> {
        let segment = self.path.rsplit('/').next()?;
        let (_, ext) = segment.rsplit_once('.')?;
        let ext = ext.to_ascii_lowercase();
        let ok = (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric());
        ok.then_some(ext)
    }
}

impl std::fmt::Display for Url {
    /// The URL written out again (without credentials).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.scheme == "nostr" {
            return write!(f, "nostr:{}", self.host);
        }
        write!(f, "{}://{}", self.scheme, self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        f.write_str(&self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        if let Some(frag) = &self.fragment {
            write!(f, "#{frag}")?;
        }
        Ok(())
    }
}

/// A Nostr URI's entity (`npub1…`, `note1…`, `naddr1…`…): letters and
/// digits only, up to a `/`, `?` or `#`.
fn nostr(rest: &str) -> Option<Url> {
    let entity = rest.split(['/', '?', '#']).next().unwrap_or("");
    if entity.is_empty() || !entity.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(Url {
        scheme: "nostr".into(),
        host: entity.to_ascii_lowercase(),
        port: None,
        path: "/".into(),
        query: None,
        fragment: None,
    })
}

/// Resolves a link found on the page at `base` into an absolute `http(s)`
/// URL, without its `#fragment`. Returns `None` for links that don't lead to
/// another page (`javascript:`, `mailto:`, `#anchor`, `data:`, …).
pub fn resolve(base: &Url, href: &str) -> Option<String> {
    let url = resolve_any(base, href)?;
    (url.scheme == "http" || url.scheme == "https").then(|| url.to_string())
}

/// Like [`resolve`], for any scheme with an authority (`gemini://`,
/// `gopher://`, …): the absolute URL, without its `#fragment`. Relative
/// links take `base`'s scheme. `None` for links that don't lead to another
/// page (`mailto:`, `#anchor`, …).
pub fn resolve_any(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    let href = href.split('#').next().unwrap_or("");
    if href.is_empty() {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    let has_scheme = lower.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    });
    let absolute = if has_scheme || lower.starts_with("nostr:") {
        Url::parse(href)?
    } else if lower.contains(':') && !lower.starts_with('/') && !lower.starts_with('?') {
        // Some other scheme (javascript:, mailto:, tel:, data:, …) — or a
        // relative path with a colon before any slash, which is too rare to matter.
        let scheme_like = lower.split(':').next().unwrap_or("");
        if scheme_like.contains('/') || scheme_like.contains('.') {
            Url::parse(&join(base, href))?
        } else {
            return None;
        }
    } else if let Some(rest) = href.strip_prefix("//") {
        Url::parse(&format!("{}://{rest}", base.scheme))?
    } else {
        Url::parse(&join(base, href))?
    };
    let mut url = absolute;
    url.fragment = None;
    Some(url)
}

/// Joins a path-relative, root-relative or query-only link onto `base`.
fn join(base: &Url, href: &str) -> String {
    let mut origin = format!("{}://{}", base.scheme, base.host);
    if let Some(port) = base.port {
        origin.push_str(&format!(":{port}"));
    }
    let (path, query) = match href.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (href, None),
    };
    let path = if path.is_empty() {
        base.path.clone()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        let dir = &base.path[..=base.path.rfind('/').unwrap_or(0)];
        format!("{dir}{path}")
    };
    let mut out = origin + &remove_dot_segments(&path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

fn remove_dot_segments(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        match *part {
            "." => {
                if last {
                    segments.push("");
                }
            }
            ".." => {
                if segments.len() > 1 {
                    segments.pop();
                }
                if last {
                    segments.push("");
                }
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if joined.starts_with('/') {
        joined
    } else {
        format!("/{joined}")
    }
}

/// `%d0%b8` and `%D0%B8` mean the same thing; use one spelling.
fn uppercase_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            out.push('%');
            out.push(bytes[i + 1].to_ascii_uppercase() as char);
            out.push(bytes[i + 2].to_ascii_uppercase() as char);
            i += 3;
        } else {
            let c = s[i..].chars().next().unwrap_or('\0');
            out.push(c);
            i += c.len_utf8().max(1);
        }
    }
    out
}

/// Whether a scheme's hosts are case-sensitive: content IDs and keys
/// (`ipfs://`, `ipns://`, `ar://`), where `Qm…` and `qm…` are two things.
pub fn keeps_case(scheme: &str) -> bool {
    matches!(scheme, "ipfs" | "ipns" | "ar")
}

/// The host of `url` as it's looked up and sent: [`ascii_host`]'s, but
/// untouched for a scheme whose hosts keep their case.
pub fn wire_host(url: &Url) -> Option<String> {
    if keeps_case(&url.scheme) {
        return (!url.host.is_empty()).then(|| url.host.clone());
    }
    ascii_host(&url.host)
}

/// A host name as it's looked up and sent: internationalized labels in
/// punycode (`café.example` → `xn--caf-dma.example`), lowercase. ASCII
/// names (and `[IPv6]` literals) come back lowercased. `None` for a name
/// that can't be one (an empty label, or one too long).
///
/// This is IDNA without its Unicode mapping tables: labels are lowercased and
/// encoded (RFC 3492), not normalized first, which is all links in practice
/// need.
pub fn ascii_host(host: &str) -> Option<String> {
    let host = host.trim_end_matches('.');
    if host.is_ascii() {
        return Some(host.to_ascii_lowercase());
    }
    let host = host
        .to_lowercase()
        .replace(['\u{3002}', '\u{ff0e}', '\u{ff61}'], ".");
    let mut labels = Vec::new();
    for label in host.split('.') {
        if label.is_empty() {
            return None;
        }
        let ascii = if label.is_ascii() {
            label.to_string()
        } else {
            format!("xn--{}", punycode(label)?)
        };
        if ascii.len() > 63 {
            return None;
        }
        labels.push(ascii);
    }
    let ascii = labels.join(".");
    (ascii.len() <= 253).then_some(ascii)
}

/// RFC 3492's Bootstring with punycode's parameters.
fn punycode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;

    fn adapt(mut delta: u32, points: u32, first: bool) -> u32 {
        delta /= if first { DAMP } else { 2 };
        delta += delta / points;
        let mut k = 0;
        while delta > (BASE - T_MIN) * T_MAX / 2 {
            delta /= BASE - T_MIN;
            k += BASE;
        }
        k + (BASE - T_MIN + 1) * delta / (delta + SKEW)
    }
    fn digit(d: u32) -> char {
        (if d < 26 {
            b'a' + d as u8
        } else {
            b'0' + (d - 26) as u8
        }) as char
    }

    let code_points: Vec<u32> = input.chars().map(|c| c as u32).collect();
    let mut out: String = input.chars().filter(char::is_ascii).collect();
    let basic = out.len() as u32;
    if basic > 0 {
        out.push('-');
    }
    let (mut n, mut delta, mut bias, mut handled) = (128u32, 0u32, 72u32, basic);
    while (handled as usize) < code_points.len() {
        let m = code_points.iter().copied().filter(|&c| c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
        n = m;
        for &c in &code_points {
            if c < n {
                delta = delta.checked_add(1)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        T_MIN
                    } else if k >= bias + T_MAX {
                        T_MAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit(q));
                bias = adapt(delta, handled + 1, handled == basic);
                delta = 0;
                handled += 1;
            }
        }
        delta = delta.checked_add(1)?;
        n += 1;
    }
    Some(out)
}

/// See [`Url::host_suffixes`].
pub fn host_suffixes(host: &str) -> Vec<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.starts_with('[') {
        return vec![host].into_iter().filter(|h| !h.is_empty()).collect();
    }
    let labels: Vec<&str> = host.split('.').rev().collect();
    (1..=labels.len()).map(|n| labels[..n].join(".")).collect()
}

/// Turns what a user typed after `site:` into the term to look up:
/// `https://www.Example.com/path` gives `com.example.www`.
pub fn site_term(input: &str) -> Option<String> {
    host_suffixes(&site_host(input)?).pop()
}

/// The host in what a user typed after `site:` or `link:`, lowercase:
/// `https://www.Example.com/path` gives `www.example.com`.
pub fn site_host(input: &str) -> Option<String> {
    let input = input.trim();
    let host = match Url::parse(input) {
        Some(url) => url.host,
        None => input
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase(),
    };
    let host = host.trim_end_matches('.').to_string();
    (!host.is_empty()).then_some(host)
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        "ftp" => Some(21),
        "gopher" => Some(70),
        "gemini" => Some(1965),
        "ws" => Some(80),
        "wss" => Some(443),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(s: &str) -> String {
        Url::parse(s).unwrap().normalized()
    }

    #[test]
    fn content_ids_keep_their_case() {
        let cid = "QmbWqxBEKC3P8tqsKc98xmWNzrzDtRLMiMPL8wBuTGsMnR";
        assert_eq!(Url::parse(&format!("ipfs://{cid}/a")).unwrap().host, cid);
        assert_eq!(
            Url::parse("IPNS://12D3KooWExample/").unwrap().host,
            "12D3KooWExample"
        );
        assert_eq!(
            Url::parse("https://Example.COM/").unwrap().host,
            "example.com"
        );
        let tx = "bNbA3TEQVL60xlgCcqdz4ZPHFZ711cZ3hmkpGttDt_U";
        assert_eq!(Url::parse(&format!("ar://{tx}/")).unwrap().host, tx);
        // And as they're sent.
        let url = Url::parse(&format!("ar://{tx}/")).unwrap();
        assert_eq!(wire_host(&url).as_deref(), Some(tx));
        let url = Url::parse(&format!("ipfs://{cid}/")).unwrap();
        assert_eq!(wire_host(&url).as_deref(), Some(cid));
        let mut url = Url::parse("gemini://example.org/").unwrap();
        url.host = "Example.ORG".into();
        assert_eq!(wire_host(&url).as_deref(), Some("example.org"));
    }

    #[test]
    fn same_page_variants_normalize_identically() {
        let a = norm("https://www.Example.com/recipes/catfish/");
        assert_eq!(a, "web://example.com/recipes/catfish");
        assert_eq!(norm("http://example.com:80/recipes/catfish"), a);
        assert_eq!(norm("https://example.com:443/recipes/catfish#comments"), a);
        assert_eq!(
            norm("https://example.com/recipes/catfish?utm_source=hn&utm_medium=x&fbclid=1"),
            a
        );
    }

    #[test]
    fn meaningful_parts_are_kept() {
        assert_eq!(norm("https://example.com/"), "web://example.com/");
        assert_eq!(norm("https://example.com"), "web://example.com/");
        assert_eq!(
            norm("https://example.com/item?id=5&utm_source=x"),
            "web://example.com/item?id=5"
        );
        assert_eq!(
            norm("https://example.com:8080/a"),
            "web://example.com:8080/a"
        );
        assert_eq!(
            norm("https://app.example.com/#/inbox"),
            "web://app.example.com/#/inbox"
        );
        assert_eq!(
            norm("https://example.com/#!/page"),
            "web://example.com/#!/page"
        );
        assert_eq!(norm("gemini://example.org/"), "gemini://example.org/");
        assert_eq!(norm("https://user:pw@example.com/x"), "web://example.com/x");
    }

    #[test]
    fn invalid_urls_are_rejected() {
        assert!(Url::parse("not a url").is_none());
        assert!(Url::parse("https:///path").is_none());
        assert!(Url::parse("").is_none());
    }

    #[test]
    fn host_suffixes_and_site_terms() {
        assert_eq!(
            host_suffixes("news.example.com"),
            vec!["com", "com.example", "com.example.news"]
        );
        assert_eq!(site_term("example.com").as_deref(), Some("com.example"));
        assert_eq!(
            site_term("https://www.Example.com/path").as_deref(),
            Some("com.example.www")
        );
        assert_eq!(site_term("edu").as_deref(), Some("edu"));
        assert_eq!(site_term("example.com/foo").as_deref(), Some("com.example"));
    }

    #[test]
    fn percent_escapes_are_case_insensitive() {
        assert_eq!(
            norm("http://x.com/2019/%D0%B8%d1%82/"),
            norm("http://x.com/2019/%d0%b8%D1%82")
        );
        assert_eq!(norm("http://x.com/a%2fb"), "web://x.com/a%2Fb");
    }

    #[test]
    fn resolving_links() {
        let base = Url::parse("https://example.com/blog/post/index.html?x=1").unwrap();
        let r = |h: &str| resolve(&base, h);
        assert_eq!(r("/"), Some("https://example.com/".into()));
        assert_eq!(
            r("other.html"),
            Some("https://example.com/blog/post/other.html".into())
        );
        assert_eq!(
            r("../up.html#top"),
            Some("https://example.com/blog/up.html".into())
        );
        assert_eq!(
            r("./a/./b/../c"),
            Some("https://example.com/blog/post/a/c".into())
        );
        assert_eq!(
            r("?page=2"),
            Some("https://example.com/blog/post/index.html?page=2".into())
        );
        assert_eq!(
            r("//cdn.example.org/x"),
            Some("https://cdn.example.org/x".into())
        );
        assert_eq!(r("HTTP://Other.com"), Some("http://other.com/".into()));
        assert_eq!(r("/../../etc"), Some("https://example.com/etc".into()));
        assert_eq!(r("javascript:void(0)"), None);
        assert_eq!(r("mailto:a@b.c"), None);
        assert_eq!(r("#top"), None);
        assert_eq!(r("ftp://x.com/f"), None);
        assert_eq!(r(""), None);
    }

    #[test]
    fn punycode_hosts() {
        assert_eq!(
            ascii_host("café.example").as_deref(),
            Some("xn--caf-dma.example")
        );
        assert_eq!(
            ascii_host("München.DE").as_deref(),
            Some("xn--mnchen-3ya.de")
        );
        assert_eq!(
            ascii_host("bücher.example.").as_deref(),
            Some("xn--bcher-kva.example")
        );
        assert_eq!(
            ascii_host("日本語。jp").as_deref(),
            Some("xn--wgv71a119e.jp")
        );
        assert_eq!(
            ascii_host("ドメイン名例.jp").as_deref(),
            Some("xn--eckwd4c7cu47r2wf.jp")
        );
        assert_eq!(ascii_host("Example.ORG").as_deref(), Some("example.org"));
        assert_eq!(
            ascii_host("xn--caf-dma.example").as_deref(),
            Some("xn--caf-dma.example")
        );
        assert_eq!(ascii_host("[::1]").as_deref(), Some("[::1]"));
        assert_eq!(ascii_host("é..example"), None);
    }

    #[test]
    fn resolving_links_of_any_scheme() {
        let base = Url::parse("gemini://example.org:1965/gemlog/post.gmi").unwrap();
        let r = |h: &str| resolve_any(&base, h).map(|u| u.to_string());
        assert_eq!(
            r("next.gmi"),
            Some("gemini://example.org/gemlog/next.gmi".into())
        );
        assert_eq!(r("/"), Some("gemini://example.org/".into()));
        assert_eq!(r("//other.net/x"), Some("gemini://other.net/x".into()));
        assert_eq!(
            r("gemini://Other.net:1966/a"),
            Some("gemini://other.net:1966/a".into())
        );
        assert_eq!(
            r("https://example.com/"),
            Some("https://example.com/".into())
        );
        assert_eq!(r("mailto:a@b.c"), None);
        assert_eq!(r("#top"), None);
        // `resolve` itself still only gives the web.
        assert_eq!(resolve(&base, "next.gmi"), None);
    }

    #[test]
    fn site_hosts() {
        assert_eq!(
            site_host("https://www.Example.com/path").as_deref(),
            Some("www.example.com")
        );
        assert_eq!(site_host("example.com/foo").as_deref(), Some("example.com"));
    }

    #[test]
    fn nostr_uris() {
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        for written in [
            format!("nostr:{npub}"),
            format!("NOSTR:{}", npub.to_uppercase()),
            format!("nostr://{npub}/"),
            format!(" nostr:{npub}#x "),
        ] {
            let url = Url::parse(&written).unwrap();
            assert_eq!(url.scheme, "nostr", "{written}");
            assert_eq!(url.host, npub, "{written}");
            assert_eq!(url.path, "/");
            assert_eq!(url.to_string(), format!("nostr:{npub}"));
            assert_eq!(url.normalized(), format!("nostr:{npub}"));
        }
        assert_eq!(Url::parse("nostr:"), None);
        assert_eq!(Url::parse("nostr:not a thing"), None);
        assert_eq!(Url::parse("mailto:someone@example.org"), None);
        // As a link on a page.
        let base = Url::parse("https://example.org/a/").unwrap();
        assert_eq!(
            resolve_any(&base, &format!("nostr:{npub}")).map(|u| u.to_string()),
            Some(format!("nostr:{npub}"))
        );
        assert_eq!(resolve(&base, &format!("nostr:{npub}")), None);
        // Relays' default ports.
        assert_eq!(Url::parse("wss://relay.example:443/").unwrap().port, None);
        assert_eq!(Url::parse("ws://relay.example:80").unwrap().port, None);
    }

    #[test]
    fn extensions() {
        let ext = |s: &str| Url::parse(s).unwrap().extension();
        assert_eq!(ext("https://example.com/paper.PDF").as_deref(), Some("pdf"));
        assert_eq!(
            ext("https://example.com/a/index.html?x=1").as_deref(),
            Some("html")
        );
        assert_eq!(ext("https://example.com/a/b"), None);
        assert_eq!(ext("https://example.com/"), None);
        assert_eq!(ext("https://example.com/v1.2.3-beta_x"), None);
    }
}
