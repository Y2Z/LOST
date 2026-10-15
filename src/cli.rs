//! Command-line front end: works out the mode (ARCHITECTURE.md §3) and runs it.
//!
//! Exit codes, like grep: 0 results found (or success), 1 no results, 2 error.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::args::{self, Args, Format, Mode, USAGE};
use crate::config::Config;
use crate::datasources::Progress;
use crate::output;
use crate::query::Limit;
use crate::text::single_line;
use crate::{Answer, Identity, Lost, Question, Result};

pub const EXIT_FOUND: i32 = 0;
pub const EXIT_NOT_FOUND: i32 = 1;
pub const EXIT_ERROR: i32 = 2;

/// Pages imported the first time LOST starts (see [`open_database`]).
static SAMPLE: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();

/// Runs the command line and returns the exit code. `sample` is a `.lost`
/// file imported the very first time LOST runs, when there's no database
/// yet; the `lost` program passes `res/data/sample.lost`.
pub fn run(sample: Option<&'static [u8]>) -> i32 {
    if let Some(sample) = sample {
        let _ = SAMPLE.set(sample);
    }
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => return usage_error(&e),
    };
    let mode = match args.mode() {
        Ok(mode) => mode,
        Err(e) => return usage_error(&e),
    };

    let result = match mode {
        Mode::Help => {
            print!("{USAGE}");
            Ok(EXIT_FOUND)
        }
        Mode::Version => {
            println!("lost {}", env!("CARGO_PKG_VERSION"));
            Ok(EXIT_FOUND)
        }
        Mode::Search => search(&args),
        Mode::Suggest => suggest(args.suggest.as_deref().unwrap_or("")),
        Mode::Import => import(&args),
        Mode::Export => export(&args),
        Mode::Server { background } => server(&args, background),
        Mode::Gui { with_server } => gui(&args, with_server),
        Mode::Imports => list_imports(&args),
        Mode::RemoveImport(id) => remove_import(&args, id),
        Mode::Trusted => list_trusted(&args),
        Mode::Untrust(id) => untrust(&id),
        Mode::NewId => new_id(args.yes),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("lost: {e}");
            EXIT_ERROR
        }
    }
}

fn usage_error(message: &str) -> i32 {
    eprintln!("lost: {message}\nRun 'lost -h' for help.");
    EXIT_ERROR
}

fn open() -> Result<(Config, Lost)> {
    let config = Config::load()?;
    let data_dir = config.data_dir()?;
    let lost = open_database(&data_dir)?;
    Ok((config, lost))
}

/// Opens the database. The first time ever (no index yet), it starts out
/// with the sample pages, as import #1.
fn open_database(data_dir: &std::path::Path) -> Result<Lost> {
    let first_time = !data_dir.join(crate::engine::INDEX_DIR).exists();
    let lost = Lost::open(data_dir)?;
    if first_time
        && let Some(sample) = SAMPLE.get()
        && let Err(e) = import_sample(&lost, sample)
    {
        eprintln!("lost: the sample pages weren't added: {e}");
    }
    Ok(lost)
}

/// Imports `bytes` as a file named sample.lost (the name the import shows).
fn import_sample(lost: &Lost, bytes: &[u8]) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("lost-sample-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("sample.lost");
    let result = std::fs::write(&path, bytes)
        .map_err(Into::into)
        .and_then(|()| {
            lost.import(
                std::slice::from_ref(&path),
                crate::config::Trust::Any,
                &mut |_| crate::datasources::lost::Answer::No,
                &mut |_| {},
            )
        });
    let _ = std::fs::remove_dir_all(&dir);
    result.map(drop)
}

fn search(args: &Args) -> Result<i32> {
    let query = match &args.input {
        Some(path) => read_input(path)?,
        None => args.query.join(" "),
    };
    let query = single_line(&query);
    if query.is_empty() {
        return Err("the search query is empty".into());
    }
    if let Some(url) = crate::query::cache_url(&query) {
        return cached(args, &url?);
    }

    let (config, lost) = open()?;
    if lost.is_empty() {
        eprintln!("lost: nothing to search yet; import a dataset first: lost -d FILE");
        return Ok(EXIT_NOT_FOUND);
    }
    let limit = args
        .limit
        .unwrap_or(Limit::new(0, config.search.default_limit));
    let results = lost.search(&query, limit)?;

    let to_stdout = matches!(args.output.as_deref(), None | Some("-"));
    let plain_on_terminal = to_stdout
        && args.format.unwrap_or(Format::Plain) == Format::Plain
        && io::stdout().is_terminal();
    {
        let mut out: Box<dyn Write> = match args.output.as_deref() {
            None | Some("-") => Box::new(io::BufWriter::new(io::stdout().lock())),
            Some(path) => Box::new(io::BufWriter::new(
                File::create(path).map_err(|e| format!("can't write {path}: {e}"))?,
            )),
        };
        let style = if to_stdout {
            output::terminal_style()
        } else {
            output::Style::default()
        };
        let written = match args.format.unwrap_or(Format::Plain) {
            Format::Plain => output::plain(&results, &mut out, style),
            Format::Json => output::json(&results, &mut out),
        };
        match written.and_then(|_| out.flush()) {
            Ok(()) => {}
            // `lost … | head` closes the pipe early; that's not an error.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Err(e) => return Err(e.into()),
        }
    }

    // A blank line keeps the summary apart from the last result.
    if plain_on_terminal && !results.results.is_empty() {
        eprintln!();
    }
    eprintln!("{} ({} ms)", output::summary(&results), results.took_ms);
    Ok(if results.results.is_empty() {
        EXIT_NOT_FOUND
    } else {
        EXIT_FOUND
    })
}

/// `cache:URL`: the stored text of one page (ARCHITECTURE.md §7.6).
fn cached(args: &Args, url: &str) -> Result<i32> {
    if args.limit.is_some() {
        return Err("-l doesn't apply to cache:".into());
    }
    let (_, lost) = open()?;
    let Some(page) = lost.page(url, None)? else {
        eprintln!("lost: {url} isn't in the database");
        return Ok(EXIT_NOT_FOUND);
    };
    let to_stdout = matches!(args.output.as_deref(), None | Some("-"));
    let mut out: Box<dyn Write> = match args.output.as_deref() {
        None | Some("-") => Box::new(io::BufWriter::new(io::stdout().lock())),
        Some(path) => Box::new(io::BufWriter::new(
            File::create(path).map_err(|e| format!("can't write {path}: {e}"))?,
        )),
    };
    let style = if to_stdout {
        output::terminal_style()
    } else {
        output::Style::default()
    };
    let written = match args.format.unwrap_or(Format::Plain) {
        Format::Plain => output::page(&page, &mut out, style),
        Format::Json => serde_json::to_writer_pretty(&mut out, &page)
            .map_err(io::Error::from)
            .and_then(|_| writeln!(out)),
    };
    match written.and_then(|_| out.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => return Err(e.into()),
    }
    Ok(EXIT_FOUND)
}

fn suggest(text: &str) -> Result<i32> {
    let (_, lost) = open()?;
    let suggestions = lost.suggest(text, 10)?;
    for s in &suggestions {
        println!("{s}");
    }
    Ok(if suggestions.is_empty() {
        EXIT_NOT_FOUND
    } else {
        EXIT_FOUND
    })
}

/// `-s` and `-b`: the web server, in the foreground or the background.
#[cfg(feature = "server")]
fn server(args: &Args, background: bool) -> Result<i32> {
    let config = Config::load()?;
    let data_dir = config.data_dir()?;
    let address = args
        .address
        .clone()
        .unwrap_or_else(|| config.server.address.clone());
    let port = args.port.unwrap_or(config.server.port);
    if background {
        // Checked here too, so the message comes straight to the terminal.
        if let Some(status) = crate::instance::ask(crate::instance::Role::Server(port), "status") {
            return Err(already_running(&status, &address, port).into());
        }
        return background_server(&data_dir, &address, port);
    }
    let lost = Arc::new(open_database(&data_dir)?);
    claim_server(&address, port, &lost)?;
    crate::server::run(lost, server_options(&config, address, port))?;
    Ok(EXIT_FOUND)
}

#[cfg(feature = "server")]
fn server_options(config: &Config, address: String, port: u16) -> crate::server::Options {
    crate::server::Options {
        address,
        port,
        max_results: config.server.max_results,
        default_limit: config.search.default_limit,
        cached_links: config.search.cached_links,
    }
}

/// Takes the "server on this port" role (ARCHITECTURE.md §4), or says who has it.
#[cfg(feature = "server")]
fn claim_server(address: &str, port: u16, lost: &Arc<Lost>) -> Result<()> {
    use crate::instance::{self, Claim, Role};
    match instance::claim(Role::Server(port))? {
        Claim::Running(status) => Err(already_running(&status, address, port).into()),
        Claim::Ours(listener) => {
            let lost = lost.clone();
            let address = address.to_string();
            listener.serve(Arc::new(move |command, _| match command {
                "status" => serde_json::json!({
                    "ok": true,
                    "version": env!("CARGO_PKG_VERSION"),
                    "role": "server",
                    "address": address,
                    "port": port,
                    "documents": lost.document_count(),
                }),
                "quit" => {
                    // Answer first, then go.
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                        std::process::exit(EXIT_FOUND);
                    });
                    serde_json::json!({ "ok": true })
                }
                other => instance::unknown(other),
            }));
            Ok(())
        }
    }
}

#[cfg(feature = "server")]
fn already_running(status: &serde_json::Value, address: &str, port: u16) -> String {
    let address = status["address"].as_str().unwrap_or(address);
    let host = if address.contains(':') {
        format!("[{address}]")
    } else {
        address.to_string()
    };
    format!("LOST server already running at {host}:{port}")
}

#[cfg(all(feature = "server", unix))]
fn background_server(data_dir: &std::path::Path, address: &str, port: u16) -> Result<i32> {
    crate::config::ensure_dir(data_dir)?;
    let (pid, log) = crate::daemon::start(data_dir, address, port)?;
    eprintln!(
        "LOST server is running in the background on {address}:{port} (PID {pid}).\n\
         Its messages go to {}. Stop it with: kill {pid}",
        log.display()
    );
    Ok(EXIT_FOUND)
}

#[cfg(all(feature = "server", not(unix)))]
fn background_server(_: &std::path::Path, _: &str, _: u16) -> Result<i32> {
    Err("-b isn't available on this system; run lost -s instead".into())
}

/// The desktop app; with `-w`, the web server too, in the same process.
#[cfg(feature = "gui")]
fn gui(args: &Args, with_server: bool) -> Result<i32> {
    use crate::instance::{self, Claim, Role};

    // One app per user: a second `lost` brings the first one's window up.
    let listener = match instance::claim(Role::Gui)? {
        Claim::Ours(listener) => listener,
        Claim::Running(_) if args.minimized => {
            eprintln!("LOST is already open.");
            return Ok(EXIT_FOUND);
        }
        Claim::Running(_) => {
            instance::ask(Role::Gui, "raise");
            eprintln!("LOST is already open; its window has been brought to the front.");
            if with_server {
                eprintln!("(-w was ignored: quit LOST first to start it with its web server.)");
            }
            return Ok(EXIT_FOUND);
        }
    };

    #[cfg(windows)]
    hide_console();

    let config = Config::load()?;
    let data_dir = config.data_dir()?;
    let lost = Arc::new(open_database(&data_dir)?);

    let server = if with_server {
        Some(gui_server(args, &config, &lost)?)
    } else {
        None
    };

    crate::gui::run(
        lost,
        crate::gui::Options {
            tray: config.gui.tray && !args.no_tray,
            minimized: args.minimized,
            data_dir,
            config: config.clone(),
            config_path: crate::config::user_dir().map(|d| d.join("lost.toml")),
            default_limit: config.search.default_limit,
            max_results: config.server.max_results,
            server,
        },
        listener,
    )?;
    Ok(EXIT_FOUND)
}

/// On Windows, lost.exe is a console program (for the command line), so
/// starting it from the Start menu or Explorer opens a console window too.
/// When that console is ours alone, let it go: it closes.
#[cfg(all(feature = "gui", windows))]
fn hide_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut processes = [0u32; 2];
    // SAFETY: the buffer and its length match; FreeConsole takes nothing.
    unsafe {
        if GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) == 1 {
            FreeConsole();
        }
    }
}

/// `-w`: the web server on a thread of the app's process.
#[cfg(all(feature = "gui", feature = "server"))]
fn gui_server(args: &Args, config: &Config, lost: &Arc<Lost>) -> Result<crate::gui::ServerInfo> {
    let address = args
        .address
        .clone()
        .unwrap_or_else(|| config.server.address.clone());
    let port = args.port.unwrap_or(config.server.port);
    claim_server(&address, port, lost)?;
    let error = Arc::new(std::sync::Mutex::new(None));
    let host = if address.contains(':') {
        format!("[{address}]")
    } else {
        address.clone()
    };
    let url = format!(
        "http://{}:{port}",
        if address == "0.0.0.0" || address == "::" {
            "127.0.0.1".to_string()
        } else {
            host
        }
    );
    let options = server_options(config, address, port);
    let (lost, failed) = (lost.clone(), error.clone());
    std::thread::Builder::new()
        .name("lost-server".into())
        .spawn(move || {
            match crate::server::run(lost, options) {
                // Stopped by Ctrl+C (or SIGTERM): the app goes too. It asks
                // first if an import is running.
                Ok(()) => {
                    crate::instance::ask(crate::instance::Role::Gui, "quit");
                }
                Err(e) => {
                    eprintln!("lost: web server: {e}");
                    if let Ok(mut slot) = failed.lock() {
                        *slot = Some(e.to_string());
                    }
                }
            }
        })?;
    Ok(crate::gui::ServerInfo { url, error })
}

#[cfg(all(feature = "gui", not(feature = "server")))]
fn gui_server(_: &Args, _: &Config, _: &Arc<Lost>) -> Result<crate::gui::ServerInfo> {
    Err("this build of LOST has no web server (it was built without the \"server\" feature)".into())
}

#[cfg(not(feature = "gui"))]
fn gui(_: &Args, _: bool) -> Result<i32> {
    Err("this build of LOST has no desktop app (it was built without the \"gui\" feature); see lost -h".into())
}

#[cfg(not(feature = "server"))]
fn server(_: &Args, _: bool) -> Result<i32> {
    Err("this build of LOST has no web server (it was built without the \"server\" feature)".into())
}

fn read_input(path: &str) -> Result<String> {
    let mut text = String::new();
    if path == "-" {
        io::stdin().read_to_string(&mut text)?;
    } else {
        File::open(path)
            .and_then(|mut f| f.read_to_string(&mut text))
            .map_err(|e| format!("can't read {path}: {e}"))?;
    }
    Ok(text)
}

fn import(args: &Args) -> Result<i32> {
    let (config, lost) = open()?;
    let paths: Vec<PathBuf> = args.data.clone();
    let interactive = io::stderr().is_terminal();
    let trust = config.trust();

    let mut ask = |q: &Question| ask_on_terminal(q);
    let mut report = |p: Progress| {
        if interactive && let Progress::Records(records) = p {
            eprint!("\rRead {} records…", output::thousands(records));
        }
    };
    let result = lost.import(&paths, trust, &mut ask, &mut report);
    if interactive {
        eprint!("\r\x1b[K");
    }
    let rows = match result {
        Err(e) if e.downcast_ref::<crate::Declined>().is_some() => {
            eprintln!("Nothing imported.");
            return Ok(EXIT_NOT_FOUND);
        }
        other => other?,
    };

    let n = output::thousands;
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            eprintln!();
        }
        eprintln!(
            "Imported {} as import #{} [{}]",
            row.files.join(", "),
            row.id,
            row.source
        );
        eprintln!(
            "  {} pages: {} new, {} updated, {} already in the database (duplicates)",
            n(row.records),
            n(row.new),
            n(row.updated),
            n(row.unchanged)
        );
        if row.repeated > 0 {
            eprintln!(
                "  {} pages appeared more than once in this import and were merged",
                n(row.repeated)
            );
        }
        if row.invalid > 0 {
            eprintln!("  {} records skipped: unusable URL", n(row.invalid));
        }
        for note in &row.notes {
            eprintln!("  {note}");
        }
    }
    Ok(EXIT_FOUND)
}

/// Asks whether to import a `.lost` file from someone not trusted yet
/// (ARCHITECTURE.md §9.2). Where nobody can answer (no terminal), the
/// answer is no, and the message says how to import it anyway.
fn ask_on_terminal(q: &Question) -> Answer {
    let pages = output::thousands(q.pages);
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        match &q.signer {
            Some(id) => eprintln!(
                "lost: {} is signed by {id}, who isn't trusted yet; not imported.",
                q.file
            ),
            None => eprintln!("lost: {} isn't signed; not imported.", q.file),
        }
        eprintln!(
            "  Import it from a terminal to be asked, or set trust = \"any\" under [import] in lost.toml."
        );
        return Answer::No;
    }
    match &q.signer {
        Some(id) => {
            if !confirm(&format!(
                "Import {} ({pages} pages), signed by {id} (not trusted yet)? [y/N] ",
                q.file
            )) {
                Answer::No
            } else if confirm(&format!("Trust {id} from now on? [y/N] ")) {
                Answer::YesAndTrust
            } else {
                Answer::Yes
            }
        }
        None => {
            if confirm(&format!(
                "{} ({pages} pages) isn't signed, so there's no telling who made it. Import it anyway? [y/N] ",
                q.file
            )) {
                Answer::Yes
            } else {
                Answer::No
            }
        }
    }
}

/// Asks a yes/no question on the terminal; no unless the answer is y or yes.
fn confirm(question: &str) -> bool {
    eprint!("{question}");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// `--new-id`: a new signing key in place of the old one, after asking.
fn new_id(yes: bool) -> Result<i32> {
    let (_, lost) = open()?;
    let current = crate::lostfile::Identity::default_dir()
        .and_then(|dir| crate::lostfile::Identity::load(&dir).ok().flatten())
        .map(|identity| identity.id());
    if let Some(id) = &current
        && !yes
    {
        if !io::stdin().is_terminal() {
            return Err("--new-id asks first; add --yes to go ahead without asking".into());
        }
        eprintln!("Your ID is {id}.");
        eprintln!(
            "Files you've signed keep it; people who trust it will be asked about files with the new one."
        );
        if !confirm("Make a new ID? [y/N] ") {
            eprintln!("Your ID wasn't changed.");
            return Ok(EXIT_NOT_FOUND);
        }
    }
    let (old, new) = lost.new_identity()?;
    match old {
        Some(old) => eprintln!(
            "Your new ID is {new}. Files signed with {old} still import without a question."
        ),
        None => eprintln!("Your ID is {new}."),
    }
    Ok(EXIT_FOUND)
}

/// `-e FILE [QUERY…]`: exports the database, or what the query finds.
fn export(args: &Args) -> Result<i32> {
    let Some(path) = args.export.clone() else {
        return Err("-e needs a file name".into());
    };
    let query = match &args.input {
        Some(input) => read_input(input)?,
        None => args.query.join(" "),
    };
    let query = single_line(&query);
    let query = (!query.is_empty()).then_some(query);

    let (_, lost) = open()?;
    // Unsigned unless asked: no key is read or made without --sign.
    let identity = if !args.sign {
        None
    } else {
        let dir = Identity::default_dir()
            .ok_or("can't find your home folder to keep a signing key in; leave out --sign")?;
        let (identity, created) = Identity::load_or_create(&dir)?;
        if created {
            eprintln!(
                "Made your signing key: {}\n\
                 Files you sign carry your ID, {}. Keep the key private: whoever has it can sign as you.",
                dir.join(crate::lostfile::KEY_FILE).display(),
                identity.id()
            );
        }
        Some(identity)
    };

    let interactive = io::stderr().is_terminal();
    let mut report = |done: u64, total: u64| {
        if interactive {
            eprint!(
                "\rExported {} of {} pages…",
                output::thousands(done),
                output::thousands(total)
            );
        }
    };
    let selection = match &query {
        Some(q) => crate::Selection::Search(q),
        None => crate::Selection::All,
    };
    let result = lost.export(&path, selection, identity.as_ref(), &mut report);
    if interactive {
        eprint!("\r\x1b[K");
    }
    let export = result?;
    eprintln!(
        "Exported {} page{} to {} ({}){}",
        output::thousands(export.pages),
        if export.pages == 1 { "" } else { "s" },
        path.display(),
        crate::text::format_bytes(export.bytes),
        match &export.signer {
            Some(id) => format!(", signed as {id}"),
            None => ", not signed".to_string(),
        }
    );
    Ok(EXIT_FOUND)
}

// ── Imports and trust (what Settings shows in the app) ────────────────

/// Writes `value` as pretty JSON to stdout.
fn print_json<T: serde::Serialize>(value: &T) -> Result<i32> {
    let mut out = io::stdout().lock();
    match serde_json::to_writer_pretty(&mut out, value)
        .map_err(io::Error::from)
        .and_then(|_| writeln!(out))
    {
        Ok(()) => Ok(EXIT_FOUND),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(EXIT_FOUND),
        Err(e) => Err(e.into()),
    }
}

/// Lines of a table: columns padded to their widest cell (numbers to the
/// right), the last one as it is.
fn table(rows: &[Vec<String>], numeric: &[bool]) -> Vec<String> {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    line.push_str("  ");
                }
                let pad = widths[c] - cell.chars().count();
                if c + 1 == columns {
                    line.push_str(cell);
                } else if numeric.get(c).copied().unwrap_or(false) {
                    line.push_str(&" ".repeat(pad));
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    line.push_str(&" ".repeat(pad));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// `--imports`: past imports, oldest first, as Settings → Data lists them.
fn list_imports(args: &Args) -> Result<i32> {
    let (_, lost) = open()?;
    let rows = lost.imports()?;
    if args.format == Some(Format::Json) {
        return print_json(&rows);
    }
    if rows.is_empty() {
        eprintln!("No imports yet. Import a dataset with: lost -d FILE");
        return Ok(EXIT_NOT_FOUND);
    }
    let n = output::thousands;
    let mut cells = vec![
        [
            "#",
            "Imported on",
            "Source",
            "Pages",
            "New",
            "Replaced older",
            "Status",
            "Files",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for row in &rows {
        cells.push(vec![
            row.id.to_string(),
            output::local_datetime(row.started),
            crate::text::source_label(&row.source),
            n(row.records),
            n(row.new),
            n(row.updated),
            if row.status == "running" {
                "interrupted".to_string()
            } else {
                row.status.clone()
            },
            row.files.join(", "),
        ]);
    }
    let numeric = [true, false, false, true, true, true, false, false];
    let width = output::terminal_style().width;
    let mut out = io::stdout().lock();
    for line in table(&cells, &numeric) {
        // On a terminal, long file lists are cut to fit.
        let line = match width {
            Some(w) if line.chars().count() > w => {
                let mut cut: String = line.chars().take(w.saturating_sub(1)).collect();
                cut.push('…');
                cut
            }
            _ => line,
        };
        if writeln!(out, "{line}").is_err() {
            break; // `| head`
        }
    }
    Ok(EXIT_FOUND)
}

/// `--remove-import ID`: removes an import and the pages whose current
/// version came from it, after asking (unless `--yes`).
fn remove_import(args: &Args, id: u64) -> Result<i32> {
    let (_, lost) = open()?;
    let rows = lost.imports()?;
    let Some(row) = rows.iter().find(|r| r.id == id) else {
        return Err(format!("there's no import #{id} (lost --imports lists them)").into());
    };
    let pages = lost.import_pages(id)?;
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    if !args.yes {
        if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
            return Err("removing asks first: run it in a terminal, or add --yes".into());
        }
        if !confirm(&format!(
            "Remove import #{id} ({}) and its {} page{}? [y/N] ",
            crate::text::files_summary(&row.files),
            output::thousands(pages),
            plural(pages)
        )) {
            eprintln!("Nothing removed.");
            return Ok(EXIT_NOT_FOUND);
        }
    }
    let removed = lost.remove_import(id)?;
    eprintln!(
        "Removed import #{id}: {} page{}.",
        output::thousands(removed),
        plural(removed)
    );
    Ok(EXIT_FOUND)
}

/// `--trusted`: whose signed `.lost` files import without a question.
fn list_trusted(args: &Args) -> Result<i32> {
    let (_, lost) = open()?;
    let rows = lost.trusted()?;
    if args.format == Some(Format::Json) {
        return print_json(&rows);
    }
    let me = Identity::default_dir()
        .and_then(|dir| Identity::load(&dir).ok().flatten())
        .map(|identity| identity.id());
    if rows.is_empty() {
        eprintln!(
            "Nobody yet. When you import a signed file from someone new, you're asked whether to trust them."
        );
    } else {
        let mut cells = vec![["ID", "Since", "Key"].map(String::from).to_vec()];
        for row in &rows {
            cells.push(vec![
                crate::lostfile::short_id_hex(&row.key),
                output::local_date(row.added),
                row.key.clone(),
            ]);
        }
        for line in table(&cells, &[]) {
            println!("{line}");
        }
    }
    if let Some(id) = me {
        eprintln!("Files you sign yourself ({id}) are always trusted.");
    }
    Ok(if rows.is_empty() {
        EXIT_NOT_FOUND
    } else {
        EXIT_FOUND
    })
}

/// `--untrust ID`: the ID as shown (`3f9a 1c2e 77b0 1d4e`), the full key,
/// or enough of its start to tell it apart.
fn untrust(id: &str) -> Result<i32> {
    let wanted: String = id
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    if wanted.len() < 4 || !wanted.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "\"{id}\" isn't an ID like 3f9a 1c2e 77b0 1d4e (lost --trusted lists them)"
        )
        .into());
    }
    let (_, lost) = open()?;
    let matching: Vec<_> = lost
        .trusted()?
        .into_iter()
        .filter(|row| row.key.starts_with(&wanted))
        .collect();
    match matching.as_slice() {
        [] => Err(format!("nobody trusted has the ID {id} (lost --trusted lists them)").into()),
        [row] => {
            lost.untrust(&row.key)?;
            eprintln!(
                "Stopped trusting {}. Files from them will be asked about again.",
                crate::lostfile::short_id_hex(&row.key)
            );
            Ok(EXIT_FOUND)
        }
        _ => Err(format!(
            "{id} matches {} people; give more of the ID (lost --trusted shows the full keys)",
            matching.len()
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::table;

    #[test]
    fn tables_line_up() {
        let rows = vec![
            vec!["#".to_string(), "Name".into(), "Files".into()],
            vec!["10".to_string(), "a".into(), "x, y".into()],
            vec!["2".to_string(), "long name".into(), "z".into()],
        ];
        assert_eq!(
            table(&rows, &[true]),
            vec![
                " #  Name       Files",
                "10  a          x, y",
                " 2  long name  z"
            ]
        );
    }
}
