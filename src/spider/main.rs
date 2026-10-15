//! lost-spider: LOST's crawler (ARCHITECTURE.md §15). It crawls a network
//! and saves what it finds as a `.lost` file (unsigned unless asked), which
//! LOST imports like any other. It's a program of its own, built only on request
//! (`--features spider`), so that LOST itself never goes online: it uses
//! LOST's library for the `.lost` format and your ID, and nothing in LOST
//! knows it exists.

mod crawl;
mod gemini;
mod gopher;
mod http;
mod i2p;
mod net;
mod onion;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use lost::Identity;
use lost::lostfile::{Reader, Writer};
use lost::text::{format_bytes, thousands};
use lost::urls::Url;

use crate::crawl::{Blacklist, Crawler, Known, Protocol, Settings};
use crate::gemini::Gemini;
use crate::gopher::Gopher;
use crate::i2p::I2p;
use crate::net::Proxy;
use crate::onion::Onion;

const USAGE: &str = "\
lost-spider — LOST's crawler. It crawls a network and saves what it finds
as a .lost file for LOST to import.

Usage:
  lost-spider gemini [OPTIONS]    Crawl Geminispace
  lost-spider gopher [OPTIONS]    Crawl Gopherspace
  lost-spider onion [OPTIONS]     Crawl .onion sites, through Tor
  lost-spider i2p [OPTIONS]       Crawl I2P sites, through an I2P router

Options:
  -o, --output PATH       Where to save: a .lost file, or a folder to put one in
                          (default: lost-crawl-data-NETWORK-DATE-TIME.lost here;
                          if the file exists, \" (1)\", \" (2)\"… is added)
  -c, --continue          Carry on with the -o file: its pages are kept, links
                          they lead to are crawled. Its signature, if any, is
                          removed (or replaced by yours, with --sign). Give
                          the other options again
      --sign              Sign the file with your ID (made if you have none).
                          Files are unsigned otherwise: a signature says who
                          made them
      --link URL          Crawl this first, even if nothing links to it
                          (repeat for more)
      --links FILE        The same, one URL a line
      --dont-crawl        Fetch only the --link URLs, and follow none of their
                          links: to add those pages to a file, and nothing else
      --search PHRASE     Type PHRASE into the search boxes found, and crawl
                          what they find (repeat for more). Only boxes that
                          look like searches; never comment or password boxes
      --searches FILE     The same, one phrase a line
      --blacklist FILE    Skip URLs and pages that FILE lists, in this crawl
                          (domains, URL beginnings, /regex/, title:/regex/,
                          body:/regex/; # starts a comment)
      --delay SECONDS     Wait between two requests to the same server
                          (default 1; decimals allowed)
      --host-limit N      Save at most N pages from one host (default: no limit)
      --proxy URL         Go through a proxy: socks5://HOST:PORT (any network;
                          Tor's is socks5://127.0.0.1:9050) or http://HOST:PORT
                          (I2P only). onion uses Tor's by default, i2p
                          http://127.0.0.1:4444
  -t, --timeout SECONDS   How long to wait for a server to connect or send more
                          (a whole page gets four times that). Defaults: gopher
                          10, gemini 15, either through a proxy 30, onion 60,
                          i2p 120
  -h, --help              Show this help
  -V, --version           Show the version

robots.txt is always obeyed. Ctrl-C stops and leaves a complete file;
-c carries on from there.
";

/// Requests running at once, to different servers.
const WORKERS: usize = 32;
/// The file is made complete at least this often, even if no chunk filled.
const CHECKPOINT_EVERY: Duration = Duration::from_secs(5 * 60);

static STOPPING: AtomicBool = AtomicBool::new(false);

struct Options {
    output: Option<PathBuf>,
    continue_: bool,
    links: Vec<String>,
    searches: Vec<String>,
    blacklist: Option<PathBuf>,
    dont_crawl: bool,
    sign: bool,
    delay: Duration,
    host_limit: Option<u64>,
    proxy: Option<Proxy>,
    timeout: Option<Duration>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lost-spider: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> lost::Result<ExitCode> {
    let Some(first) = args.first() else {
        eprint!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    match first.as_str() {
        "-h" | "--help" => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "-V" | "--version" => {
            println!("lost-spider {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        "gemini" => start(&args[1..], Gemini::new),
        "gopher" => start(&args[1..], Gopher::new),
        "onion" => start(&args[1..], Onion::new),
        "i2p" => start(&args[1..], I2p::new),
        other => Err(format!(
            "unknown network \"{other}\" (lost-spider knows: gemini, gopher, onion, i2p)"
        )
        .into()),
    }
}

/// Reads the options for network `P` and crawls it.
fn start<P: Protocol>(
    args: &[String],
    make: fn(Option<Proxy>, Option<Duration>) -> lost::Result<P>,
) -> lost::Result<ExitCode> {
    let Some(mut options) = parse_options::<P>(args)? else {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    };
    let protocol = make(options.proxy.take(), options.timeout)?;
    crawl(protocol, options)
}

/// `None` for `--help`.
fn parse_options<P: Protocol>(args: &[String]) -> lost::Result<Option<Options>> {
    let mut options = Options {
        output: None,
        continue_: false,
        links: Vec::new(),
        searches: Vec::new(),
        blacklist: None,
        dont_crawl: false,
        sign: false,
        delay: Duration::from_secs(1),
        host_limit: None,
        proxy: None,
        timeout: None,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = || -> lost::Result<String> {
            inline
                .clone()
                .or_else(|| args.next().cloned())
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match name {
            "-h" | "--help" => return Ok(None),
            "-o" | "--output" => options.output = Some(PathBuf::from(value()?)),
            "-c" | "--continue" => options.continue_ = true,
            "--link" => options.links.push(link::<P>(&value()?)?),
            "--links" => {
                for line in lines(Path::new(&value()?))? {
                    options.links.push(link::<P>(&line)?);
                }
            }
            "--search" => options.searches.push(value()?),
            "--searches" => options.searches.extend(lines(Path::new(&value()?))?),
            "--blacklist" => options.blacklist = Some(PathBuf::from(value()?)),
            "--dont-crawl" => options.dont_crawl = true,
            "--sign" => options.sign = true,
            "--delay" => {
                let text = value()?;
                let seconds: f64 = text
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s >= 0.0 && *s <= 86_400.0)
                    .ok_or_else(|| {
                        format!("--delay takes seconds, like 1 or 0.5 (not \"{text}\")")
                    })?;
                options.delay = Duration::from_secs_f64(seconds);
            }
            "--host-limit" => {
                let text = value()?;
                let limit: u64 = text.parse().ok().filter(|n| *n > 0).ok_or_else(|| {
                    format!("--host-limit takes a number of pages (not \"{text}\")")
                })?;
                options.host_limit = Some(limit);
            }
            "--proxy" => options.proxy = Some(Proxy::parse(&value()?)?),
            "-t" | "--timeout" => {
                let text = value()?;
                let seconds: f64 = text
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0 && *s <= 3600.0)
                    .ok_or_else(|| format!("--timeout takes seconds, like 30 (not \"{text}\")"))?;
                options.timeout = Some(Duration::from_secs_f64(seconds));
            }
            other => return Err(format!("unknown option {other} (see lost-spider --help)").into()),
        }
    }
    if options.dont_crawl && options.links.is_empty() {
        return Err("--dont-crawl needs --link or --links: the pages to fetch".into());
    }
    if options.continue_ && options.output.is_none() {
        return Err("-c needs the file to carry on with: -c -o FILE.lost".into());
    }
    Ok(Some(options))
}

/// A `--link`: `example.org` is taken as `gemini://example.org/` (or the
/// network's scheme).
fn link<P: Protocol>(text: &str) -> lost::Result<String> {
    let text = text.trim();
    let full = if text.contains("://") {
        text.to_string()
    } else {
        format!("{}://{text}", P::SCHEME)
    };
    match Url::parse(&full) {
        Some(url) if P::accepts(&url) => Ok(url.to_string()),
        _ => Err(format!("\"{text}\" isn't a link lost-spider {} can follow", P::NAME).into()),
    }
}

/// A file's lines, without blank ones and `#` comments.
fn lines(path: &Path) -> lost::Result<Vec<String>> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn crawl<P: Protocol>(protocol: P, options: Options) -> lost::Result<ExitCode> {
    let mut blacklist = match &options.blacklist {
        Some(path) => Blacklist::load(path)?,
        None => Blacklist::default(),
    };
    if let Some(proxy) = protocol.proxy() {
        proxy.check().map_err(|e| {
            let hint = match P::NAME {
                "onion" => " (is Tor running?)",
                "i2p" => " (is the I2P router running?)",
                _ => "",
            };
            format!("{e}{hint}")
        })?;
    }
    protocol.prepare(&mut blacklist)?;
    // Unsigned unless asked: no ID is read or made without --sign.
    let identity = if options.sign {
        Some(load_identity()?)
    } else {
        None
    };
    let path = output_path(options.output.as_deref(), options.continue_, P::NAME)?;

    let (mut writer, known, path) = if options.continue_ && path.exists() {
        let known = read_known(&path, P::accepts)?;
        (Writer::append(&path, identity.as_ref())?, known, path)
    } else {
        let (file, path) = create_numbered(&path, !options.continue_)?;
        lost::lostfile::lock(&file, &name_of(&path))?;
        let mut writer = Writer::new(file, None, identity.as_ref())?;
        writer.checkpoint()?;
        (writer, Known::default(), path)
    };
    let name = name_of(&path);
    let before = writer.pages();
    match &identity {
        Some(identity) => eprintln!("Saving to {name}, signed as {}.", identity.id()),
        None => eprintln!("Saving to {name}, unsigned."),
    }
    if before > 0 {
        eprintln!(
            "It already has {} pages; carrying on from their links.",
            thousands(before)
        );
    }

    let settings = Settings {
        delay: options.delay,
        host_limit: options.host_limit,
        searches: options.searches,
        blacklist,
        dont_crawl: options.dont_crawl,
        workers: WORKERS,
    };
    let crawler = Crawler::new(protocol, settings, known, &options.links);
    let (tx, rx) = mpsc::sync_channel(1000);
    crawler.start(tx);

    ctrlc::set_handler(|| {
        if STOPPING.swap(true, Ordering::Relaxed) {
            eprintln!();
            std::process::exit(130);
        }
        eprintln!("\nStopping… (Ctrl-C again to quit at once)");
    })
    .map_err(|e| format!("can't watch for Ctrl-C: {e}"))?;

    // With --dont-crawl it only fetches what it's given; otherwise it crawls.
    let verb = if options.dont_crawl {
        "Scraping"
    } else {
        "Crawling"
    };
    let started = Instant::now();
    let terminal = std::io::stderr().is_terminal();
    let every = if terminal {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(60)
    };
    let status = |pages: u64| {
        let stats = crawler.stats();
        format!(
            "{} {} for {}… {} pages saved · {} {} · {} links waiting",
            verb,
            P::SPACE,
            duration(started.elapsed()),
            thousands(pages),
            thousands(stats.hosts as u64),
            P::HOSTS,
            thousands(stats.waiting),
        )
    };
    let show = |line: String| {
        if terminal {
            eprint!("\r{line}\x1b[K");
        } else {
            eprintln!("{line}");
        }
    };
    let mut chunks = writer.chunk_count();
    let mut checkpointed = Instant::now();
    let mut reported = Instant::now();
    show(status(writer.pages()));

    let finished = loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(page) => {
                writer.add(&page)?;
                if writer.chunk_count() != chunks {
                    writer.checkpoint()?;
                    chunks = writer.chunk_count();
                    checkpointed = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break true,
        }
        if STOPPING.load(Ordering::Relaxed) {
            crawler.stop();
            while let Ok(page) = rx.try_recv() {
                writer.add(&page)?;
            }
            break false;
        }
        if checkpointed.elapsed() >= CHECKPOINT_EVERY && writer.buffered() > 0 {
            writer.flush_chunk()?;
            writer.checkpoint()?;
            chunks = writer.chunk_count();
            checkpointed = Instant::now();
        }
        if reported.elapsed() >= every {
            show(status(writer.pages()));
            reported = Instant::now();
        }
    };
    show(status(writer.pages()));
    if terminal {
        eprintln!();
    }

    let (file, pages, size) = writer.finish()?;
    file.sync_all()?;
    let new = pages - before;
    let took = duration(started.elapsed());
    if finished {
        eprintln!(
            "Done in {took}: {} new pages, {} in all, in {name} ({}).",
            thousands(new),
            thousands(pages),
            format_bytes(size)
        );
    } else {
        eprintln!(
            "Stopped after {took}: {} new pages, {} in all, in {name} ({}).",
            thousands(new),
            thousands(pages),
            format_bytes(size)
        );
        eprintln!(
            "To carry on: lost-spider {} -c -o \"{}\" (and the same options as before)",
            P::NAME,
            path.display()
        );
    }
    eprintln!("To search them: lost -d \"{}\"", path.display());
    Ok(ExitCode::SUCCESS)
}

/// "6 hours, 15 minutes, 56 seconds": the units from the largest one there
/// is, so "56 seconds", "3 minutes, 0 seconds", "2 days, 0 hours, …".
fn duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let parts = [
        (secs / 86_400, "day"),
        (secs / 3600 % 24, "hour"),
        (secs / 60 % 60, "minute"),
        (secs % 60, "second"),
    ];
    let first = parts.iter().position(|(n, _)| *n > 0).unwrap_or(3);
    parts[first..]
        .iter()
        .map(|(n, unit)| format!("{n} {unit}{}", if *n == 1 { "" } else { "s" }))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Your ID, made if there isn't one yet; `None` if LOST's folder can't be
/// found (no home folder).
fn load_identity() -> lost::Result<Identity> {
    let dir = Identity::default_dir()
        .ok_or("--sign needs your ID, and LOST's folder can't be found (no home folder)")?;
    let (identity, created) = Identity::load_or_create(&dir)
        .map_err(|e| format!("--sign needs your ID, which can't be read or made: {e}"))?;
    if created {
        eprintln!(
            "Made your ID: {} (kept in {}).",
            identity.id(),
            dir.display()
        );
    }
    Ok(identity)
}

/// The file to write: `-o` as given, or a default name in the folder `-o`
/// names (or here).
fn output_path(given: Option<&Path>, continue_: bool, scheme: &str) -> lost::Result<PathBuf> {
    let now = lost::store::now();
    let seconds = now.rem_euclid(86_400);
    let default = format!(
        "lost-crawl-data-{scheme}-{}-{:02}{:02}{:02}.lost",
        lost::text::format_date(now).replace('-', ""),
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    let Some(given) = given else {
        return Ok(PathBuf::from(default));
    };
    let text = given.to_string_lossy();
    let is_folder =
        given.is_dir() || text.ends_with('/') || text.ends_with(std::path::MAIN_SEPARATOR);
    if !is_folder {
        return Ok(given.to_path_buf());
    }
    if continue_ {
        return Err(
            format!("-c needs a .lost file to carry on with, and {text} is a folder").into(),
        );
    }
    std::fs::create_dir_all(given).map_err(|e| format!("can't make the folder {text}: {e}"))?;
    Ok(given.join(default))
}

/// Makes `path`, or `name (1).lost`, `name (2).lost`… if it's taken
/// (only `path` itself when `numbered` is false).
fn create_numbered(path: &Path, numbered: bool) -> lost::Result<(std::fs::File, PathBuf)> {
    for n in 0u32.. {
        let candidate = if n == 0 {
            path.to_path_buf()
        } else {
            with_number(path, n)
        };
        match std::fs::File::create_new(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && numbered => continue,
            Err(e) => return Err(format!("can't make {}: {e}", candidate.display()).into()),
        }
    }
    unreachable!("there's always another number")
}

/// `crawl.lost` → `crawl (2).lost`.
fn with_number(path: &Path, n: u32) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let split = name.len().saturating_sub(5);
    let lost = name.len() > 5
        && name
            .get(split..)
            .is_some_and(|end| end.eq_ignore_ascii_case(".lost"));
    let (stem, extension) = if lost {
        name.split_at(split)
    } else {
        (name.as_str(), "")
    };
    path.with_file_name(format!("{stem} ({n}){extension}"))
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// What a file being continued has already: every page's URL, text and links.
fn read_known(path: &Path, accepts: fn(&Url) -> bool) -> lost::Result<Known> {
    let mut reader = Reader::open(path)?;
    eprintln!("Reading {}…", reader.name());
    let mut known = Known::default();
    let mut damaged = 0;
    for i in 0..reader.chunk_count() {
        match reader.read_chunk(i)? {
            Some(chunk) => {
                for page in chunk.pages {
                    known.add(page, accepts);
                }
            }
            None => damaged += 1,
        }
    }
    if damaged > 0 {
        eprintln!(
            "{damaged} damaged part(s) of the file were skipped; their pages may be crawled again."
        );
    }
    Ok(known)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        let d = |s: u64| duration(Duration::from_secs(s));
        assert_eq!(d(0), "0 seconds");
        assert_eq!(d(1), "1 second");
        assert_eq!(d(56), "56 seconds");
        assert_eq!(d(180), "3 minutes, 0 seconds");
        assert_eq!(
            d(6 * 3600 + 15 * 60 + 56),
            "6 hours, 15 minutes, 56 seconds"
        );
        assert_eq!(d(86_400 + 3601), "1 day, 1 hour, 0 minutes, 1 second");
    }

    #[test]
    fn numbered_names() {
        assert_eq!(
            with_number(Path::new("a/crawl.lost"), 2),
            PathBuf::from("a/crawl (2).lost")
        );
        assert_eq!(
            with_number(Path::new("crawl.LOST"), 1),
            PathBuf::from("crawl (1).LOST")
        );
        assert_eq!(
            with_number(Path::new("crawl"), 1),
            PathBuf::from("crawl (1)")
        );
        // The last five bytes fall inside a character: no panic.
        assert_eq!(
            with_number(Path::new("пауки"), 1),
            PathBuf::from("пауки (1)")
        );
        assert_eq!(
            with_number(Path::new("пауки.lost"), 3),
            PathBuf::from("пауки (3).lost")
        );
    }

    #[test]
    fn options() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse_options::<Gemini>(&args(&[
            "-o",
            "x.lost",
            "-c",
            "--link",
            "example.org",
            "--search=cats",
            "--delay",
            "0.5",
            "--host-limit",
            "10",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(o.output, Some(PathBuf::from("x.lost")));
        assert!(o.continue_);
        assert_eq!(o.links, ["gemini://example.org/"]);
        assert_eq!(o.searches, ["cats"]);
        assert_eq!(o.delay, Duration::from_millis(500));
        assert_eq!(o.host_limit, Some(10));
        let o = parse_options::<Onion>(&args(&["--proxy", "socks5://localhost:9150", "-t", "90"]));
        let o = o.unwrap().unwrap();
        assert_eq!(o.proxy, Some(Proxy::Socks5("localhost:9150".into())));
        assert_eq!(o.timeout, Some(Duration::from_secs(90)));
        assert!(parse_options::<Onion>(&args(&["--link", "example.org"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["--proxy", "localhost:9050"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["-t", "0"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["-c"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["--dont-crawl"])).is_err());
        let o = parse_options::<Gemini>(&args(&["--dont-crawl", "--link", "a.org"]));
        assert!(o.unwrap().unwrap().dont_crawl);
        assert!(!parse_options::<Gemini>(&args(&[])).unwrap().unwrap().sign);
        assert!(
            parse_options::<Gemini>(&args(&["--sign"]))
                .unwrap()
                .unwrap()
                .sign
        );
        assert!(parse_options::<Gemini>(&args(&["--link", "https://x.org/"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["--delay", "-1"])).is_err());
        assert!(parse_options::<Gemini>(&args(&["--bogus"])).is_err());
        assert!(
            parse_options::<Gemini>(&args(&["--help"]))
                .unwrap()
                .is_none()
        );
    }
}
