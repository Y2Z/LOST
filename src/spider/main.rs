//! lost-spider: LOST's crawler (ARCHITECTURE.md §15). It wanders a network
//! and saves what it finds as a signed `.lost` file, which LOST imports like
//! any other. It's a program of its own, built only on request
//! (`--features spider`), so that LOST itself never goes online: it uses
//! LOST's library for the `.lost` format and your ID, and nothing in LOST
//! knows it exists.

mod crawl;
mod gemini;
mod gopher;

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

const USAGE: &str = "\
lost-spider — LOST's crawler. It wanders a network and saves what it finds
as a .lost file, signed with your ID, for LOST to import.

Usage:
  lost-spider gemini [OPTIONS]    Wander Geminispace
  lost-spider gopher [OPTIONS]    Wander Gopherspace

Options:
  -o, --output PATH       Where to save: a .lost file, or a folder to put one in
                          (default: lost-crawl-data-NETWORK-DATE-TIME.lost here;
                          if the file exists, \" (1)\", \" (2)\"… is added)
  -c, --continue          Carry on with the -o file: its pages are kept, links
                          they lead to are crawled, and the file is signed
                          with your ID. Give the other options again
      --link URL          Crawl this first, even if nothing links to it
                          (repeat for more)
      --links FILE        The same, one URL a line
      --dont-wander       Fetch only the --link URLs, and follow none of their
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
    dont_wander: bool,
    delay: Duration,
    host_limit: Option<u64>,
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
        "gemini" => {
            let Some(options) = parse_options(&args[1..], Gemini::SCHEME)? else {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            };
            crawl(Gemini::new()?, options)
        }
        "gopher" => {
            let Some(options) = parse_options(&args[1..], Gopher::SCHEME)? else {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            };
            crawl(Gopher, options)
        }
        other => {
            Err(format!("unknown network \"{other}\" (lost-spider knows: gemini, gopher)").into())
        }
    }
}

/// `None` for `--help`.
fn parse_options(args: &[String], scheme: &str) -> lost::Result<Option<Options>> {
    let mut options = Options {
        output: None,
        continue_: false,
        links: Vec::new(),
        searches: Vec::new(),
        blacklist: None,
        dont_wander: false,
        delay: Duration::from_secs(1),
        host_limit: None,
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
            "--link" => options.links.push(link(&value()?, scheme)?),
            "--links" => {
                for line in lines(Path::new(&value()?))? {
                    options.links.push(link(&line, scheme)?);
                }
            }
            "--search" => options.searches.push(value()?),
            "--searches" => options.searches.extend(lines(Path::new(&value()?))?),
            "--blacklist" => options.blacklist = Some(PathBuf::from(value()?)),
            "--dont-wander" => options.dont_wander = true,
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
            other => return Err(format!("unknown option {other} (see lost-spider --help)").into()),
        }
    }
    if options.dont_wander && options.links.is_empty() {
        return Err("--dont-wander needs --link or --links: the pages to fetch".into());
    }
    if options.continue_ && options.output.is_none() {
        return Err("-c needs the file to carry on with: -c -o FILE.lost".into());
    }
    Ok(Some(options))
}

/// A `--link`: `example.org` is taken as `gemini://example.org/`.
fn link(text: &str, scheme: &str) -> lost::Result<String> {
    let text = text.trim();
    let full = if text.contains("://") {
        text.to_string()
    } else {
        format!("{scheme}://{text}")
    };
    match Url::parse(&full) {
        Some(url) if url.scheme == scheme => Ok(url.to_string()),
        _ => Err(format!("\"{text}\" isn't a {scheme}:// link").into()),
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
    let blacklist = match &options.blacklist {
        Some(path) => Blacklist::load(path)?,
        None => Blacklist::default(),
    };
    let identity = load_identity();
    let path = output_path(options.output.as_deref(), options.continue_, P::SCHEME)?;

    let (mut writer, known, path) = if options.continue_ && path.exists() {
        let known = read_known(&path, P::SCHEME)?;
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
        Some(identity) => eprintln!("Saving to {name}, signed by {}.", identity.id()),
        None => eprintln!("Saving to {name}, unsigned."),
    }
    if before > 0 {
        eprintln!(
            "It has {} pages already; carrying on from their links.",
            thousands(before)
        );
    }

    let settings = Settings {
        delay: options.delay,
        host_limit: options.host_limit,
        searches: options.searches,
        blacklist,
        dont_wander: options.dont_wander,
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

    // With --dont-wander it only fetches what it's given; otherwise it wanders.
    let verb = if options.dont_wander {
        "Scraping"
    } else {
        "Wandering"
    };
    let terminal = std::io::stderr().is_terminal();
    let every = if terminal {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(60)
    };
    let status = |pages: u64| {
        let stats = crawler.stats();
        format!(
            "{} {}… {} pages saved · {} {} · {} links waiting",
            verb,
            P::SPACE,
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
    let started = Instant::now();
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
    let took = started.elapsed().as_secs();
    let took = format!("{}h {:02}m", took / 3600, took / 60 % 60);
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
            P::SCHEME,
            path.display()
        );
    }
    eprintln!("To search them: lost -d \"{}\"", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Your ID, made if there isn't one yet; `None` if LOST's folder can't be
/// found (no home folder).
fn load_identity() -> Option<Identity> {
    let dir = Identity::default_dir()?;
    match Identity::load_or_create(&dir) {
        Ok((identity, created)) => {
            if created {
                eprintln!(
                    "Made your ID: {} (kept in {}).",
                    identity.id(),
                    dir.display()
                );
            }
            Some(identity)
        }
        Err(e) => {
            eprintln!("lost-spider: {e}; the file won't be signed");
            None
        }
    }
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
    let (stem, extension) = if name.len() > 5 && name[split..].eq_ignore_ascii_case(".lost") {
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
fn read_known(path: &Path, scheme: &str) -> lost::Result<Known> {
    let mut reader = Reader::open(path)?;
    eprintln!("Reading {}…", reader.name());
    let mut known = Known::default();
    let mut damaged = 0;
    for i in 0..reader.chunk_count() {
        match reader.read_chunk(i)? {
            Some(chunk) => {
                for page in chunk.pages {
                    known.add(page, scheme);
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
    }

    #[test]
    fn options() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse_options(
            &args(&[
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
            ]),
            "gemini",
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.output, Some(PathBuf::from("x.lost")));
        assert!(o.continue_);
        assert_eq!(o.links, ["gemini://example.org/"]);
        assert_eq!(o.searches, ["cats"]);
        assert_eq!(o.delay, Duration::from_millis(500));
        assert_eq!(o.host_limit, Some(10));
        assert!(parse_options(&args(&["-c"]), "gemini").is_err());
        assert!(parse_options(&args(&["--dont-wander"]), "gemini").is_err());
        let o = parse_options(&args(&["--dont-wander", "--link", "a.org"]), "gemini");
        assert!(o.unwrap().unwrap().dont_wander);
        assert!(parse_options(&args(&["--link", "https://x.org/"]), "gemini").is_err());
        assert!(parse_options(&args(&["--delay", "-1"]), "gemini").is_err());
        assert!(parse_options(&args(&["--bogus"]), "gemini").is_err());
        assert!(
            parse_options(&args(&["--help"]), "gemini")
                .unwrap()
                .is_none()
        );
    }
}
