//! `dunk_context.py::Ledger`: what the daemon wrote, the table the digest trusts
//! (FLAT_FLEET.md 8).
//!
//! Every lane runs as the same Unix user, so a file under `context/` proves nothing about
//! who wrote it. The daemon records (store, slug, sha256) for every entry it writes; the
//! digest injects a ground truth only when the file on disk still matches a row here, and
//! `check` names every file that does not. A lane can forge this table too; the ADR says
//! so. With no rows at all (before `context ledger adopt`) the ledger is in bootstrap:
//! nothing is refused, and `check` says to adopt.

use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::clock::now_s;

/// `_sha`: hex sha256 of the UTF-8 text.
pub fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `str(Path(...))`: the key the Python stores. `Path` drops `.` components, repeated and
/// trailing slashes, so a Rust `PathBuf` must be normalised the same way or a row written
/// by one implementation is "unrecorded" to the other.
pub fn path_key(path: &Path) -> String {
    let mut out = PathBuf::new();
    for component in path.components() {
        if component != Component::CurDir {
            out.push(component.as_os_str());
        }
    }
    if out.as_os_str().is_empty() {
        ".".to_string()
    } else {
        out.to_string_lossy().into_owned()
    }
}

/// `LEDGER_PATH`: `$DUNKINGSHEEP_DIR/context-ledger.db`, default under `~/.config/dunkingsheep`.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DUNKINGSHEEP_CONTEXT_LEDGER").filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let base = std::env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".config")
                .join("dunkingsheep")
        });
    base.join("context-ledger.db")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Recorded,
    Modified,
    Unrecorded,
    /// No rows at all.
    Bootstrap,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Recorded => "recorded",
            Status::Modified => "modified",
            Status::Unrecorded => "unrecorded",
            Status::Bootstrap => "bootstrap",
        }
    }
}

pub struct Ledger {
    pub path: PathBuf,
    db: Option<Connection>,
}

impl Ledger {
    pub fn new(path: Option<&Path>) -> Self {
        Ledger {
            path: path.map(Path::to_path_buf).unwrap_or_else(default_path),
            db: None,
        }
    }

    fn connect(&mut self) -> Result<&Connection, rusqlite::Error> {
        if self.db.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let connection = Connection::open(&self.path)?;
            connection.busy_timeout(std::time::Duration::from_secs(5))?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS entries (store TEXT NOT NULL, slug TEXT NOT NULL, \
                 sha256 TEXT NOT NULL, cls TEXT, authority TEXT, written_at REAL NOT NULL, \
                 by TEXT, PRIMARY KEY (store, slug))",
            )?;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
            self.db = Some(connection);
        }
        Ok(self.db.as_ref().expect("connected above"))
    }

    pub fn close(&mut self) {
        self.db = None;
    }

    /// `record`: INSERT OR REPLACE the (store, slug) row with the text's sha256.
    pub fn record(
        &mut self,
        store_root: &Path,
        slug: &str,
        text: &str,
        cls: Option<&str>,
        authority: Option<&str>,
        by: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let key = path_key(store_root);
        self.connect()?.execute(
            "INSERT OR REPLACE INTO entries (store, slug, sha256, cls, authority, written_at, by) \
             VALUES (?,?,?,?,?,?,?)",
            rusqlite::params![key, slug, sha(text), cls, authority, now_s(), by],
        )?;
        Ok(())
    }

    /// `status`: recorded | modified | unrecorded | bootstrap (no rows at all).
    pub fn status(
        &mut self,
        store_root: &Path,
        slug: &str,
        text: &str,
    ) -> Result<Status, rusqlite::Error> {
        let key = path_key(store_root);
        let db = self.connect()?;
        let any: Option<i64> = db
            .query_row("SELECT 1 FROM entries LIMIT 1", [], |row| row.get(0))
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
        if any.is_none() {
            return Ok(Status::Bootstrap);
        }
        let row: Option<String> = db
            .query_row(
                "SELECT sha256 FROM entries WHERE store = ? AND slug = ?",
                rusqlite::params![key, slug],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    Ok(None)
                } else {
                    Err(e)
                }
            })?;
        Ok(match row {
            None => Status::Unrecorded,
            Some(stored) if stored == sha(text) => Status::Recorded,
            Some(_) => Status::Modified,
        })
    }

    pub fn count(&mut self) -> Result<i64, rusqlite::Error> {
        self.connect()?
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
    }
}
