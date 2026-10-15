//! LOST — Local Offline Search Tool.
//!
//! An offline search engine for the web: the database lives on the device,
//! nothing is ever sent anywhere. This crate is the search library that the
//! CLI, web server and GUI are built on, and that other programs can embed.
//!
//! ```no_run
//! let lost = lost::Lost::open(std::path::Path::new("/path/to/data"))?;
//! let results = lost.search("catfish recipe", lost::query::Limit::new(0, 10))?;
//! for hit in results.results {
//!     println!("{} {}", hit.url, hit.title.unwrap_or_default());
//! }
//! # Ok::<(), lost::Error>(())
//! ```
//!
//! See ARCHITECTURE.md for the design.

pub mod config;
pub mod datasources;
pub mod engine;
pub mod lang;
pub mod lostfile;
pub mod query;
pub mod record;
pub mod store;
pub mod text;
pub mod urls;
pub mod warc;

#[cfg(feature = "cli")]
pub mod args;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "cli")]
pub mod instance;
#[cfg(feature = "cli")]
pub mod output;

#[cfg(all(feature = "server", unix))]
pub mod daemon;
#[cfg(feature = "server")]
pub mod html;
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "gui")]
pub mod quick;

use std::path::{Path, PathBuf};

pub use config::Trust;
pub use datasources::Declined;
pub use datasources::lost::{Answer, Question};
pub use engine::{CachedPage, Hit, SearchResults, Selection};
pub use lostfile::Identity;
pub use query::Limit;
pub use store::{ImportRow, TrustRow};

/// Errors carry a message meant for the user.
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type Result<T> = std::result::Result<T, Error>;

/// Default web server port: 1057, "LOST" in leetspeak.
pub const DEFAULT_PORT: u16 = 1057;

/// Default web server bind address.
pub const DEFAULT_ADDRESS: &str = "127.0.0.1";

/// Default number of results for CLI and Web API searches.
pub const DEFAULT_LIMIT: usize = 50;

/// File extension of LOST dataset files.
pub const DATASET_EXTENSION: &str = "lost";

/// Page text beyond this size is cut off at a word boundary (ARCHITECTURE.md §6.4).
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// What an export wrote.
#[derive(Debug, Clone)]
pub struct Export {
    pub pages: u64,
    /// Size of the file.
    pub bytes: u64,
    /// The ID it was signed with, if signed.
    pub signer: Option<String>,
}

/// A LOST database: the search index plus the app database next to it.
pub struct Lost {
    data_dir: PathBuf,
    engine: engine::Engine,
}

impl Lost {
    /// Opens the database in `data_dir`, creating it if needed.
    pub fn open(data_dir: &Path) -> Result<Lost> {
        config::ensure_dir(data_dir)?;
        Ok(Lost {
            data_dir: data_dir.to_path_buf(),
            engine: engine::Engine::open(data_dir)?,
        })
    }

    /// Number of pages in the database.
    pub fn document_count(&self) -> u64 {
        self.engine.num_docs()
    }

    /// Whether anything has been imported yet.
    pub fn is_empty(&self) -> bool {
        self.engine.num_docs() == 0
    }

    /// Searches (ARCHITECTURE.md §7).
    pub fn search(&self, query: &str, limit: Limit) -> Result<SearchResults> {
        self.engine.search(query, limit)
    }

    /// The stored copy of the page at `url` — the plain text LOST keeps,
    /// as `cache:URL` shows it (ARCHITECTURE.md §7.6) — with the words of
    /// `highlight` (the search it was opened from, if any) marked. A URL
    /// without a scheme is taken as `https://`. `None` if it isn't stored.
    pub fn page(&self, url: &str, highlight: Option<&str>) -> Result<Option<CachedPage>> {
        let url = url.trim();
        if url.contains("://") {
            self.engine.page(url, highlight)
        } else {
            self.engine.page(&format!("https://{url}"), highlight)
        }
    }

    /// Autocomplete suggestions for what's been typed so far (ARCHITECTURE.md §7.4).
    pub fn suggest(&self, input: &str, limit: usize) -> Result<Vec<String>> {
        self.engine.suggest(input, limit)
    }

    /// Imports dataset files or folders (ARCHITECTURE.md §8.2): one import per
    /// data source found. Fails at once if another import or export is running.
    ///
    /// `.lost` files signed by you or by someone you trust are imported; for
    /// the others `trust` decides, calling `ask` when the user is to decide
    /// (§9.2). If they say no to everything, the error is [`Declined`].
    pub fn import(
        &self,
        paths: &[PathBuf],
        trust: Trust,
        ask: &mut dyn FnMut(&Question) -> Answer,
        progress: &mut dyn FnMut(datasources::Progress),
    ) -> Result<Vec<ImportRow>> {
        datasources::import(&self.data_dir, &self.engine, paths, trust, ask, progress)
    }

    /// Exports the pages `selection` picks (all, a search's, some imports')
    /// as a `.lost` file
    /// (ARCHITECTURE.md §9.3), signed with `identity` if given. `progress`
    /// gets (pages written, pages in all). The file is written next to
    /// `path` and moved there when complete. Fails at once if an import or
    /// export is running.
    pub fn export(
        &self,
        path: &Path,
        selection: Selection,
        identity: Option<&Identity>,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<Export> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let _lock = store::BusyLock::acquire(&self.data_dir, &format!("export to {name}"))?;
        let partial = path.with_file_name(format!("{name}.part"));
        let file = std::fs::File::create(&partial)
            .map_err(|e| format!("can't write {}: {e}", partial.display()))?;
        // Removed unless the export completes.
        struct Cleanup<'a>(&'a Path, bool);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    let _ = std::fs::remove_file(self.0);
                }
            }
        }
        let mut cleanup = Cleanup(&partial, false);

        let mut writer = lostfile::Writer::new(
            std::io::BufWriter::with_capacity(1 << 20, file),
            match selection {
                Selection::Search(q) => Some(q),
                _ => None,
            },
            identity,
        )?;
        let mut last = std::time::Instant::now();
        let total = self.engine.for_each_doc(selection, &mut |doc, total| {
            writer.add(&lostfile::Page::from_doc(doc))?;
            let done = writer.pages();
            if done == total || last.elapsed() >= std::time::Duration::from_millis(100) {
                last = std::time::Instant::now();
                progress(done, total);
            }
            Ok(())
        })?;
        if total == 0 {
            return Err(match selection {
                Selection::Search(q) => {
                    format!("nothing found for \"{q}\"; nothing exported").into()
                }
                Selection::Imports(_) => {
                    "those imports have no pages left; nothing exported".into()
                }
                Selection::All => "the database is empty; nothing to export".into(),
            });
        }
        let (out, pages, bytes) = writer.finish()?;
        let file = out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&partial, path)
            .map_err(|e| format!("can't write {}: {e}", path.display()))?;
        cleanup.1 = true;
        Ok(Export {
            pages,
            bytes,
            signer: identity.map(Identity::id),
        })
    }

    /// The people whose signed `.lost` files import without a question.
    pub fn trusted(&self) -> Result<Vec<TrustRow>> {
        store::Store::open(&self.data_dir)?.trusted()
    }

    /// Gives you a new ID: a new signing key in place of the old one
    /// (ARCHITECTURE.md §9.2). The old ID is trusted from then on, so files
    /// you signed with it still import without a question. Returns the old
    /// ID, if there was one, and the new one.
    pub fn new_identity(&self) -> Result<(Option<String>, String)> {
        let dir =
            Identity::default_dir().ok_or("there's no home folder to keep your signing key in")?;
        let old = Identity::load(&dir)?;
        if let Some(old) = &old {
            store::Store::open(&self.data_dir)?
                .trust(&lostfile::to_hex(&old.public_key()), "your earlier ID")?;
        }
        let new = Identity::replace(&dir)?;
        Ok((old.map(|o| o.id()), new.id()))
    }

    /// Stops trusting `key` (hex, from [`TrustRow::key`]).
    pub fn untrust(&self, key: &str) -> Result<()> {
        store::Store::open(&self.data_dir)?.untrust(key)
    }

    /// Bytes the database takes on disk: the search index and the app
    /// database (the data folder's other files are tiny).
    pub fn disk_usage(&self) -> u64 {
        fn walk(path: &Path) -> u64 {
            match std::fs::symlink_metadata(path) {
                Ok(m) if m.is_dir() => std::fs::read_dir(path)
                    .map(|entries| entries.flatten().map(|e| walk(&e.path())).sum())
                    .unwrap_or(0),
                Ok(m) => m.len(),
                Err(_) => 0,
            }
        }
        walk(&self.data_dir.join(engine::INDEX_DIR)) + walk(&self.data_dir.join(store::DB_FILE))
    }

    /// How many pages would go if import `id` were removed.
    pub fn import_pages(&self, id: u64) -> Result<u64> {
        self.engine.count_import(id)
    }

    /// Removes import `id`: the pages whose current version came from it
    /// (including pages it updated: older versions aren't kept), and its row
    /// in the `imports` table. Returns how many pages were removed. Fails at
    /// once if an import or export is running.
    pub fn remove_import(&self, id: u64) -> Result<u64> {
        let _lock = store::BusyLock::acquire(&self.data_dir, &format!("removal of import #{id}"))?;
        let removed = self.engine.delete_import(id)?;
        store::Store::open(&self.data_dir)?.remove_import(id)?;
        Ok(removed)
    }

    /// The `imports` table, oldest first.
    pub fn imports(&self) -> Result<Vec<ImportRow>> {
        store::Store::open(&self.data_dir)?.imports()
    }
}
