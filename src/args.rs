//! Command-line argument parsing (ARCHITECTURE.md §3.1, §3.3).
//!
//! Hand-written rather than generated, because search queries need special
//! treatment: `lost search engine -yandex` must read `-yandex` as part of the
//! query, not as an unknown option. The rule: only `-X` (exactly two
//! characters) and `--long` are options; any other word starting with `-` is a
//! query word. Everything after `--` is query.

use std::path::PathBuf;

use crate::query::Limit;

pub const USAGE: &str = "\
LOST — Local Offline Search Tool

Usage:
  lost [OPTIONS] QUERY…       Search
  lost -d PATH…               Import datasets (files or folders)
  lost -e FILE [QUERY…]       Export the database (or what QUERY finds)
                              as a .lost file
  lost -s [-a ADDR] [-p PORT] Run the web server
  lost -b [-a ADDR] [-p PORT] Run the web server in the background
  lost [-t] [-w] [--minimized]
                              Open the desktop app (with -w: plus web server)
  lost --imports              List past imports
  lost --remove-import ID     Remove an import and its pages
  lost --trusted              List the people whose .lost files you trust
  lost --untrust ID           Stop trusting someone
  lost --new-id               Make a new ID to sign your exports with

Search options:
  -f, --format FORMAT     Output format: plain (default) or json
  -i, --input FILE        Read the query from FILE (- for stdin)
  -l, --limit [OFFSET,]COUNT
                          Like SQL's LIMIT: -l 10 shows results 1-10,
                          -l 10,20 skips 10 and shows the next 20 (default 50)
  -o, --output FILE       Write results to FILE (- for stdout, the default)

Server options:
  -a, --address ADDR      Address to listen on (default 127.0.0.1)
  -p, --port PORT         Port to listen on (default 1057)

Export options:
      --unsigned          Don't sign the file (it's signed by default)

Data options:
  -f, --format FORMAT     --imports and --trusted: plain (default) or json
      --yes               --remove-import, --new-id: don't ask first

Other options:
  -t, --no-tray           Desktop app without a tray icon
      --minimized         Desktop app: start in the tray (minimized without one)
      --suggest TEXT      Print autocomplete suggestions for TEXT
                          (a standalone _ is a wildcard: --suggest 'how to _ fish')
  -h, --help              Show this help
  -V, --version           Show the version

Query words starting with '-' exclude results: lost search engine -yandex
Put -- before a query that starts with '-': lost -- -yandex search engine

Exit status: 0 results found, 1 no results, 2 error.
";

/// Output format for search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Plain,
    Json,
}

/// Everything given on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub address: Option<String>,
    pub background: bool,
    pub data: Vec<PathBuf>,
    pub export: Option<PathBuf>,
    pub format: Option<Format>,
    pub input: Option<String>,
    pub limit: Option<Limit>,
    pub output: Option<String>,
    pub port: Option<u16>,
    pub server: bool,
    pub no_tray: bool,
    pub minimized: bool,
    pub with_server: bool,
    pub help: bool,
    pub version: bool,
    pub suggest: Option<String>,
    pub unsigned: bool,
    pub imports: bool,
    pub remove_import: Option<u64>,
    pub trusted: bool,
    pub untrust: Option<String>,
    pub new_id: bool,
    pub yes: bool,
    pub query: Vec<String>,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Help,
    Version,
    Suggest,
    Search,
    Import,
    Export,
    Server { background: bool },
    Gui { with_server: bool },
    Imports,
    RemoveImport(u64),
    Trusted,
    Untrust(String),
    NewId,
}

const VALUE_OPTIONS: &[(char, &str)] = &[
    ('a', "address"),
    ('d', "data"),
    ('e', "export"),
    ('f', "format"),
    ('i', "input"),
    ('l', "limit"),
    ('o', "output"),
    ('p', "port"),
];

/// Options with a value that have no one-letter form.
const LONG_VALUE_OPTIONS: &[&str] = &["--suggest", "--remove-import", "--untrust"];

const FLAG_OPTIONS: &[(char, &str)] = &[
    ('b', "background"),
    ('s', "server"),
    ('t', "no-tray"),
    ('w', "with-server"),
    ('h', "help"),
    ('V', "version"),
];

/// Parses arguments (without the program name).
pub fn parse<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut out = Args::default();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        i += 1;

        if arg == "--" {
            out.query.extend(args[i..].iter().cloned());
            break;
        }

        // Long-only options.
        match arg.as_str() {
            "--unsigned" => out.unsigned = true,
            "--imports" => out.imports = true,
            "--trusted" => out.trusted = true,
            "--new-id" => out.new_id = true,
            "--minimized" => out.minimized = true,
            "--yes" => out.yes = true,
            _ => {}
        }
        if matches!(
            arg.as_str(),
            "--unsigned" | "--imports" | "--trusted" | "--new-id" | "--minimized" | "--yes"
        ) {
            continue;
        }
        if let Some((name, rest)) = LONG_VALUE_OPTIONS
            .iter()
            .find_map(|name| arg.strip_prefix(name).map(|rest| (*name, rest)))
        {
            let value = match rest.strip_prefix('=') {
                Some(v) => v.to_string(),
                None if rest.is_empty() => {
                    let v = args
                        .get(i)
                        .cloned()
                        .ok_or_else(|| format!("option {arg} needs a value"))?;
                    i += 1;
                    v
                }
                None => return Err(format!("unknown option {arg}")),
            };
            match name {
                "--suggest" => set_once(&mut out.suggest, value, name)?,
                "--remove-import" => {
                    let id = value
                        .trim()
                        .trim_start_matches('#')
                        .parse::<u64>()
                        .map_err(|_| format!("{name} needs an import number, like 4"))?;
                    set_once(&mut out.remove_import, id, name)?
                }
                _ => set_once(&mut out.untrust, value, name)?,
            }
            continue;
        }

        // Identify the option, if this is one.
        let (letter, inline_value) = if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let letter = VALUE_OPTIONS
                .iter()
                .chain(FLAG_OPTIONS)
                .find(|(_, n)| *n == name)
                .map(|(c, _)| *c)
                .ok_or_else(|| format!("unknown option --{name}"))?;
            (letter, value)
        } else if arg.len() == 2 && arg.starts_with('-') && arg != "--" {
            let c = arg.chars().nth(1).unwrap_or('-');
            if !VALUE_OPTIONS
                .iter()
                .chain(FLAG_OPTIONS)
                .any(|(l, _)| *l == c)
            {
                return Err(format!("unknown option -{c}"));
            }
            (c, None)
        } else {
            // A query word (including "-word" exclusions and a lone "-").
            out.query.push(arg.clone());
            continue;
        };

        if FLAG_OPTIONS.iter().any(|(c, _)| *c == letter) {
            if inline_value.is_some() {
                return Err(format!("option {arg} doesn't take a value"));
            }
            match letter {
                'b' => out.background = true,
                's' => out.server = true,
                't' => out.no_tray = true,
                'w' => out.with_server = true,
                'h' => out.help = true,
                'V' => out.version = true,
                _ => unreachable!(),
            }
            continue;
        }

        let value = match inline_value {
            Some(v) => v,
            None => {
                let v = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| format!("option {arg} needs a value"))?;
                i += 1;
                v
            }
        };

        match letter {
            'a' => set_once(&mut out.address, value, arg)?,
            'd' => out.data.push(PathBuf::from(value)),
            'e' => set_once(&mut out.export, PathBuf::from(value), arg)?,
            'f' => {
                let format = match value.as_str() {
                    "plain" => Format::Plain,
                    "json" => Format::Json,
                    other => return Err(format!("unknown format \"{other}\": use plain or json")),
                };
                set_once(&mut out.format, format, arg)?
            }
            'i' => set_once(&mut out.input, value, arg)?,
            'l' => set_once(&mut out.limit, value.parse::<Limit>()?, arg)?,
            'o' => set_once(&mut out.output, value, arg)?,
            'p' => {
                let port = value
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| format!("invalid port \"{value}\""))?;
                set_once(&mut out.port, port, arg)?
            }
            _ => unreachable!(),
        }
    }
    Ok(out)
}

fn set_once<T>(slot: &mut Option<T>, value: T, arg: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("option {arg} given more than once"));
    }
    *slot = Some(value);
    Ok(())
}

impl Args {
    /// Works out the mode and checks that the options fit it (§3.3).
    pub fn mode(&self) -> Result<Mode, String> {
        if self.help {
            return Ok(Mode::Help);
        }
        if self.version {
            return Ok(Mode::Version);
        }

        let has_query = !self.query.is_empty() || self.input.is_some();
        let server = self.server || self.background;
        let importing = !self.data.is_empty();
        let exporting = self.export.is_some();

        if !self.query.is_empty() && self.input.is_some() {
            return Err("give the query either as words or with -i, not both".into());
        }
        // With -e, a query says what to export.
        let exclusive = [
            (has_query && !exporting, "a search query"),
            (self.suggest.is_some(), "--suggest"),
            (importing, "-d"),
            (exporting, "-e"),
            (server, if self.background { "-b" } else { "-s" }),
            (self.with_server, "-w"),
            (self.imports, "--imports"),
            (self.remove_import.is_some(), "--remove-import"),
            (self.trusted, "--trusted"),
            (self.untrust.is_some(), "--untrust"),
            (self.new_id, "--new-id"),
        ];
        let chosen: Vec<&str> = exclusive
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, n)| *n)
            .collect();
        if chosen.len() > 1 {
            return Err(format!("{} can't be combined", chosen.join(" and ")));
        }

        let listing = self.imports || self.trusted;
        let search_only = [
            (self.format.is_some() && !listing, "-f"),
            (self.limit.is_some(), "-l"),
            (self.output.is_some(), "-o"),
        ];
        if (!has_query || exporting)
            && let Some((_, name)) = search_only.iter().find(|(on, _)| *on)
        {
            return Err(format!("{name} only applies to a search"));
        }
        if self.unsigned && !exporting {
            return Err("--unsigned only applies to -e".into());
        }
        if self.yes && self.remove_import.is_none() && !self.new_id {
            return Err("--yes only applies to --remove-import and --new-id".into());
        }
        let server_only = [(self.address.is_some(), "-a"), (self.port.is_some(), "-p")];
        if !server
            && !self.with_server
            && let Some((_, name)) = server_only.iter().find(|(on, _)| *on)
        {
            return Err(format!(
                "{name} only applies to the web server (-s, -b, -w)"
            ));
        }
        let other_mode = has_query
            || importing
            || exporting
            || server
            || self.suggest.is_some()
            || self.imports
            || self.remove_import.is_some()
            || self.trusted
            || self.untrust.is_some()
            || self.new_id;
        if self.no_tray && other_mode {
            return Err("-t only applies to the desktop app".into());
        }
        if self.minimized && other_mode {
            return Err("--minimized only applies to the desktop app".into());
        }

        if self.imports {
            return Ok(Mode::Imports);
        }
        if let Some(id) = self.remove_import {
            return Ok(Mode::RemoveImport(id));
        }
        if self.trusted {
            return Ok(Mode::Trusted);
        }
        if let Some(id) = &self.untrust {
            return Ok(Mode::Untrust(id.clone()));
        }
        if self.new_id {
            return Ok(Mode::NewId);
        }
        Ok(if self.suggest.is_some() {
            Mode::Suggest
        } else if exporting {
            Mode::Export
        } else if has_query {
            Mode::Search
        } else if importing {
            Mode::Import
        } else if server {
            Mode::Server {
                background: self.background,
            }
        } else {
            Mode::Gui {
                with_server: self.with_server,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(s.split_whitespace())
    }

    #[test]
    fn query_with_exclusions_and_options_anywhere() {
        let a = p("search engine -yandex -l 10,20 -site:yandex.com -f json").unwrap();
        assert_eq!(
            a.query,
            vec!["search", "engine", "-yandex", "-site:yandex.com"]
        );
        assert_eq!(a.limit, Some(Limit::new(10, 20)));
        assert_eq!(a.format, Some(Format::Json));
        assert_eq!(a.mode(), Ok(Mode::Search));
    }

    #[test]
    fn double_dash_and_long_options() {
        let a = p("--limit=5 --output out.txt -- -l catfish").unwrap();
        assert_eq!(a.limit, Some(Limit::new(0, 5)));
        assert_eq!(a.output.as_deref(), Some("out.txt"));
        assert_eq!(a.query, vec!["-l", "catfish"]);
    }

    #[test]
    fn modes() {
        assert_eq!(p("").unwrap().mode(), Ok(Mode::Gui { with_server: false }));
        assert_eq!(
            p("-w -p 8080").unwrap().mode(),
            Ok(Mode::Gui { with_server: true })
        );
        assert_eq!(
            p("-s -a 0.0.0.0").unwrap().mode(),
            Ok(Mode::Server { background: false })
        );
        assert_eq!(
            p("-b").unwrap().mode(),
            Ok(Mode::Server { background: true })
        );
        assert_eq!(p("-d a.csv -d b.csv").unwrap().data.len(), 2);
        assert_eq!(p("-d a.csv").unwrap().mode(), Ok(Mode::Import));
        assert_eq!(p("-e out.lost").unwrap().mode(), Ok(Mode::Export));
        let a = p("-e out.lost site:example.com catfish --unsigned").unwrap();
        assert_eq!(a.mode(), Ok(Mode::Export));
        assert_eq!(a.query, vec!["site:example.com", "catfish"]);
        assert!(a.unsigned);
        assert!(p("-e out.lost -l 10 catfish").unwrap().mode().is_err());
        assert!(p("catfish --unsigned").unwrap().mode().is_err());
        assert_eq!(p("-i -").unwrap().mode(), Ok(Mode::Search));
        assert_eq!(p("-h catfish").unwrap().mode(), Ok(Mode::Help));
        let a = parse(["--suggest", "how to _"]).unwrap();
        assert_eq!(a.suggest.as_deref(), Some("how to _"));
        assert_eq!(a.mode(), Ok(Mode::Suggest));
        assert!(p("--suggest x catfish").unwrap().mode().is_err());
        assert!(p("--suggestion x").is_err());
        assert_eq!(p("--imports").unwrap().mode(), Ok(Mode::Imports));
        assert_eq!(p("--imports -f json").unwrap().format, Some(Format::Json));
        assert!(p("--imports -f json").unwrap().mode().is_ok());
        assert_eq!(
            p("--remove-import #4 --yes").unwrap().mode(),
            Ok(Mode::RemoveImport(4))
        );
        assert!(p("--remove-import four").is_err());
        assert!(p("--yes catfish").unwrap().mode().is_err());
        assert!(p("--imports --trusted").unwrap().mode().is_err());
        assert_eq!(
            parse(["--untrust=3f9a 1c2e"]).unwrap().mode(),
            Ok(Mode::Untrust("3f9a 1c2e".into()))
        );
        assert!(p("--trusted catfish").unwrap().mode().is_err());
        assert_eq!(p("--new-id").unwrap().mode(), Ok(Mode::NewId));
        assert!(matches!(
            p("--minimized").unwrap().mode(),
            Ok(Mode::Gui { .. })
        ));
        assert!(matches!(
            p("-w --minimized").unwrap().mode(),
            Ok(Mode::Gui { with_server: true })
        ));
        assert!(p("--minimized catfish").unwrap().mode().is_err());
        assert!(p("--minimized -s").unwrap().mode().is_err());
        assert_eq!(p("--new-id --yes").unwrap().mode(), Ok(Mode::NewId));
        assert!(p("--new-id --trusted").unwrap().mode().is_err());
        assert!(p("--new-id catfish").unwrap().mode().is_err());
    }

    #[test]
    fn invalid_combinations() {
        assert!(p("catfish -s").unwrap().mode().is_err());
        assert!(p("-d a.csv -e b.lost").unwrap().mode().is_err());
        assert!(p("-l 10").unwrap().mode().is_err());
        assert!(p("-p 80 catfish").unwrap().mode().is_err());
        assert!(p("-t -s").unwrap().mode().is_err());
        assert!(p("catfish -i q.txt").unwrap().mode().is_err());
    }

    #[test]
    fn invalid_options() {
        assert!(p("-x").is_err());
        assert!(p("--nope").is_err());
        assert!(p("-l").is_err());
        assert!(p("-l abc catfish").is_err());
        assert!(p("-p 0 -s").is_err());
        assert!(p("-f xml catfish").is_err());
        assert!(p("-l 1 -l 2 catfish").is_err());
        assert!(p("--server=yes").is_err());
    }
}
