//! App database in redb (ARCHITECTURE.md §6.3), and the busy lock that keeps
//! imports and exports from running at the same time.
//!
//! redb allows only one process to open the file at a time, so it's opened
//! only for as long as an operation needs it.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::text::format_datetime;

const IMPORTS: TableDefinition<u64, &str> = TableDefinition::new("imports");
/// People whose signed `.lost` files import without a question: public key
/// (hex) → [`TrustRow`] as JSON.
const TRUST: TableDefinition<&str, &str> = TableDefinition::new("trust");

/// File name of the app database inside the data folder.
pub const DB_FILE: &str = "lost.redb";
/// File name of the busy lock inside the data folder.
pub const LOCK_FILE: &str = "busy.lock";

/// One row of the `imports` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportRow {
    pub id: u64,
    /// Unix seconds.
    pub started: i64,
    pub finished: Option<i64>,
    pub files: Vec<String>,
    /// BLAKE3 hash of each file, to recognize a file imported before.
    pub hashes: Vec<String>,
    /// Data source name, e.g. `hacker_news`.
    pub source: String,
    /// `running`, `complete` or `failed`. A `running` row with no import
    /// actually running is an interrupted import.
    pub status: String,
    /// Pages read from the files.
    pub records: u64,
    /// Pages for URLs that weren't in the database before.
    pub new: u64,
    /// Pages replaced by a fuller or newer version.
    pub updated: u64,
    /// Pages already in the database as they are (duplicates).
    pub unchanged: u64,
    /// Pages that came up more than once within this import.
    pub repeated: u64,
    /// Records skipped because their URL isn't usable.
    pub invalid: u64,
    /// Human-readable notes, e.g. how many items were skipped and why.
    pub notes: Vec<String>,
}

/// Someone whose signed `.lost` files import without a question.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustRow {
    /// Public key, hex.
    pub key: String,
    /// A name for them, if one was given.
    pub label: String,
    /// When they were trusted (Unix seconds).
    pub added: i64,
}

/// The app database.
pub struct Store {
    db: Database,
}

impl Store {
    /// Opens (or creates) `lost.redb` in the data folder.
    pub fn open(data_dir: &Path) -> Result<Store> {
        let path = data_dir.join(DB_FILE);
        let db = Database::create(&path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => {
                format!("{} is in use by another LOST process", path.display())
            }
            other => format!("can't open {}: {other}", path.display()),
        })?;
        Ok(Store { db })
    }

    /// Records the start of an import and returns its row (with its new id).
    pub fn begin_import(
        &self,
        files: Vec<String>,
        hashes: Vec<String>,
        source: &str,
    ) -> Result<ImportRow> {
        let txn = self.db.begin_write()?;
        let row = {
            let mut table = txn.open_table(IMPORTS)?;
            let id = match table.last()? {
                Some((key, _)) => key.value() + 1,
                None => 1,
            };
            let row = ImportRow {
                id,
                started: now(),
                finished: None,
                files,
                hashes,
                source: source.to_string(),
                status: "running".into(),
                ..ImportRow::default()
            };
            let json = serde_json::to_string(&row)?;
            table.insert(id, json.as_str())?;
            row
        };
        txn.commit()?;
        Ok(row)
    }

    /// Saves a changed row.
    pub fn update_import(&self, row: &ImportRow) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(IMPORTS)?;
            let json = serde_json::to_string(row)?;
            table.insert(row.id, json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes an import's row.
    pub fn remove_import(&self, id: u64) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(IMPORTS)?;
            table.remove(id)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Whether `key` (hex) is trusted.
    pub fn is_trusted(&self, key: &str) -> Result<bool> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(TRUST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(key)?.is_some())
    }

    /// Trusts `key` (hex) from now on.
    pub fn trust(&self, key: &str, label: &str) -> Result<()> {
        let row = TrustRow {
            key: key.to_string(),
            label: label.to_string(),
            added: now(),
        };
        let json = serde_json::to_string(&row)?;
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TRUST)?;
            table.insert(key, json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Stops trusting `key` (hex).
    pub fn untrust(&self, key: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(TRUST)?;
            table.remove(key)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Everyone trusted, longest-trusted first.
    pub fn trusted(&self) -> Result<Vec<TrustRow>> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(TRUST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut rows: Vec<TrustRow> = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            rows.push(serde_json::from_str(value.value())?);
        }
        rows.sort_by_key(|r| r.added);
        Ok(rows)
    }

    /// All imports, oldest first.
    pub fn imports(&self) -> Result<Vec<ImportRow>> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(IMPORTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut rows = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            rows.push(serde_json::from_str(value.value())?);
        }
        Ok(rows)
    }
}

/// Current time as Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Held while an import or export runs. Nothing ever waits for it: if another
/// process holds it, acquiring fails at once with a message saying what's
/// running (ARCHITECTURE.md §6.3). The OS releases the lock if the process dies.
pub struct BusyLock {
    file: File,
    path: PathBuf,
}

impl BusyLock {
    /// `operation` is e.g. `import of hn.csv`.
    pub fn acquire(data_dir: &Path, operation: &str) -> Result<BusyLock> {
        let path = data_dir.join(LOCK_FILE);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("can't open {}: {e}", path.display()))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // On Windows the holder's lock also prevents reading the description.
                let mut what = String::new();
                let _ = file.read_to_string(&mut what);
                let what = what.trim();
                return Err(if what.is_empty() {
                    "Database busy: another import or export is running".into()
                } else {
                    format!("Database busy: {what}").into()
                });
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("can't lock {}: {e}", path.display()).into());
            }
        }

        let description = format!(
            "{operation} in progress since {} (PID {})",
            format_datetime(now()),
            std::process::id()
        );
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(description.as_bytes())?;
        file.flush()?;
        Ok(BusyLock { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BusyLock {
    fn drop(&mut self) {
        // Clear the description; closing the file releases the lock.
        let _ = self.file.set_len(0);
    }
}
