//! Configuration (`lost.toml`) and platform-specific paths (ARCHITECTURE.md §6.1, §12).
//!
//! Precedence: command-line flag, then the user's `lost.toml`, then
//! `/etc/lost/lost.toml`, then built-in defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Result;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub import: ImportConfig,
    pub gui: GuiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub address: String,
    pub port: u16,
    pub max_results: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub default_limit: usize,
    /// A "Cached" link after each result's address, in the web UI and the
    /// app (never on the command line).
    pub cached_links: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImportConfig {
    /// "ask" | "signed-only" | "any": see [`Trust`].
    pub trust: String,
}

/// What to do with a `.lost` file that isn't signed by someone you trust
/// (`import.trust`, ARCHITECTURE.md §9.2). Files signed by someone you
/// trust, or by you, always import without a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Ask (the default). Where nobody can be asked, don't import.
    Ask,
    /// Don't import unsigned files; ask about the others.
    SignedOnly,
    /// Import anything without asking.
    Any,
}

impl Trust {
    pub fn parse(text: &str) -> Option<Trust> {
        match text {
            "ask" => Some(Trust::Ask),
            "signed-only" => Some(Trust::SignedOnly),
            "any" => Some(Trust::Any),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuiConfig {
    pub tray: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            address: crate::DEFAULT_ADDRESS.to_string(),
            port: crate::DEFAULT_PORT,
            max_results: 1000,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            default_limit: crate::DEFAULT_LIMIT,
            cached_links: false,
        }
    }
}

impl Default for ImportConfig {
    fn default() -> Self {
        ImportConfig {
            trust: "ask".to_string(),
        }
    }
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { tray: true }
    }
}

/// The per-user folder that holds both config and data:
/// `~/.lost` (Linux, BSD), `~/Library/Application Support/LOST` (macOS),
/// `%LOCALAPPDATA%\LOST` (Windows: `AppData\Local`, which roaming
/// profiles never copy to a server, unlike `AppData\Roaming`; the database
/// can be big, and the signing key shouldn't leave the computer).
pub fn user_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .filter(|dir| !dir.is_empty())
            .map(|dir| PathBuf::from(dir).join("LOST"))
    } else {
        let home = PathBuf::from(std::env::var_os("HOME").filter(|h| !h.is_empty())?);
        if cfg!(target_os = "macos") {
            Some(home.join("Library/Application Support/LOST"))
        } else {
            Some(home.join(".lost"))
        }
    }
}

/// The system-wide config file, on systems that have one.
pub fn system_config_path() -> Option<PathBuf> {
    cfg!(unix).then(|| PathBuf::from("/etc/lost/lost.toml"))
}

impl Config {
    /// Loads `/etc/lost/lost.toml` and then the user's `lost.toml` on top of it.
    /// Missing files are fine; malformed ones are an error.
    pub fn load() -> Result<Config> {
        let mut merged = toml::Table::new();
        let user = user_dir().map(|d| d.join("lost.toml"));
        for path in [system_config_path(), user].into_iter().flatten() {
            if let Some(table) = read_table(&path)? {
                merge_tables(&mut merged, table);
            }
        }
        let config = toml::Value::Table(merged)
            .try_into::<Config>()
            .map_err(|e| format!("invalid configuration: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks values serde can't: the ones from a fixed list.
    fn validate(&self) -> Result<()> {
        if Trust::parse(&self.import.trust).is_none() {
            return Err(format!(
                "invalid configuration: import.trust is \"{}\"; use \"ask\", \"signed-only\" or \"any\"",
                self.import.trust
            )
            .into());
        }
        Ok(())
    }

    /// `import.trust`.
    pub fn trust(&self) -> Trust {
        Trust::parse(&self.import.trust).unwrap_or(Trust::Ask)
    }

    /// Where the index and app database live.
    pub fn data_dir(&self) -> Result<PathBuf> {
        match &self.storage.data_dir {
            Some(dir) => Ok(dir.clone()),
            None => user_dir().ok_or_else(|| {
                "can't find your home folder; set storage.data_dir in lost.toml".into()
            }),
        }
    }
}

fn read_table(path: &Path) -> Result<Option<toml::Table>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("can't read {}: {e}", path.display()).into()),
    };
    text.parse::<toml::Table>()
        .map(Some)
        .map_err(|e| format!("invalid {}: {e}", path.display()).into())
}

/// Deep-merges `overlay` into `base`; values in `overlay` win.
fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge_tables(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

impl Config {
    /// The configuration as a lost.toml, every setting written out.
    pub fn to_toml(&self) -> String {
        let data_dir = match &self.storage.data_dir {
            Some(dir) => format!("data_dir = {}", toml_string(&dir.to_string_lossy())),
            None => "# data_dir = \"…\"   (the default: next to this file)".to_string(),
        };
        format!(
            "# LOST configuration. Every key is optional; command-line flags override them.\n\
             \n\
             [server]\n\
             # \"127.0.0.1\": this computer only; \"0.0.0.0\": the local network.\n\
             address = {}\n\
             port = {}\n\
             # Largest result count the Web API returns in one request.\n\
             max_results = {}\n\
             \n\
             [storage]\n\
             {}\n\
             \n\
             [search]\n\
             default_limit = {}\n\
             # A \"Cached\" link after each result's address (web UI and app).\n\
             cached_links = {}\n\
             \n\
             [import]\n\
             # .lost files from others, unless signed by someone you trust:\n\
             # \"ask\" first, \"signed-only\" (never unsigned ones; ask about\n\
             # the rest) or \"any\" (import everything without asking).\n\
             trust = {}\n\
             \n\
             [gui]\n\
             tray = {}\n",
            toml_string(&self.server.address),
            self.server.port,
            self.server.max_results,
            data_dir,
            self.search.default_limit,
            self.search.cached_links,
            toml_string(&self.import.trust),
            self.gui.tray,
        )
    }
}

/// A TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Checks that `text` is a usable lost.toml: valid TOML, known keys only.
pub fn check(text: &str) -> Result<()> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|e| format!("not valid TOML: {e}"))?;
    toml::Value::Table(table)
        .try_into::<Config>()
        .map_err(|e| format!("invalid configuration: {e}"))?
        .validate()
}

/// Saves the user's lost.toml (`path`), once `text` passes [`check`].
pub fn save_user_config(path: &Path, text: &str) -> Result<()> {
    check(text)?;
    if let Some(dir) = path.parent() {
        ensure_dir(dir)?;
    }
    // Written next to it, then moved over it: never half a file.
    let temp = path.with_extension("toml.new");
    std::fs::write(&temp, text)
        .and_then(|_| std::fs::rename(&temp, path))
        .map_err(|e| format!("can't save {}: {e}", path.display()).into())
}

/// Creates the data folder if needed, readable by its owner only.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| format!("can't create {}: {e}", dir.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_strings() {
        assert_eq!(toml_string("127.0.0.1"), "\"127.0.0.1\"");
        assert_eq!(toml_string(r#"C:\LOST "x""#), r#""C:\\LOST \"x\"""#);
        assert_eq!(toml_string("a\nb\u{1}"), "\"a\\nb\\u0001\"");
        let text = Config::default().to_toml();
        assert!(text.contains("port = 1057") && text.contains("tray = true"));
    }
}
