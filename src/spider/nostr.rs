//! Nostr: posts ("events") signed by their authors and kept by relays,
//! servers anyone can run, reached over WebSocket (`wss://`). The same post
//! is usually on many relays, and none of them is its home: a post is named
//! by itself (NIP-19, NIP-21), `nostr:note1…` (by its ID), `nostr:naddr1…`
//! (an article or wiki entry, by author, kind and name: the newest version)
//! or `nostr:npub1…` (a profile, by its key).
//!
//! What's saved: notes (kind 1) and comments (1111), long-form articles
//! (30023), wiki entries (30818) and profiles (0, with the author's newest
//! posts and relays as its links). Each post's signature is checked, so no
//! relay can make up a post in someone's name; posts that ask to expire
//! (NIP-40) aren't saved at all.
//!
//! How it's crawled: each relay is listed, newest first, 200 posts at a
//! time, as pages that are followed, not saved: `wss://RELAY/?kinds=1`, then
//! `…&until=TIME` for the posts before. A listing's posts are kept here until
//! the crawl gets to them, so they're fetched once. Authors lead to their
//! profiles, profiles to their relays (NIP-65), posts to what they mention
//! and answer (ARCHITECTURE.md §15.9).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use k256::schnorr::{Signature, VerifyingKey};
use k256::sha2::{Digest, Sha256};
use lost::urls::Url;
use rustls::ClientConfig;
use serde_json::{Value, json};

use crate::crawl::{
    Blacklist, Link, Parsed, Protocol, Response, Route, encode_query, mentions, percent_decode,
};
use crate::http::Http;
use crate::net::{self, Proxy};
use crate::websocket::WebSocket;

pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Where a crawl starts: big, general relays.
const SEEDS: &[&str] = &[
    "wss://relay.damus.io/",
    "wss://nos.lol/",
    "wss://relay.primal.net/",
    "wss://relay.snort.social/",
    "wss://nostr.mom/",
    "wss://purplepag.es/",
];
/// With `--search`: the seeds, and relays known to search (NIP-50). Any
/// other relay that says it can (its NIP-11 document) is searched too.
const SEEDS_AND_SEARCH: &[&str] = &[
    "wss://relay.damus.io/",
    "wss://nos.lol/",
    "wss://relay.primal.net/",
    "wss://relay.snort.social/",
    "wss://nostr.mom/",
    "wss://purplepag.es/",
    "wss://search.nos.today/",
    "wss://relay.nostr.band/",
    "wss://relay.ditto.pub/",
    "wss://relay.noswhere.com/",
];

const PROFILE: u32 = 0;
const NOTE: u32 = 1;
const DELETION: u32 = 5;
const REPOST: u32 = 6;
const COMMENT: u32 = 1111;
const RELAY_LIST: u32 = 10002;
const ARTICLE: u32 = 30023;
const WIKI: u32 = 30818;

/// What each relay is listed for, one listing apiece: page-like posts
/// (rarer, so not drowned among notes), and notes with the requests to
/// delete them (NIP-09: newer than what they delete, so listed first).
const LISTS: &[&str] = &["30023,30818", "1,5"];
/// What a search asks for: profiles, notes, articles, wiki entries.
const SEARCHED: &str = "0,1,30023,30818";
/// The kinds a listing can ask for.
const LISTABLE: &[u32] = &[PROFILE, NOTE, DELETION, ARTICLE, WIKI];
const LISTING_SIZE: usize = 200;
/// Search results, a page at a time (relays give fewer than for listings).
const SEARCH_SIZE: usize = 100;
/// Deletion requests read from a profile's author.
const DELETIONS_PER_PROFILE: usize = 200;
/// A profile's newest posts, of each sort, that become its links.
const POSTS_PER_PROFILE: usize = 50;

/// What the listing pages say they are; and the posts, worked out already.
const LISTING_TYPE: &str = "application/x-lost-spider-listing";
const PAGE_TYPE: &str = "application/x-lost-spider-nostr";

/// Posts from listings, kept until they're crawled (the oldest go first
/// past this: they're asked for again then).
const MAX_KEPT: usize = 50_000;
/// Names and where posts were seen, remembered up to this many.
const MAX_REMEMBERED: usize = 500_000;
/// Relays asked for a post before it's taken as gone.
const RELAYS_TRIED: usize = 3;
/// A relay that can't be reached this many times in a row isn't asked again.
const MAX_RELAY_FAILURES: u32 = 3;
/// An answer stops here, whatever was asked.
const MAX_EVENTS: usize = 1_000;
/// Notes with fewer letters and digits than this ("gm", "🤙", a link) aren't
/// saved; what they link to is still followed.
const MIN_NOTE_CHARS: usize = 10;
/// A note's title: its author, and its start up to this many characters.
const TITLE_CHARS: usize = 60;

pub struct Nostr {
    proxy: Option<Proxy>,
    timeout: Duration,
    /// Between two requests to the same relay (`--delay`).
    delay: Duration,
    tls: OnceLock<Result<Arc<ClientConfig>, String>>,
    relays: Mutex<HashMap<String, Arc<Relay>>>,
    kept: Mutex<Kept>,
    /// Authors' names, by public key (hex).
    names: Mutex<HashMap<String, String>>,
    /// The relay a post or profile was last seen on, by its `nostr:` URI.
    hints: Mutex<HashMap<String, String>>,
    subscriptions: AtomicU64,
    /// `--search`'s phrases, asked of the relays that can search.
    searches: Vec<String>,
    /// For relays' information documents (NIP-11), over HTTP.
    http: Http,
    /// Authors whose posts aren't saved (`--blacklist`), public keys (hex).
    blocked: Mutex<HashSet<String>>,
    /// Posts their authors asked to delete (NIP-09): IDs and addresses
    /// (`KIND:PUBKEY:NAME`), each with who asked (`TARGET PUBKEY`): only the
    /// author's own request counts.
    deleted: Mutex<HashSet<String>>,
}

#[derive(Default)]
struct Relay {
    state: Mutex<RelayState>,
    /// What it says about itself (NIP-11), asked once.
    info: OnceLock<Info>,
}

/// What matters of a relay's information document.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Info {
    /// It searches (NIP-50).
    searches: bool,
    /// Nothing can be read without logging in or paying.
    closed: bool,
}

impl Info {
    fn of(document: &Value) -> Info {
        let searches = document["supported_nips"]
            .as_array()
            .is_some_and(|nips| nips.iter().any(|n| n.as_u64() == Some(50)));
        let limits = &document["limitation"];
        let closed = limits["auth_required"].as_bool() == Some(true)
            || limits["payment_required"].as_bool() == Some(true);
        Info { searches, closed }
    }
}

struct RelayState {
    socket: Option<WebSocket>,
    /// When it may be asked again.
    next: Instant,
    /// Connections that failed in a row.
    failures: u32,
}

impl Default for RelayState {
    fn default() -> Self {
        RelayState {
            socket: None,
            next: Instant::now(),
            failures: 0,
        }
    }
}

/// Posts from listings, by URI, with the relay they came from; the oldest
/// first out.
#[derive(Default)]
struct Kept {
    events: HashMap<String, (Event, String)>,
    order: VecDeque<String>,
}

impl Kept {
    fn keep(&mut self, uri: String, event: Event, relay: &str) {
        if self.events.contains_key(&uri) {
            return;
        }
        // Those taken already leave their place behind: cleared out now
        // and then.
        if self.order.len() >= 2 * MAX_KEPT {
            let events = &self.events;
            self.order.retain(|u| events.contains_key(u));
        }
        while self.events.len() >= MAX_KEPT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.events.remove(&oldest);
        }
        self.order.push_back(uri.clone());
        self.events.insert(uri, (event, relay.to_string()));
    }

    fn take(&mut self, uri: &str) -> Option<(Event, String)> {
        // Its place in `order` goes when it comes up: nothing's there then.
        self.events.remove(uri)
    }
}

/// A relay's answer: the events it sent, and why it stopped early if it did
/// (`CLOSED`: "rate-limited: …", "auth-required: …").
struct Answer {
    events: Vec<Event>,
    closed: Option<String>,
}

impl Nostr {
    pub fn new(
        proxy: Option<Proxy>,
        timeout: Option<Duration>,
        delay: Duration,
        searches: Vec<String>,
    ) -> lost::Result<Nostr> {
        if let Some(Proxy::Http(_)) = proxy {
            return Err("lost-spider nostr goes through socks5:// proxies only".into());
        }
        let timeout = timeout.unwrap_or(TIMEOUT);
        Ok(Nostr {
            http: Http::new(proxy.clone(), timeout)?,
            searches,
            blocked: Mutex::new(HashSet::new()),
            deleted: Mutex::new(HashSet::new()),
            proxy,
            timeout,
            delay,
            tls: OnceLock::new(),
            relays: Mutex::new(HashMap::new()),
            kept: Mutex::new(Kept::default()),
            names: Mutex::new(HashMap::new()),
            hints: Mutex::new(HashMap::new()),
            subscriptions: AtomicU64::new(0),
        })
    }

    /// What the relay at `key` says about itself (NIP-11: its address over
    /// HTTP, asking for `application/nostr+json`); nothing if it won't say.
    fn info(&self, key: &str) -> Info {
        *self.relay(key).info.get_or_init(|| {
            let Some(mut url) = Url::parse(key) else {
                return Info::default();
            };
            url.scheme = if url.scheme == "ws" { "http" } else { "https" }.into();
            let route = match &self.proxy {
                Some(_) => Some(Route::Proxied),
                None => {
                    use std::net::ToSocketAddrs;
                    let port = url
                        .port
                        .unwrap_or(if url.scheme == "https" { 443 } else { 80 });
                    (url.host.as_str(), port)
                        .to_socket_addrs()
                        .ok()
                        .and_then(|mut a| a.next())
                        .map(Route::Direct)
                }
            };
            let answer = route
                .and_then(|route| self.http.get_as(route, &url, "application/nostr+json").ok());
            match answer {
                Some(answer) if answer.status == 200 => serde_json::from_slice(&answer.body)
                    .map(|document: Value| Info::of(&document))
                    .unwrap_or_default(),
                _ => Info::default(),
            }
        })
    }

    /// Takes the authors named in `blacklist`; how many there are.
    fn block_authors(&self, blacklist: &Blacklist) -> usize {
        let mut blocked = self.blocked.lock().unwrap_or_else(|e| e.into_inner());
        for name in blacklist.names() {
            let entity = name.strip_prefix("nostr:").unwrap_or(name);
            if let Some(Entity::Profile { pubkey, .. }) = Entity::decode(entity) {
                blocked.insert(hex(&pubkey));
            }
        }
        blocked.len()
    }

    /// A post that may be saved: signed, not expiring, not by a blocked
    /// author, not deleted by its author.
    fn wanted(&self, event: &Event) -> bool {
        if !usable(event) {
            return false;
        }
        if self
            .blocked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&event.pubkey)
        {
            return false;
        }
        let deleted = self.deleted.lock().unwrap_or_else(|e| e.into_inner());
        let by_author = |target: &str| deleted.contains(&format!("{target} {}", event.pubkey));
        !(by_author(&event.id) || event.coordinate().is_some_and(|a| by_author(&a)))
    }

    /// Takes in the deletion requests among `events` (checked: signed).
    fn note_deletions(&self, events: &[Event]) {
        let mut deleted = self.deleted.lock().unwrap_or_else(|e| e.into_inner());
        for event in events.iter().filter(|e| e.kind == DELETION && usable(e)) {
            if deleted.len() >= MAX_REMEMBERED {
                deleted.clear();
            }
            for tag in &event.tags {
                if let (Some(name), Some(value)) = (tag.first(), tag.get(1))
                    && (name == "e" || name == "a")
                {
                    deleted.insert(format!("{value} {}", event.pubkey));
                }
            }
        }
    }

    fn relay(&self, key: &str) -> Arc<Relay> {
        let mut relays = self.relays.lock().unwrap_or_else(|e| e.into_inner());
        relays.entry(key.to_string()).or_default().clone()
    }

    fn connect(&self, key: &str) -> Result<WebSocket, String> {
        let url = Url::parse(key).ok_or_else(|| format!("\"{key}\" isn't a relay"))?;
        let tls = match url.scheme.as_str() {
            "wss" => Some(self.tls.get_or_init(net::any_certificate).clone()?),
            _ => None,
        };
        WebSocket::connect(&url, self.proxy.as_ref(), self.timeout, tls.as_ref())
    }

    /// Asks the relay at `key` for the events matching `filters`, once
    /// it's had `--delay` since the last time. A connection that was open
    /// already and fails is opened again, once.
    fn ask(&self, key: &str, filters: &[Value]) -> Result<Answer, String> {
        let relay = self.relay(key);
        let mut state = relay.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failures >= MAX_RELAY_FAILURES {
            return Err("the relay can't be reached".into());
        }
        let wait = state.next.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        let result = loop {
            let reused = state.socket.is_some();
            if !reused {
                match self.connect(key) {
                    Ok(socket) => {
                        state.socket = Some(socket);
                        state.failures = 0;
                    }
                    Err(e) => {
                        state.failures += 1;
                        break Err(e);
                    }
                }
            }
            let socket = state.socket.as_mut().expect("connected");
            match self.subscribe(socket, filters) {
                Ok(answer) => break Ok(answer),
                Err(e) => {
                    state.socket = None;
                    if !reused {
                        break Err(e);
                    }
                }
            }
        };
        state.next = Instant::now() + self.delay;
        result
    }

    /// One subscription: `REQ`, the events up to `EOSE` (or `CLOSED`),
    /// `CLOSE`.
    fn subscribe(&self, socket: &mut WebSocket, filters: &[Value]) -> Result<Answer, String> {
        let id = format!("lost{}", self.subscriptions.fetch_add(1, Ordering::Relaxed));
        let mut request = vec![json!("REQ"), json!(id)];
        request.extend(filters.iter().cloned());
        socket.send(&Value::Array(request).to_string())?;
        let deadline = Instant::now() + self.timeout * 4;
        let mut events = Vec::new();
        loop {
            let text = socket.receive(deadline)?;
            let Ok(Value::Array(message)) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let ours = message.get(1).and_then(Value::as_str) == Some(id.as_str());
            match message.first().and_then(Value::as_str) {
                Some("EVENT") if ours => {
                    if let Some(event) = message.get(2).and_then(Event::from_json) {
                        events.push(event);
                    }
                    if events.len() >= MAX_EVENTS {
                        socket.send(&json!(["CLOSE", id]).to_string())?;
                        return Ok(Answer {
                            events,
                            closed: None,
                        });
                    }
                }
                Some("EOSE") if ours => {
                    socket.send(&json!(["CLOSE", id]).to_string())?;
                    return Ok(Answer {
                        events,
                        closed: None,
                    });
                }
                Some("CLOSED") if ours => {
                    let reason = message.get(2).and_then(Value::as_str).unwrap_or("");
                    return Ok(Answer {
                        events,
                        closed: Some(reason.to_string()),
                    });
                }
                // NOTICE, AUTH (no logging in here), answers to others.
                _ => {}
            }
        }
    }

    /// Relays to ask for `uri`: where it was seen, those it names, and some
    /// of the seeds (a different few for each, to share the asking out).
    fn relays_for(&self, uri: &str, named: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let seen = self
            .hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
            .cloned();
        let start = uri.bytes().map(usize::from).sum::<usize>() % SEEDS.len();
        let seeds = (0..SEEDS.len()).map(|i| SEEDS[(start + i) % SEEDS.len()].to_string());
        for relay in seen
            .into_iter()
            .chain(named.iter().filter_map(|r| relay_key(r)))
            .chain(seeds)
        {
            if !out.contains(&relay) {
                out.push(relay);
            }
        }
        out.truncate(RELAYS_TRIED);
        out
    }

    /// The newest event matching `filters` on the relays for `uri`, and the
    /// relay it came from; `None` if none of them has one.
    fn look_up(
        &self,
        uri: &str,
        named: &[String],
        filters: &[Value],
    ) -> Result<Option<(Event, String)>, String> {
        let mut problem = None;
        let mut answered = false;
        for relay in self.relays_for(uri, named) {
            match self.ask(&relay, filters) {
                Ok(answer) => {
                    answered = true;
                    self.note_deletions(&answer.events);
                    let best = answer
                        .events
                        .into_iter()
                        .filter(|e| e.kind != DELETION && self.wanted(e))
                        .max_by_key(|e| e.created_at);
                    if let Some(event) = best {
                        return Ok(Some((event, relay)));
                    }
                }
                Err(e) => problem = Some(e),
            }
        }
        match problem {
            Some(e) if !answered => Err(e),
            _ => Ok(None),
        }
    }

    fn remember_name(&self, event: &Event) {
        if event.kind != PROFILE {
            return;
        }
        let Some(name) = Profile::of(event).name() else {
            return;
        };
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        if names.len() >= MAX_REMEMBERED {
            names.clear();
        }
        names.insert(event.pubkey.clone(), name);
    }

    fn remember_hint(&self, uri: &str, relay: &str) {
        let mut hints = self.hints.lock().unwrap_or_else(|e| e.into_inner());
        if hints.len() >= MAX_REMEMBERED {
            hints.clear();
        }
        hints.insert(uri.to_string(), relay.to_string());
    }

    fn name_of(&self, pubkey: &str) -> Option<String> {
        self.names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(pubkey)
            .cloned()
    }

    /// `wss://RELAY/`: its listings, and its searches for `--search`'s
    /// phrases if it can search. `wss://RELAY/?kinds=…&until=…` (and
    /// `&search=…`): a page of one, newest first.
    fn listing(&self, url: &Url) -> Result<Response, String> {
        let Some(relay) = relay_key(&url.to_string()) else {
            return Ok(Response::Gone);
        };
        let Some(query) = &url.query else {
            let info = self.info(&relay);
            if info.closed {
                return Ok(Response::Gone);
            }
            let mut links: Vec<Link> = LISTS
                .iter()
                .map(|kinds| Link {
                    url: format!("{relay}?kinds={kinds}"),
                    label: None,
                })
                .collect();
            if info.searches {
                for phrase in &self.searches {
                    links.push(Link {
                        url: format!("{relay}?kinds={SEARCHED}&search={}", encode_query(phrase)),
                        label: None,
                    });
                }
            }
            return Ok(page(LISTING_TYPE, None, String::new(), links, true));
        };
        let Some(list) = List::of(query) else {
            return Ok(Response::Gone);
        };
        let size = if list.search.is_some() {
            SEARCH_SIZE
        } else {
            LISTING_SIZE
        };
        let mut filter = json!({ "kinds": list.kinds, "limit": size });
        if let Some(until) = list.until {
            filter["until"] = json!(until);
        }
        if let Some(search) = &list.search {
            filter["search"] = json!(search);
        }
        let answer = self.ask(&relay, &[filter])?;
        if let Some(reason) = &answer.closed
            && answer.events.is_empty()
        {
            return Ok(refused(reason));
        }
        let oldest = answer.events.iter().map(|e| e.created_at).min();
        self.note_deletions(&answer.events);
        let events: Vec<Event> = answer
            .events
            .into_iter()
            .filter(|e| e.kind != DELETION && self.wanted(e))
            .collect();

        // Names for the notes' titles: those not known yet, in one go.
        let unknown: HashSet<&str> = events
            .iter()
            .map(|e| e.pubkey.as_str())
            .filter(|p| self.name_of(p).is_none())
            .collect();
        if !unknown.is_empty() {
            let authors: Vec<&str> = unknown.into_iter().collect();
            let filter = json!({ "kinds": [PROFILE], "authors": authors });
            if let Ok(answer) = self.ask(&relay, &[filter]) {
                for profile in answer.events.iter().filter(|e| usable(e)) {
                    self.remember_name(profile);
                }
            }
        }

        let mut links = Vec::new();
        {
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            for event in events {
                let Some(uri) = event.uri() else {
                    continue;
                };
                self.remember_hint(&uri, &relay);
                // A profile (found by a search) is asked for whole later.
                if event.kind == PROFILE {
                    self.remember_name(&event);
                    let label = self.name_of(&event.pubkey);
                    links.push(Link { url: uri, label });
                    continue;
                }
                let label = event.title_tag().map(str::to_string);
                kept.keep(uri.clone(), event, &relay);
                links.push(Link { url: uri, label });
            }
        }
        if let Some(oldest) = oldest {
            // Posts of the same second as the last page's oldest may not
            // all have fit: that second again, unless it's all there was.
            let until = if list.until == Some(oldest) {
                oldest.checked_sub(1)
            } else {
                Some(oldest)
            };
            if let Some(until) = until {
                links.push(Link {
                    url: format!(
                        "{relay}?{}",
                        List {
                            until: Some(until),
                            ..list
                        }
                        .query()
                    ),
                    label: None,
                });
            }
        }
        Ok(page(LISTING_TYPE, None, String::new(), links, true))
    }

    /// A profile: who it is, and its newest posts and relays as links.
    fn profile(&self, uri: &str, pubkey: &str, named: &[String]) -> Result<Response, String> {
        let filters = [
            json!({ "kinds": [PROFILE, RELAY_LIST], "authors": [pubkey] }),
            json!({ "kinds": [ARTICLE, WIKI], "authors": [pubkey], "limit": POSTS_PER_PROFILE }),
            json!({ "kinds": [NOTE], "authors": [pubkey], "limit": POSTS_PER_PROFILE }),
            json!({ "kinds": [DELETION], "authors": [pubkey], "limit": DELETIONS_PER_PROFILE }),
        ];
        let mut problem = None;
        let mut best: Option<(Vec<Event>, String)> = None;
        for relay in self.relays_for(uri, named) {
            match self.ask(&relay, &filters) {
                Ok(answer) => {
                    self.note_deletions(&answer.events);
                    let events: Vec<Event> = answer
                        .events
                        .into_iter()
                        .filter(|e| e.pubkey == pubkey && e.kind != DELETION && self.wanted(e))
                        .collect();
                    let has_profile = events.iter().any(|e| e.kind == PROFILE);
                    let better = best.as_ref().is_none_or(|(b, _)| events.len() > b.len());
                    if has_profile || better {
                        best = Some((events, relay));
                    }
                    if has_profile {
                        break;
                    }
                }
                Err(e) => problem = Some(e),
            }
        }
        let Some((events, relay)) = best else {
            return match problem {
                Some(e) => Err(e),
                None => Ok(Response::Gone),
            };
        };
        if events.is_empty() {
            return Ok(Response::Gone);
        }
        let newest = |kind: u32| {
            events
                .iter()
                .filter(|e| e.kind == kind)
                .max_by_key(|e| e.created_at)
        };
        let profile = newest(PROFILE).map(Profile::of).unwrap_or_default();
        if let Some(event) = newest(PROFILE) {
            self.remember_name(event);
        }
        let mut links = Vec::new();
        if let Some(website) = profile.website() {
            links.push(Link {
                url: website,
                label: None,
            });
        }
        if let Some(list) = newest(RELAY_LIST) {
            for tag in &list.tags {
                if tag.first().map(String::as_str) == Some("r")
                    && let Some(relay) = tag.get(1).and_then(|r| relay_key(r))
                {
                    links.push(Link {
                        url: relay,
                        label: None,
                    });
                }
            }
        }
        {
            let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            let mut posts: Vec<&Event> = events
                .iter()
                .filter(|e| matches!(e.kind, NOTE | ARTICLE | WIKI))
                .collect();
            posts.sort_by_key(|e| std::cmp::Reverse(e.created_at));
            for post in posts {
                let Some(post_uri) = post.uri() else {
                    continue;
                };
                self.remember_hint(&post_uri, &relay);
                links.push(Link {
                    url: post_uri.clone(),
                    label: post.title_tag().map(str::to_string),
                });
                kept.keep(post_uri, post.clone(), &relay);
            }
        }
        let Some(name) = profile.name() else {
            // No profile: only its posts, followed.
            return Ok(page(PAGE_TYPE, None, String::new(), links, true));
        };
        Ok(page(PAGE_TYPE, Some(name), profile.text(), links, false))
    }

    /// A post (seen on `relay`) as a page.
    fn post(&self, event: &Event, relay: &str) -> Response {
        let author = self
            .name_of(&event.pubkey)
            .unwrap_or_else(|| short(&npub(&event.pubkey)));
        let author_uri = npub_uri(&event.pubkey);
        // Its author's profile is looked for where the post was.
        self.remember_hint(&author_uri, relay);
        let mut links = vec![Link {
            url: author_uri,
            label: Some(author.clone()),
        }];
        for (uri, named) in tag_links(event) {
            if let Some(relay) = named {
                self.remember_hint(&uri, &relay);
            }
            if !links.iter().any(|l| l.url == uri) {
                links.push(Link {
                    url: uri,
                    label: None,
                });
            }
        }
        // URLs are read from what's shown (not an article's image sources).
        let (title, body, skip) = match event.kind {
            NOTE | COMMENT => {
                let text = self.with_names(&event.content);
                let start = lost::text::single_line(&text);
                let title = format!("{author}: {}", cut(&start, TITLE_CHARS));
                let words = strip_references(&event.content)
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .count();
                (title, text, words < MIN_NOTE_CHARS)
            }
            ARTICLE => {
                let (text, written) = markdown_to_text(&event.content);
                links.extend(written);
                let title = event
                    .title_tag()
                    .map(str::to_string)
                    .unwrap_or_else(|| cut(&lost::text::single_line(&text), TITLE_CHARS));
                let body = match event.tag("summary").filter(|s| !s.trim().is_empty()) {
                    Some(summary) => format!("{}\n\n{text}", summary.trim()),
                    None => text,
                };
                (title, with_topics(body, event), false)
            }
            WIKI => {
                let text = asciidoc_to_text(&event.content);
                let title = event
                    .title_tag()
                    .or_else(|| event.tag("d"))
                    .map(str::to_string)
                    .unwrap_or_else(|| cut(&lost::text::single_line(&text), TITLE_CHARS));
                (title, with_topics(text, event), false)
            }
            // A repost: what it reposts.
            REPOST => (String::new(), String::new(), true),
            _ => return Response::Gone,
        };
        // References that say where to look: remembered.
        for (_, _, entity) in references(&event.content) {
            let named = match &entity {
                Entity::Profile { relays, .. }
                | Entity::Event { relays, .. }
                | Entity::Address { relays, .. } => relays.iter().find_map(|r| relay_key(r)),
            };
            if let Some(relay) = named {
                self.remember_hint(&entity.uri(), &relay);
            }
        }
        let mut found = nostr_links(&event.content);
        let shown = if event.kind == ARTICLE {
            &body
        } else {
            &event.content
        };
        found.extend(mentions(shown, "https"));
        found.extend(mentions(shown, "http"));
        for link in found {
            if !links.iter().any(|l| l.url == link.url) {
                links.push(link);
            }
        }
        if skip {
            return page(PAGE_TYPE, None, String::new(), links, true);
        }
        page(PAGE_TYPE, Some(title), body, links, false)
    }

    /// `nostr:npub1…` mentions written as `@name`, for the names known.
    fn with_names(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for (start, end, entity) in references(text) {
            if let Entity::Profile { pubkey, .. } = entity
                && let Some(name) = self.name_of(&hex(&pubkey))
            {
                out.push_str(&text[last..start]);
                out.push('@');
                out.push_str(&name);
                last = end;
            }
        }
        out.push_str(&text[last..]);
        out
    }
}

/// A listing's page: what it asks for.
#[derive(Debug, PartialEq)]
struct List {
    kinds: Vec<u32>,
    until: Option<u64>,
    /// A search's words (NIP-50).
    search: Option<String>,
}

impl List {
    fn of(query: &str) -> Option<List> {
        let mut kinds = None;
        let mut until = None;
        let mut search = None;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("search", value)) => {
                    search = Some(percent_decode(value)).filter(|s| !s.trim().is_empty());
                    search.as_ref()?;
                }
                Some(("kinds", value)) => {
                    let list: Option<Vec<u32>> = value
                        .split([',', '+'])
                        .map(|k| k.parse().ok().filter(|k| LISTABLE.contains(k)))
                        .collect();
                    kinds = Some(list?);
                }
                Some(("until", value)) => until = Some(value.parse().ok()?),
                _ => return None,
            }
        }
        let kinds = kinds.filter(|k| !k.is_empty())?;
        Some(List {
            kinds,
            until,
            search,
        })
    }

    /// Written as a query again: `kinds=…`, then `search=…`, `until=…`.
    fn query(&self) -> String {
        let kinds: Vec<String> = self.kinds.iter().map(u32::to_string).collect();
        let mut query = format!("kinds={}", kinds.join(","));
        if let Some(search) = &self.search {
            query.push_str(&format!("&search={}", encode_query(search)));
        }
        if let Some(until) = self.until {
            query.push_str(&format!("&until={until}"));
        }
        query
    }
}

/// What a relay's `CLOSED` means for the page that asked.
fn refused(reason: &str) -> Response {
    if reason.starts_with("rate-limited") {
        Response::SlowDown(Duration::from_secs(60))
    } else {
        Response::Gone
    }
}

/// A page worked out here, for [`Protocol::parse`] to read back.
fn page(mime: &str, title: Option<String>, body: String, links: Vec<Link>, skip: bool) -> Response {
    let links: Vec<Value> = links.into_iter().map(|l| json!([l.url, l.label])).collect();
    let page = json!({ "title": title, "body": body, "links": links, "skip": skip });
    Response::Document {
        mime: mime.to_string(),
        body: Some(page.to_string().into_bytes()),
    }
}

impl Protocol for Nostr {
    const NAME: &'static str = "nostr";
    /// `--link npub1…`: `nostr:npub1…`.
    const SCHEME: &'static str = "nostr";
    const SPACE: &'static str = "Nostr";
    const HOSTS: &'static str = "items";

    fn seeds(&self) -> &'static [&'static str] {
        if self.searches.is_empty() {
            SEEDS
        } else {
            SEEDS_AND_SEARCH
        }
    }

    fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }

    /// Every post, profile and relay is a server of its own to the crawl:
    /// posts are mostly kept here from listings already, and relays are
    /// asked one thing at a time here (`--delay`).
    fn proxied(&self) -> bool {
        true
    }

    fn accepts(url: &Url) -> bool {
        match url.scheme.as_str() {
            "ws" | "wss" => !url.host.is_empty(),
            "nostr" => Entity::decode(&url.host).is_some_and(|e| e.uri() == url.to_string()),
            _ => false,
        }
    }

    /// Relays are servers for programs, and posts aren't sites.
    fn has_robots(_origin: &Url) -> bool {
        false
    }

    /// `nostr:nevent1…`, `nprofile1…`, a bare `npub1…`: the way posts and
    /// profiles are saved (`note1…`, `npub1…`, `naddr1…` without relays).
    fn rewrite(link: &str) -> Option<String> {
        let url = Url::parse(link)?;
        if url.scheme != "nostr" {
            return None;
        }
        Entity::decode(&url.host).map(|e| e.uri())
    }

    fn fallback_title(url: &Url) -> String {
        short(&url.host)
    }

    fn fetch(&self, _route: Route, url: &Url) -> Result<Response, String> {
        if url.scheme == "ws" || url.scheme == "wss" {
            return self.listing(url);
        }
        let Some(entity) = Entity::decode(&url.host) else {
            return Ok(Response::Gone);
        };
        let uri = entity.uri();
        if uri != url.to_string() {
            return Ok(Response::Redirect(uri));
        }
        match &entity {
            Entity::Profile { pubkey, relays } => self.profile(&uri, &hex(pubkey), relays),
            Entity::Event { id, relays, .. } => {
                let kept = self
                    .kept
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(&uri);
                let found = match kept {
                    // Its author may have asked to delete it since.
                    Some((event, _)) if !self.wanted(&event) => None,
                    Some(found) => Some(found),
                    None => self.look_up(
                        &uri,
                        relays,
                        &[
                            json!({ "ids": [hex(id)] }),
                            json!({ "kinds": [DELETION], "#e": [hex(id)] }),
                        ],
                    )?,
                };
                let Some((event, relay)) = found else {
                    return Ok(Response::Gone);
                };
                // Saved under its own name: an article's, a profile's.
                match event.uri() {
                    Some(own) if own != uri => {
                        self.kept.lock().unwrap_or_else(|e| e.into_inner()).keep(
                            own.clone(),
                            event,
                            &relay,
                        );
                        Ok(Response::Redirect(own))
                    }
                    Some(_) => Ok(self.post(&event, &relay)),
                    None => Ok(Response::Gone),
                }
            }
            Entity::Address {
                kind,
                author,
                identifier,
                relays,
            } => {
                let kept = self
                    .kept
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(&uri);
                let coordinate = format!("{kind}:{}:{identifier}", hex(author));
                let found = match kept {
                    Some((event, _)) if !self.wanted(&event) => None,
                    Some(found) => Some(found),
                    None => self.look_up(
                        &uri,
                        relays,
                        &[
                            json!({ "kinds": [kind], "authors": [hex(author)], "#d": [identifier] }),
                            json!({ "kinds": [DELETION], "#a": [coordinate] }),
                        ],
                    )?,
                };
                match found {
                    Some((event, relay)) => Ok(self.post(&event, &relay)),
                    None => Ok(Response::Gone),
                }
            }
        }
    }

    fn parse(&self, _url: &Url, mime: &str, body: &[u8]) -> Parsed {
        if mime != PAGE_TYPE && mime != LISTING_TYPE {
            return Parsed::default();
        }
        let Ok(page) = serde_json::from_slice::<Value>(body) else {
            return Parsed::default();
        };
        let links = page["links"]
            .as_array()
            .map(|links| {
                links
                    .iter()
                    .filter_map(|link| {
                        Some(Link {
                            url: link[0].as_str()?.to_string(),
                            label: link[1].as_str().map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Parsed {
            title: page["title"].as_str().map(str::to_string),
            body: page["body"].as_str().unwrap_or("").to_string(),
            links,
            skip: page["skip"].as_bool().unwrap_or(true),
        }
    }

    /// Some relay must answer. Authors in `--blacklist` (`npub1…`,
    /// `nostr:npub1…`, `nprofile1…`) have their posts skipped.
    fn prepare(&self, blacklist: &mut Blacklist) -> lost::Result<()> {
        match self.block_authors(blacklist) {
            0 => {}
            1 => eprintln!("Skipping the posts of 1 blacklisted author."),
            n => eprintln!("Skipping the posts of {n} blacklisted authors."),
        }
        if !self.searches.is_empty() {
            eprintln!(
                "Searching for {} on the relays that can search (NIP-50).",
                match self.searches.len() {
                    1 => "1 phrase".to_string(),
                    n => format!("{n} phrases"),
                }
            );
        }
        let reached = std::thread::scope(|scope| {
            let tries: Vec<_> = SEEDS
                .iter()
                .map(|seed| scope.spawn(move || (seed, self.connect(seed))))
                .collect();
            let mut reached = 0;
            let mut problem = None;
            for attempt in tries {
                let Ok((seed, result)) = attempt.join() else {
                    continue;
                };
                match result {
                    Ok(socket) => {
                        let relay = self.relay(seed);
                        relay.state.lock().unwrap_or_else(|e| e.into_inner()).socket = Some(socket);
                        reached += 1;
                    }
                    Err(e) => problem = Some(format!("{seed}: {e}")),
                }
            }
            (reached, problem)
        });
        match reached {
            (0, problem) => Err(format!(
                "can't reach any Nostr relay ({})",
                problem.unwrap_or_default()
            )
            .into()),
            (n, _) => {
                eprintln!("Reached {n} of {} relays.", SEEDS.len());
                Ok(())
            }
        }
    }
}

// ── Posts ───────────────────────────────────────────────────────────────

/// A post (NIP-01).
#[derive(Debug, Clone, PartialEq)]
struct Event {
    /// Hex.
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
}

impl Event {
    fn from_json(value: &Value) -> Option<Event> {
        let text = |name: &str| value[name].as_str().map(str::to_string);
        let tags = value["tags"]
            .as_array()?
            .iter()
            .map(|tag| {
                tag.as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<String>>>()
            })
            .collect::<Option<Vec<Vec<String>>>>()?;
        Some(Event {
            id: text("id")?,
            pubkey: text("pubkey")?,
            created_at: value["created_at"].as_u64()?,
            kind: u32::try_from(value["kind"].as_u64()?).ok()?,
            tags,
            content: text("content")?,
            sig: text("sig")?,
        })
    }

    /// Whether the ID is the post's (the SHA-256 of what NIP-01 says to
    /// write out) and its author signed it (BIP-340).
    fn is_signed(&self) -> bool {
        let (Some(id), Some(pubkey), Some(sig)) = (
            unhex::<32>(&self.id),
            unhex::<32>(&self.pubkey),
            unhex::<64>(&self.sig),
        ) else {
            return false;
        };
        let written = json!([
            0,
            self.pubkey,
            self.created_at,
            self.kind,
            self.tags,
            self.content
        ])
        .to_string();
        if Sha256::digest(written.as_bytes()).as_slice() != id.as_slice() {
            return false;
        }
        let (Ok(key), Ok(signature)) = (
            VerifyingKey::from_slice(&pubkey),
            Signature::from_slice(&sig),
        ) else {
            return false;
        };
        key.verify_raw(&id, &signature).is_ok()
    }

    /// The first value of the first tag named `name`.
    fn tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|t| t.first().map(String::as_str) == Some(name))
            .and_then(|t| t.get(1))
            .map(String::as_str)
    }

    /// An article's or wiki entry's address, `KIND:PUBKEY:NAME`, as
    /// deletion requests write it.
    fn coordinate(&self) -> Option<String> {
        (30_000..40_000).contains(&self.kind).then(|| {
            format!(
                "{}:{}:{}",
                self.kind,
                self.pubkey,
                self.tag("d").unwrap_or("")
            )
        })
    }

    fn title_tag(&self) -> Option<&str> {
        self.tag("title").filter(|t| !t.trim().is_empty())
    }

    /// The post's `nostr:` URI, as it's saved: a profile's key, an
    /// article's or wiki entry's address (kind, author, name), any other
    /// post's ID.
    fn uri(&self) -> Option<String> {
        let pubkey = unhex::<32>(&self.pubkey)?;
        let entity = if self.kind == PROFILE {
            Entity::Profile {
                pubkey,
                relays: Vec::new(),
            }
        } else if (30_000..40_000).contains(&self.kind) {
            Entity::Address {
                kind: self.kind,
                author: pubkey,
                identifier: self.tag("d").unwrap_or("").to_string(),
                relays: Vec::new(),
            }
        } else {
            Entity::Event {
                id: unhex::<32>(&self.id)?,
                relays: Vec::new(),
                author: None,
                kind: None,
            }
        };
        // An address whose name is too long for one (over 255 bytes) has none.
        let uri = entity.uri();
        Entity::decode(uri.strip_prefix("nostr:")?).map(|_| uri)
    }
}

/// Signed, and not asking to expire (NIP-40: whatever the date, it was
/// meant to go away, so it isn't kept in a file for good).
fn usable(event: &Event) -> bool {
    event.tag("expiration").is_none() && event.is_signed()
}

/// A profile's fields (kind 0's content, JSON).
#[derive(Debug, Default)]
struct Profile {
    name: Option<String>,
    display_name: Option<String>,
    about: Option<String>,
    website: Option<String>,
    nip05: Option<String>,
}

impl Profile {
    fn of(event: &Event) -> Profile {
        let Ok(fields) = serde_json::from_str::<Value>(&event.content) else {
            return Profile::default();
        };
        let field = |name: &str| {
            fields[name]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        Profile {
            name: field("name"),
            display_name: field("display_name").or_else(|| field("displayName")),
            about: field("about"),
            website: field("website"),
            nip05: field("nip05"),
        }
    }

    fn name(&self) -> Option<String> {
        self.display_name
            .clone()
            .or_else(|| self.name.clone())
            .map(|n| lost::text::single_line(&n))
            .filter(|n| !n.is_empty())
    }

    fn website(&self) -> Option<String> {
        let site = self.website.as_deref()?;
        let site = if site.contains("://") {
            site.to_string()
        } else {
            format!("https://{site}")
        };
        Url::parse(&site)
            .filter(|u| u.scheme == "http" || u.scheme == "https")
            .map(|u| u.to_string())
    }

    fn text(&self) -> String {
        let mut text = self.about.clone().unwrap_or_default();
        for extra in [&self.name, &self.nip05, &self.website]
            .into_iter()
            .flatten()
        {
            if !text.contains(extra.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(extra);
            }
        }
        text
    }
}

/// What a post's tags point at: profiles (`p`), posts (`e`, `q`) and
/// addresses (`a`), with the relay named there, if any.
fn tag_links(event: &Event) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for tag in &event.tags {
        let (Some(name), Some(value)) = (tag.first(), tag.get(1)) else {
            continue;
        };
        let relay = tag.get(2).and_then(|r| relay_key(r));
        let entity = match name.as_str() {
            "p" => unhex::<32>(value).map(|pubkey| Entity::Profile {
                pubkey,
                relays: Vec::new(),
            }),
            "e" | "q" => unhex::<32>(value).map(|id| Entity::Event {
                id,
                relays: Vec::new(),
                author: None,
                kind: None,
            }),
            "a" => address(value),
            _ => None,
        };
        // A `q` can be an address too.
        let entity = entity.or_else(|| (name == "q").then(|| address(value)).flatten());
        if let Some(uri) = entity.map(|e| e.uri())
            && !out.iter().any(|(u, _)| *u == uri)
        {
            out.push((uri, relay));
        }
    }
    out
}

/// `KIND:PUBKEY:NAME`, an `a` tag's address.
fn address(text: &str) -> Option<Entity> {
    let mut parts = text.splitn(3, ':');
    let kind = parts.next()?.parse().ok()?;
    let author = unhex::<32>(parts.next()?)?;
    let identifier = parts.next()?.to_string();
    Some(Entity::Address {
        kind,
        author,
        identifier,
        relays: Vec::new(),
    })
}

/// A relay's address as it's kept: `wss://HOST[:PORT]/PATH`, nothing after.
fn relay_key(text: &str) -> Option<String> {
    let mut url = Url::parse(text.trim())?;
    if url.scheme != "wss" && url.scheme != "ws" {
        return None;
    }
    url.query = None;
    url.fragment = None;
    Some(url.to_string())
}

// ── Text ────────────────────────────────────────────────────────────────

/// `nostr:` references written in `text`: where each is, and what it names.
fn references(text: &str) -> Vec<(usize, usize, Entity)> {
    let mut out = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("nostr:") {
        let start = from + i;
        let entity_start = start + "nostr:".len();
        let end = entity_start
            + lower[entity_start..]
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(lower.len() - entity_start);
        if let Some(entity) = Entity::decode(&lower[entity_start..end]) {
            out.push((start, end, entity));
        }
        from = end.max(entity_start);
    }
    out
}

/// The posts and profiles `text` refers to, as links.
fn nostr_links(text: &str) -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    for (_, _, entity) in references(text) {
        let url = entity.uri();
        if !links.iter().any(|l| l.url == url) {
            links.push(Link { url, label: None });
        }
    }
    links
}

/// `text` without its `nostr:` references and URLs: what's left to read.
fn strip_references(text: &str) -> String {
    text.split_whitespace()
        .filter(|w| {
            let w = w.to_ascii_lowercase();
            !w.contains("nostr:") && !w.contains("://")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Markdown as text to read, and its links: `[text](url)` keeps its text,
/// an image its description; `#`s, `**`, `` ` ``s and the like go.
fn markdown_to_text(markdown: &str) -> (String, Vec<Link>) {
    let mut links: Vec<Link> = Vec::new();
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            continue;
        }
        let line = trimmed.trim_start_matches('#').trim_start_matches('>');
        let line = if line.len() < trimmed.len() {
            line.trim_start()
        } else {
            line
        };
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(open) = rest.find('[') {
            let image = open > 0 && rest[..open].ends_with('!');
            let Some(close) = rest[open..].find("](").map(|c| open + c) else {
                break;
            };
            let Some(end) = rest[close + 2..].find(')').map(|e| close + 2 + e) else {
                break;
            };
            out.push_str(&rest[..if image { open - 1 } else { open }]);
            out.push_str(&rest[open + 1..close]);
            let target = rest[close + 2..end].split_whitespace().next().unwrap_or("");
            if !image
                && (target.starts_with("http://")
                    || target.starts_with("https://")
                    || target.starts_with("nostr:"))
            {
                let url = match Url::parse(target) {
                    Some(u) if u.scheme == "nostr" => Entity::decode(&u.host).map(|e| e.uri()),
                    Some(_) => Some(target.to_string()),
                    None => None,
                };
                if let Some(url) = url
                    && !links.iter().any(|l| l.url == url)
                {
                    links.push(Link {
                        url,
                        label: Some(rest[open + 1..close].to_string()).filter(|l| !l.is_empty()),
                    });
                }
            }
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        let out = out.replace("**", "").replace("__", "").replace("~~", "");
        lines.push(out.replace('`', ""));
    }
    (lines.join("\n").trim().to_string(), links)
}

/// AsciiDoc (wiki entries) as text to read: `=` headings and `[[links]]`
/// as their words.
fn asciidoc_to_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let line = if trimmed.starts_with('=') {
                trimmed.trim_start_matches('=').trim_start()
            } else {
                line
            };
            let mut out = String::with_capacity(line.len());
            let mut rest = line;
            while let Some(open) = rest.find("[[") {
                let Some(close) = rest[open..].find("]]").map(|c| open + c) else {
                    break;
                };
                out.push_str(&rest[..open]);
                let inside = &rest[open + 2..close];
                out.push_str(inside.rsplit('|').next().unwrap_or(inside));
                rest = &rest[close + 2..];
            }
            out.push_str(rest);
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `text`, and its post's topics (`t` tags) as `#hashtags` at the end, those
/// it doesn't have already: an article's topics are often only there.
fn with_topics(mut text: String, event: &Event) -> String {
    let lower = text.to_lowercase();
    let mut topics: Vec<String> = Vec::new();
    for tag in &event.tags {
        if let (Some("t"), Some(topic)) = (tag.first().map(String::as_str), tag.get(1)) {
            let topic = topic.trim().trim_start_matches('#');
            let hashtag = format!("#{topic}");
            if !topic.is_empty()
                && !topic.contains(char::is_whitespace)
                && !lower.contains(&hashtag.to_lowercase())
                && !topics.contains(&hashtag)
            {
                topics.push(hashtag);
            }
        }
    }
    if !topics.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&topics.join(" "));
    }
    text
}

/// `text` cut to `max` characters, at a word, with "…" if anything went.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let end = text.char_indices().nth(max).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    let head = match head.rfind(' ') {
        Some(i) if i > end / 2 => &head[..i],
        _ => head,
    };
    format!("{}…", head.trim_end())
}

/// `npub1abcdefgh…wxyz`: an entity short enough for a title.
fn short(entity: &str) -> String {
    let chars: Vec<char> = entity.chars().collect();
    if chars.len() <= 20 {
        return entity.to_string();
    }
    let head: String = chars[..12].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn npub(pubkey: &str) -> String {
    unhex::<32>(pubkey).map_or_else(|| pubkey.to_string(), |k| bech32_encode("npub", &k))
}

fn npub_uri(pubkey: &str) -> String {
    format!("nostr:{}", npub(pubkey))
}

// ── Names: NIP-19 ───────────────────────────────────────────────────────

/// What a `nostr:` URI names. Relays are only hints of where to look.
#[derive(Debug, Clone, PartialEq)]
enum Entity {
    /// `npub1…` / `nprofile1…`.
    Profile {
        pubkey: [u8; 32],
        relays: Vec<String>,
    },
    /// `note1…` / `nevent1…`.
    Event {
        id: [u8; 32],
        relays: Vec<String>,
        author: Option<[u8; 32]>,
        kind: Option<u32>,
    },
    /// `naddr1…`: the newest of an author's posts of a kind under a name.
    Address {
        kind: u32,
        author: [u8; 32],
        identifier: String,
        relays: Vec<String>,
    },
}

impl Entity {
    /// From bech32 (no `nostr:`). Secret keys (`nsec1…`) and anything else
    /// are `None`.
    fn decode(text: &str) -> Option<Entity> {
        let (hrp, data) = bech32_decode(text)?;
        match hrp.as_str() {
            "npub" => Some(Entity::Profile {
                pubkey: data.try_into().ok()?,
                relays: Vec::new(),
            }),
            "note" => Some(Entity::Event {
                id: data.try_into().ok()?,
                relays: Vec::new(),
                author: None,
                kind: None,
            }),
            "nprofile" => {
                let tlv = Tlv::read(&data)?;
                Some(Entity::Profile {
                    pubkey: tlv.special.try_into().ok()?,
                    relays: tlv.relays,
                })
            }
            "nevent" => {
                let tlv = Tlv::read(&data)?;
                Some(Entity::Event {
                    id: tlv.special.try_into().ok()?,
                    relays: tlv.relays,
                    author: tlv.author,
                    kind: tlv.kind,
                })
            }
            "naddr" => {
                let tlv = Tlv::read(&data)?;
                Some(Entity::Address {
                    kind: tlv.kind?,
                    author: tlv.author?,
                    identifier: String::from_utf8(tlv.special).ok()?,
                    relays: tlv.relays,
                })
            }
            _ => None,
        }
    }

    /// The URI it's saved under: `nostr:npub1…`, `nostr:note1…`, or
    /// `nostr:naddr1…` with kind, author and name only.
    fn uri(&self) -> String {
        let text = match self {
            Entity::Profile { pubkey, .. } => bech32_encode("npub", pubkey),
            Entity::Event { id, .. } => bech32_encode("note", id),
            Entity::Address {
                kind,
                author,
                identifier,
                ..
            } => {
                let name = identifier.as_bytes();
                // Too long a name can't be written: no URI decodes to this.
                let Ok(length) = u8::try_from(name.len()) else {
                    return "nostr:".to_string();
                };
                let mut data = Vec::with_capacity(name.len() + 42);
                data.extend_from_slice(&[0, length]);
                data.extend_from_slice(name);
                data.extend_from_slice(&[2, 32]);
                data.extend_from_slice(author);
                data.extend_from_slice(&[3, 4]);
                data.extend_from_slice(&kind.to_be_bytes());
                bech32_encode("naddr", &data)
            }
        };
        format!("nostr:{text}")
    }
}

/// NIP-19's type-length-value fields.
#[derive(Default)]
struct Tlv {
    /// Type 0: a key, an ID or a name.
    special: Vec<u8>,
    relays: Vec<String>,
    author: Option<[u8; 32]>,
    kind: Option<u32>,
}

impl Tlv {
    fn read(data: &[u8]) -> Option<Tlv> {
        let mut tlv = Tlv::default();
        let mut special = None;
        let mut rest = data;
        while !rest.is_empty() {
            let [kind, length, ..] = *rest else {
                return None;
            };
            let value = rest.get(2..2 + usize::from(length))?;
            match kind {
                0 if special.is_none() => special = Some(value.to_vec()),
                1 => {
                    if let Ok(relay) = std::str::from_utf8(value) {
                        tlv.relays.push(relay.to_string());
                    }
                }
                2 => tlv.author = value.try_into().ok(),
                3 => tlv.kind = Some(u32::from_be_bytes(value.try_into().ok()?)),
                _ => {}
            }
            rest = &rest[2 + usize::from(length)..];
        }
        tlv.special = special?;
        Some(tlv)
    }
}

const BECH32: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn polymod(values: impl IntoIterator<Item = u8>) -> u32 {
    const GENERATOR: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut check: u32 = 1;
    for value in values {
        let top = check >> 25;
        check = ((check & 0x1ff_ffff) << 5) ^ u32::from(value);
        for (i, g) in GENERATOR.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                check ^= g;
            }
        }
    }
    check
}

fn expand(hrp: &str) -> Vec<u8> {
    hrp.bytes()
        .map(|b| b >> 5)
        .chain([0])
        .chain(hrp.bytes().map(|b| b & 31))
        .collect()
}

/// Bech32 (BIP-173): the prefix and the bytes. No length limit: NIP-19's
/// longer entities go past BIP-173's 90 characters.
fn bech32_decode(text: &str) -> Option<(String, Vec<u8>)> {
    if text.bytes().any(|b| b.is_ascii_uppercase()) && text.bytes().any(|b| b.is_ascii_lowercase())
    {
        return None;
    }
    let text = text.to_ascii_lowercase();
    let split = text.rfind('1')?;
    let (hrp, data) = (&text[..split], &text[split + 1..]);
    if hrp.is_empty() || data.len() < 6 {
        return None;
    }
    let values: Vec<u8> = data
        .bytes()
        .map(|c| BECH32.iter().position(|&b| b == c).map(|p| p as u8))
        .collect::<Option<_>>()?;
    if polymod(expand(hrp).into_iter().chain(values.iter().copied())) != 1 {
        return None;
    }
    let bytes = regroup(&values[..values.len() - 6], 5, 8, false)?;
    Some((hrp.to_string(), bytes))
}

fn bech32_encode(hrp: &str, bytes: &[u8]) -> String {
    let values = regroup(bytes, 8, 5, true).expect("padding allowed");
    let check = polymod(
        expand(hrp)
            .into_iter()
            .chain(values.iter().copied())
            .chain([0; 6]),
    ) ^ 1;
    let mut out = String::with_capacity(hrp.len() + 1 + values.len() + 6);
    out.push_str(hrp);
    out.push('1');
    for v in values {
        out.push(BECH32[usize::from(v)] as char);
    }
    for i in 0..6 {
        out.push(BECH32[((check >> (5 * (5 - i))) & 31) as usize] as char);
    }
    out
}

/// Bits regrouped (`from`-bit values to `to`-bit ones).
fn regroup(data: &[u8], from: u32, to: u32, pad: bool) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let max = (1u32 << to) - 1;
    let mut out = Vec::with_capacity(data.len() * from as usize / to as usize + 1);
    for &value in data {
        let value = u32::from(value);
        if value >> from != 0 {
            return None;
        }
        acc = (acc << from) | value;
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & max) as u8);
        }
    }
    if pad {
        if bits > 0 {
            out.push(((acc << (to - bits)) & max) as u8);
        }
    } else if bits >= from || ((acc << (to - bits)) & max) != 0 {
        return None;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signed with BIP-340's first test key (made with its reference code).
    const NOTE_JSON: &str = r##"{"id": "d44d44ca126877188068e2150f78f459fadd681e71df1247b4dc1e094af2ae26", "pubkey": "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659", "created_at": 1760000000, "kind": 1, "tags": [["t", "lost"], ["p", "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659"]], "content": "Hello from LOST\n\"quoted\" \\ back — ünïcødé ✓\ttab", "sig": "54734a4b6d5f2cc512ca8cb3b367e89eaaac444221162d6f2102e9f21cba256db6bb98402e211b20c7c4eb2c31f9328ff65e5654430b1895d5281462fd8fcf3d"}"##;
    const ARTICLE_JSON: &str = r##"{"id": "64807c365425d5093ef6cae9ac6c02399993fad0cc170eea12b7203be284da73", "pubkey": "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659", "created_at": 1760000100, "kind": 30023, "tags": [["d", "hello"], ["title", "Hello, Nostr"], ["summary", "A test article."], ["published_at", "1760000000"]], "content": "# Hello\n\nSome **bold** text and a [link](https://example.org/).\n\n![a cat](https://example.org/cat.png)", "sig": "9195f881de527b0e8677a859d60f555a6135cad91712151dfc64f6751e0ee102512ae0c3afce397a65ca843b8903fa9ec5e9d67b894aa51995d3710d4ada6248"}"##;
    const AUTHOR: &str = "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659";
    /// BIP-340's second test key.
    const OTHER: &str = "dd308afec5777e13121fa72b9cc1b7cc0139715309b086c960e18fd969774eb8";
    /// The note's author asks to delete it; then someone else does.
    const DELETION_JSON: &str = r##"{"id": "9ef55d8b3ecb18c6bafc3ca45427d419a70befeb5af57e458d180205dec89273", "pubkey": "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659", "created_at": 1760000200, "kind": 5, "tags": [["e", "d44d44ca126877188068e2150f78f459fadd681e71df1247b4dc1e094af2ae26"]], "content": "", "sig": "bc55e8c55454333439886e0b28ad70b47314c4b0fe02cb664442f44d24a58028aab604b605e1c902f4ca3573b8e2f8a06a9f32d2cba516f649ed54fc7515ca19"}"##;
    const OTHERS_DELETION_JSON: &str = r##"{"id": "2447055bbfd15be374f6ad5080974b8122577bfc8fb6b75c489b336e5177fa65", "pubkey": "dd308afec5777e13121fa72b9cc1b7cc0139715309b086c960e18fd969774eb8", "created_at": 1760000300, "kind": 5, "tags": [["e", "d44d44ca126877188068e2150f78f459fadd681e71df1247b4dc1e094af2ae26"]], "content": "", "sig": "0fabedf8bd06af44fb04e0543b45dfecf3239956ef30e687627724f56319dd5dbb085764ec8649ee247fb64ece495e59f2b8934c8053166a703cbad4d2e58f4a"}"##;

    fn event(json: &str) -> Event {
        Event::from_json(&serde_json::from_str(json).unwrap()).unwrap()
    }

    fn nostr() -> Nostr {
        Nostr::new(None, None, Duration::ZERO, Vec::new()).unwrap()
    }

    #[test]
    fn nip19() {
        // NIP-19's own example.
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        let key = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        assert_eq!(
            Entity::decode(npub),
            Some(Entity::Profile {
                pubkey: unhex(key).unwrap(),
                relays: Vec::new()
            })
        );
        assert_eq!(bech32_encode("npub", &unhex::<32>(key).unwrap()), npub);
        assert_eq!(
            Entity::decode(&npub.to_uppercase()).unwrap().uri(),
            format!("nostr:{npub}")
        );
        // A changed character, mixed case, a secret key: none.
        assert_eq!(Entity::decode(&npub.replace("0elf", "0elg")), None);
        assert_eq!(
            Entity::decode("npub10ELFcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg"),
            None
        );
        let nsec = bech32_encode("nsec", &[7; 32]);
        assert_eq!(Entity::decode(&nsec), None);
        // From nostr-tools' tests: addresses, TLVs in either order.
        let naddr = "naddr1qq98yetxv4ex2mnrv4esygrl54h466tz4v0re4pyuavvxqptsejl0vxcmnhfl60z3rth2xkpjspsgqqqw4rsf34vl5";
        let Some(Entity::Address {
            kind,
            author,
            identifier,
            ..
        }) = Entity::decode(naddr)
        else {
            panic!("an address");
        };
        assert_eq!(kind, 30023);
        assert_eq!(
            hex(&author),
            "7fa56f5d6962ab1e3cd424e758c3002b8665f7b0d8dcee9fe9e288d7751ac194"
        );
        assert_eq!(identifier, "references");
        let other = "naddr1qqrxyctwv9hxzq3q80cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsxpqqqp65wqfwwaehxw309aex2mrp0yhxummnw3ezuetcv9khqmr99ekhjer0d4skjm3wv4uxzmtsd3jjucm0d5q3vamnwvaz7tmwdaehgu3wvfskuctwvyhxxmmd0zfmwx";
        let entity = Entity::decode(other).unwrap();
        let Entity::Address { relays, .. } = &entity else {
            panic!("an address");
        };
        assert!(relays.contains(&"wss://nostr.banana.com".to_string()));
        // Saved without its relays, and that decodes the same.
        let uri = entity.uri();
        assert!(uri.len() < other.len());
        let Some(Entity::Address {
            kind,
            identifier,
            relays,
            ..
        }) = Entity::decode(uri.strip_prefix("nostr:").unwrap())
        else {
            panic!("an address");
        };
        assert_eq!(
            (kind, identifier.as_str(), relays.len()),
            (30023, "banana", 0)
        );
        // An event with relays: saved as its note.
        let nevent = "nevent1qqst8cujky046negxgwwm5ynqwn53t8aqjr6afd8g59nfqwxpdhylpcpzamhxue69uhhyetvv9ujuetcv9khqmr99e3k7mg8arnc9";
        let entity = Entity::decode(nevent).unwrap();
        let Entity::Event { relays, .. } = &entity else {
            panic!("an event");
        };
        assert_eq!(relays.len(), 1);
        assert!(entity.uri().starts_with("nostr:note1"));
        // Malformed TLVs don't loop or panic.
        assert_eq!(Entity::decode("nevent1qq5v4rkz"), None);
    }

    #[test]
    fn uris_and_links() {
        let npub = "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg";
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(Nostr::accepts(&url(&format!("nostr:{npub}"))));
        assert!(Nostr::accepts(&url("wss://relay.damus.io/")));
        assert!(Nostr::accepts(&url(
            "wss://relay.damus.io/?kinds=1&until=5"
        )));
        assert!(!Nostr::accepts(&url("https://relay.damus.io/")));
        let nprofile = bech32_encode("nprofile", &{
            let mut d = vec![0, 32];
            d.extend(
                unhex::<32>("7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e")
                    .unwrap(),
            );
            d.extend([1, 21]);
            d.extend(b"wss://relay.example/x");
            d
        });
        // Not the way it's saved: rewritten, and not crawled as it is.
        assert!(!Nostr::accepts(&url(&format!("nostr:{nprofile}"))));
        assert_eq!(
            Nostr::rewrite(&format!("nostr:{nprofile}")),
            Some(format!("nostr:{npub}"))
        );
        assert_eq!(
            Nostr::rewrite(&format!("nostr://{npub}")),
            Some(format!("nostr:{npub}"))
        );
        assert_eq!(Nostr::rewrite("nostr:nsec1xyz"), None);
        assert!(!Nostr::accepts(&url(&format!(
            "nostr:{}",
            bech32_encode("nsec", &[1; 32])
        ))));
        assert_eq!(
            Nostr::fallback_title(&url(&format!("nostr:{npub}"))),
            "npub10elfcs4…jptg"
        );
        assert_eq!(
            relay_key("wss://Relay.Example:443/path?x=1"),
            Some("wss://relay.example/path".into())
        );
        assert_eq!(relay_key("https://relay.example/"), None);
        // References written in text.
        let text = format!("hi nostr:{npub}, see NOSTR:{nprofile}. and nostr:nope");
        let links: Vec<String> = nostr_links(&text).into_iter().map(|l| l.url).collect();
        assert_eq!(links, [format!("nostr:{npub}")]);
        assert_eq!(strip_references(&text), "hi see and");
    }

    #[test]
    fn signatures() {
        let note = event(NOTE_JSON);
        assert!(note.is_signed());
        assert!(usable(&note));
        assert!(event(ARTICLE_JSON).is_signed());
        // Anything changed, and the ID isn't its own.
        let mut changed = note.clone();
        changed.content.push('!');
        assert!(!changed.is_signed());
        let mut changed = note.clone();
        changed.created_at += 1;
        assert!(!changed.is_signed());
        // Asking to expire: not kept.
        let mut expiring = note.clone();
        expiring
            .tags
            .push(vec!["expiration".into(), "9999999999".into()]);
        assert!(!usable(&expiring));
        // Not a post.
        assert_eq!(Event::from_json(&json!({ "id": "x" })), None);
        assert_eq!(
            Event::from_json(&json!({ "id": "", "pubkey": "", "created_at": 1, "kind": 1,
                "tags": [["p", 5]], "content": "", "sig": "" })),
            None
        );
        // Another post's signature, by the same key: the ID is the post's,
        // the signature isn't for it.
        let mut changed = note.clone();
        changed.sig = event(ARTICLE_JSON).sig;
        assert!(!changed.is_signed());
    }

    #[test]
    fn posts_as_pages() {
        let nostr = nostr();
        let note = event(NOTE_JSON);
        assert_eq!(note.uri().as_deref().map(|u| &u[..11]), Some("nostr:note1"));
        let parse = |r: Response| match r {
            Response::Document { mime, body } => {
                nostr.parse(&Url::parse("nostr:x").unwrap(), &mime, &body.unwrap())
            }
            other => panic!("{other:?}"),
        };
        let page = parse(nostr.post(&note, "wss://relay.example/"));
        assert!(!page.skip);
        assert_eq!(
            page.title,
            Some(format!(
                "{}: Hello from LOST \"quoted\" \\ back — ünïcødé ✓ tab",
                short(&npub(AUTHOR))
            ))
        );
        assert_eq!(page.body, note.content);
        let links: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(links, [npub_uri(AUTHOR)]);
        // With the author's name known, and a mention of them.
        nostr
            .names
            .lock()
            .unwrap()
            .insert(AUTHOR.into(), "Satoshi".into());
        let mut mention = note.clone();
        mention.content = format!("Thanks nostr:{} for everything", npub(AUTHOR));
        let page = parse(nostr.post(&mention, "wss://relay.example/"));
        assert_eq!(
            page.title.as_deref(),
            Some("Satoshi: Thanks @Satoshi for everything")
        );
        assert_eq!(page.body, "Thanks @Satoshi for everything");
        // Too little to read: followed, not saved.
        let mut short_note = note.clone();
        short_note.content = "gm 🤙 https://example.org/a-long-link".into();
        assert!(parse(nostr.post(&short_note, "wss://relay.example/")).skip);

        let article = event(ARTICLE_JSON);
        assert!(article.uri().unwrap().starts_with("nostr:naddr1"));
        let page = parse(nostr.post(&article, "wss://relay.example/"));
        assert_eq!(page.title.as_deref(), Some("Hello, Nostr"));
        assert_eq!(
            page.body,
            "A test article.\n\nHello\n\nSome bold text and a link.\n\na cat"
        );
        let links: Vec<(&str, Option<&str>)> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.label.as_deref()))
            .collect();
        assert_eq!(
            links,
            [
                (npub_uri(AUTHOR).as_str(), Some("Satoshi")),
                ("https://example.org/", Some("link")),
            ]
        );
    }

    #[test]
    fn tags_and_text() {
        let mut note = event(NOTE_JSON);
        let other = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        note.tags = vec![
            vec![
                "e".into(),
                other.into(),
                "wss://hint.example".into(),
                "reply".into(),
            ],
            vec!["a".into(), format!("30023:{other}:my-article")],
            vec!["p".into(), other.into()],
            vec!["p".into(), other.into()],
            vec!["t".into(), "hashtag".into()],
            vec!["e".into(), "not hex".into()],
        ];
        let links = tag_links(&note);
        assert_eq!(links.len(), 3);
        assert!(links[0].0.starts_with("nostr:note1"));
        assert_eq!(links[0].1.as_deref(), Some("wss://hint.example/"));
        assert!(links[1].0.starts_with("nostr:naddr1"));
        assert_eq!(links[2].0, npub_uri(other));

        assert_eq!(
            asciidoc_to_text("= Bitcoin\n\nSee [[Satoshi Nakamoto]] and [[cryptography|crypto]]."),
            "Bitcoin\n\nSee Satoshi Nakamoto and crypto."
        );
        assert_eq!(cut("one two three four", 9), "one two…");
        assert_eq!(cut("short", 9), "short");
        assert_eq!(cut("ünïcødéünïcødé", 5), "ünïcø…");
        let profile = Profile {
            name: Some("satoshi".into()),
            display_name: Some(" Satoshi\nNakamoto ".into()),
            about: Some("Bitcoin's creator.".into()),
            website: Some("bitcoin.org".into()),
            nip05: Some("_@bitcoin.org".into()),
        };
        assert_eq!(profile.name().as_deref(), Some("Satoshi Nakamoto"));
        assert_eq!(profile.website().as_deref(), Some("https://bitcoin.org/"));
        // What's in the text already isn't added again.
        assert_eq!(profile.text(), "Bitcoin's creator.\nsatoshi\n_@bitcoin.org");
    }

    #[test]
    fn listings() {
        assert_eq!(
            List::of("kinds=30023,30818&until=1700000000"),
            Some(List {
                kinds: vec![30023, 30818],
                until: Some(1_700_000_000),
                search: None,
            })
        );
        assert_eq!(
            List::of("kinds=1").map(|l| l.query()).as_deref(),
            Some("kinds=1")
        );
        // A search, and the same written again.
        let query = "kinds=0,1,30023,30818&search=bitcoin%20mining%3F&until=5";
        let list = List::of(query).unwrap();
        assert_eq!(list.search.as_deref(), Some("bitcoin mining?"));
        assert_eq!(list.query(), query);
        assert_eq!(List::of("kinds=1&search="), None);
        // Other kinds, other questions: not listings.
        assert_eq!(List::of("kinds=4"), None);
        assert_eq!(List::of("kinds=1&authors=x"), None);
        assert_eq!(List::of("until=5"), None);
        assert_eq!(List::of("kinds=1&until=soon"), None);
        // A relay's own page: its listings, without asking it anything (but
        // for what it says about itself, known here already).
        let nostr = nostr();
        let _ = nostr
            .relay("wss://relay.example/")
            .info
            .set(Info::default());
        let url = Url::parse("wss://relay.example/").unwrap();
        let Ok(Response::Document { mime, body }) = nostr.listing(&url) else {
            panic!("a listing");
        };
        let parsed = nostr.parse(&url, &mime, &body.unwrap());
        assert!(parsed.skip);
        let links: Vec<String> = parsed.links.into_iter().map(|l| l.url).collect();
        assert_eq!(
            links,
            [
                "wss://relay.example/?kinds=30023,30818",
                "wss://relay.example/?kinds=1,5"
            ]
        );
        assert_eq!(
            refused("rate-limited: slow down"),
            Response::SlowDown(Duration::from_secs(60))
        );
        assert_eq!(refused("auth-required: members only"), Response::Gone);

        // What relays say about themselves.
        assert_eq!(
            Info::of(
                &json!({ "supported_nips": [1, 11, 50], "limitation": { "auth_required": false } })
            ),
            Info {
                searches: true,
                closed: false
            }
        );
        assert_eq!(
            Info::of(&json!({ "supported_nips": [1], "limitation": { "payment_required": true } })),
            Info {
                searches: false,
                closed: true
            }
        );
        assert_eq!(Info::of(&json!("not a document")), Info::default());
        // A relay that searches gets a listing per phrase; one that's
        // closed, none at all.
        let nostr = Nostr::new(None, None, Duration::ZERO, vec!["bitcoin mining".into()]).unwrap();
        assert_eq!(nostr.seeds(), SEEDS_AND_SEARCH);
        let links = |nostr: &Nostr, key: &str| match nostr.listing(&Url::parse(key).unwrap()) {
            Ok(Response::Document { mime, body }) => nostr
                .parse(&Url::parse(key).unwrap(), &mime, &body.unwrap())
                .links
                .into_iter()
                .map(|l| l.url)
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        let _ = nostr.relay("wss://search.example/").info.set(Info {
            searches: true,
            closed: false,
        });
        assert_eq!(
            links(&nostr, "wss://search.example/"),
            [
                "wss://search.example/?kinds=30023,30818",
                "wss://search.example/?kinds=1,5",
                "wss://search.example/?kinds=0,1,30023,30818&search=bitcoin%20mining",
            ]
        );
        let _ = nostr.relay("wss://paid.example/").info.set(Info {
            searches: true,
            closed: true,
        });
        assert_eq!(
            nostr.listing(&Url::parse("wss://paid.example/").unwrap()),
            Ok(Response::Gone)
        );
    }

    #[test]
    fn blocked_and_deleted() {
        let nostr = nostr();
        let note = event(NOTE_JSON);
        assert!(nostr.wanted(&note));
        // Blacklisted authors, written either way.
        let blacklist = Blacklist::parse(&format!(
            "nostr:{}\n{}\nexample.org\nnostr:{}\n",
            npub(AUTHOR),
            npub(OTHER),
            npub(OTHER)
        ))
        .unwrap();
        assert_eq!(nostr.block_authors(&blacklist), 2);
        assert!(!nostr.wanted(&note));
        // Deleted: only by its author's request, whoever else asks after.
        let nostr = super::tests::nostr();
        nostr.note_deletions(&[event(OTHERS_DELETION_JSON)]);
        assert!(nostr.wanted(&note));
        nostr.note_deletions(&[event(DELETION_JSON), event(OTHERS_DELETION_JSON)]);
        assert!(!nostr.wanted(&note));
        // An article, by its address.
        let article = event(ARTICLE_JSON);
        assert_eq!(
            article.coordinate().as_deref(),
            Some(format!("30023:{AUTHOR}:hello").as_str())
        );
        assert!(nostr.wanted(&article));
        nostr
            .deleted
            .lock()
            .unwrap()
            .insert(format!("{} {AUTHOR}", article.coordinate().unwrap()));
        assert!(!nostr.wanted(&article));
        assert_eq!(note.coordinate(), None);
        // Topics written as hashtags, unless they're there.
        let mut tagged = article.clone();
        tagged.tags.extend([
            vec!["t".into(), "nostr".into()],
            vec!["t".into(), "#Bitcoin".into()],
            vec!["t".into(), "nostr".into()],
            vec!["t".into(), "two words".into()],
        ]);
        assert_eq!(
            with_topics("About #bitcoin".into(), &tagged),
            "About #bitcoin\n\n#nostr"
        );
        assert_eq!(with_topics(String::new(), &tagged), "#nostr #Bitcoin");
    }

    #[test]
    fn kept_posts() {
        let mut kept = Kept::default();
        let note = event(NOTE_JSON);
        for i in 0..MAX_KEPT + 10 {
            kept.keep(format!("nostr:{i}"), note.clone(), "wss://r/");
        }
        assert_eq!(kept.events.len(), MAX_KEPT);
        assert!(kept.take("nostr:0").is_none());
        assert!(kept.take(&format!("nostr:{}", MAX_KEPT + 9)).is_some());
        assert!(kept.take(&format!("nostr:{}", MAX_KEPT + 9)).is_none());
        // Kept and taken, over and over: what's left to do stays small.
        let mut kept = Kept::default();
        for i in 0..MAX_KEPT * 5 {
            let uri = format!("nostr:{i}");
            kept.keep(uri.clone(), note.clone(), "wss://r/");
            assert!(kept.take(&uri).is_some());
        }
        assert!(kept.events.is_empty());
        assert!(kept.order.len() <= 2 * MAX_KEPT);
    }

    /// A relay on this computer that answers every `REQ` with `events`,
    /// then `EOSE`; what it was asked comes back at the end.
    fn relay(events: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("ws://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                s.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            s.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                .unwrap();
            let send = |s: &mut std::net::TcpStream, text: &str| {
                let mut frame = vec![0x81];
                if text.len() < 126 {
                    frame.push(text.len() as u8);
                } else {
                    frame.push(126);
                    frame.extend_from_slice(&(text.len() as u16).to_be_bytes());
                }
                frame.extend_from_slice(text.as_bytes());
                s.write_all(&frame).unwrap();
            };
            let mut asked = Vec::new();
            loop {
                let mut h = [0u8; 2];
                if s.read_exact(&mut h).is_err() {
                    break;
                }
                let length = match h[1] & 0x7f {
                    126 => {
                        let mut b = [0u8; 2];
                        s.read_exact(&mut b).unwrap();
                        usize::from(u16::from_be_bytes(b))
                    }
                    n => usize::from(n),
                };
                let mut mask = [0u8; 4];
                s.read_exact(&mut mask).unwrap();
                let mut payload = vec![0u8; length];
                s.read_exact(&mut payload).unwrap();
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
                if h[0] & 0x0f == 0x8 {
                    break;
                }
                let message: Value = serde_json::from_slice(&payload).unwrap();
                if message[0] == "REQ" {
                    let id = message[1].as_str().unwrap().to_string();
                    send(&mut s, &json!(["NOTICE", "welcome"]).to_string());
                    for event in &events {
                        let event: Value = serde_json::from_str(event).unwrap();
                        send(&mut s, &json!(["EVENT", id, event]).to_string());
                    }
                    send(&mut s, &json!(["EOSE", id]).to_string());
                    asked.push(message);
                }
            }
            asked
        });
        (address, server)
    }

    #[test]
    fn deleted_while_listed() {
        // The note's author asked to delete it, someone else too: it's
        // left out, and so are the requests.
        let (address, server) = relay(vec![DELETION_JSON, OTHERS_DELETION_JSON, NOTE_JSON]);
        let nostr = nostr();
        let url = Url::parse(&format!("{address}?kinds=1,5")).unwrap();
        let Ok(Response::Document { mime, body }) = nostr.fetch(Route::Proxied, &url) else {
            panic!("a listing");
        };
        let links: Vec<String> = nostr
            .parse(&url, &mime, &body.unwrap())
            .links
            .into_iter()
            .map(|l| l.url)
            .collect();
        assert_eq!(links, [format!("{address}?kinds=1,5&until=1760000000")]);
        drop(nostr);
        server.join().unwrap();
    }

    #[test]
    fn searching_a_relay() {
        let (address, server) = relay(vec![ARTICLE_JSON]);
        let nostr = Nostr::new(None, None, Duration::ZERO, vec!["hello".into()]).unwrap();
        let url = Url::parse(&format!("{address}?kinds=0,1,30023,30818&search=hello")).unwrap();
        let Ok(Response::Document { mime, body }) = nostr.fetch(Route::Proxied, &url) else {
            panic!("a listing");
        };
        let links: Vec<String> = nostr
            .parse(&url, &mime, &body.unwrap())
            .links
            .into_iter()
            .map(|l| l.url)
            .collect();
        assert_eq!(
            links,
            [
                event(ARTICLE_JSON).uri().unwrap(),
                format!("{address}?kinds=0,1,30023,30818&search=hello&until=1760000100"),
            ]
        );
        drop(nostr);
        let asked = server.join().unwrap();
        assert_eq!(
            asked[0][2],
            json!({ "kinds": [0, 1, 30023, 30818], "limit": 100, "search": "hello" })
        );
    }

    #[test]
    fn listing_a_relay() {
        let (address, server) = relay(vec![NOTE_JSON, ARTICLE_JSON]);
        let nostr = nostr();
        let url = Url::parse(&format!("{address}?kinds=1,30023&until=1760000100")).unwrap();
        let Ok(Response::Document { mime, body }) = nostr.fetch(Route::Proxied, &url) else {
            panic!("a listing");
        };
        let parsed = nostr.parse(&url, &mime, &body.unwrap());
        assert!(parsed.skip);
        let note_uri = event(NOTE_JSON).uri().unwrap();
        let article_uri = event(ARTICLE_JSON).uri().unwrap();
        let links: Vec<(String, Option<String>)> =
            parsed.links.into_iter().map(|l| (l.url, l.label)).collect();
        assert_eq!(
            links,
            [
                (note_uri.clone(), None),
                (article_uri.clone(), Some("Hello, Nostr".into())),
                (format!("{address}?kinds=1,30023&until=1760000000"), None),
            ]
        );
        // The posts listed are there already: no relay is asked.
        let Ok(Response::Document { mime, body }) =
            nostr.fetch(Route::Proxied, &Url::parse(&article_uri).unwrap())
        else {
            panic!("a page");
        };
        let page = nostr.parse(&url, &mime, &body.unwrap());
        assert_eq!(page.title.as_deref(), Some("Hello, Nostr"));
        drop(nostr);
        let asked = server.join().unwrap();
        // The listing, and the authors' names.
        assert_eq!(asked.len(), 2);
        assert_eq!(
            asked[0][2],
            json!({ "kinds": [1, 30023], "limit": 200, "until": 1760000100 })
        );
        assert_eq!(asked[1][2], json!({ "kinds": [0], "authors": [AUTHOR] }));
    }
}
