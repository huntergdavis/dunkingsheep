//! The superuser allowlist (`dunk_superusers.py`, T31 prerequisite for t1273's real
//! `Allowlist`): pane ids and names allowed to perform destructive operations.
//!
//! File: `$DUNKINGSHEEP_DIR/superusers.json` (default `~/.config/dunkingsheep`):
//! `{"panes": [...], "names": [...]}`, a flat `{"superusers": [...]}` list (entries with
//! `:` are pane ids), or a roles file `{"lanes": {pane: {"role": "owner"}}}` (its owner
//! lanes are the panes, no names). The write is atomic and mode 0600.
//!
//! Locking (t423): writers take an EXCLUSIVE lock on the sidecar `<path>.lock` across
//! replacement; an authorization reader takes a SHARED lock across its re-read through its
//! mutation, so a revocation landing in between blocks instead of slipping through. Locks
//! release on drop, so a killed holder cannot wedge readers.
//!
//! DIFFERENCE from the Python: the config dir and the default VP/owner names are read when
//! used, not once at import. NOT ported here: the daemon's use of this store (T31+).

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::DunkError;
use crate::lock::{Guard, Mode};
use crate::tasks::advance::Allowlist;

fn refused<T>(text: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(text))
}

pub fn config_dir() -> PathBuf {
    match std::env::var("DUNKINGSHEEP_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/dunkingsheep"),
    }
}

pub fn superusers_file() -> PathBuf {
    config_dir().join("superusers.json")
}

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

/// `_classify`: entries containing ':' are pane ids, everything else a name.
fn classify(entry: &str) -> (&'static str, String) {
    let entry = entry.trim().to_string();
    if entry.contains(':') {
        ("pane", entry)
    } else {
        ("name", entry)
    }
}

/// Sidecar lock file beside the allowlist.
fn lock_path(path: &Path) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(".lock");
    PathBuf::from(text)
}

/// Python's `str(value)` for a decoded JSON entry.
fn py_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

type Sets = (BTreeSet<String>, BTreeSet<String>);

fn stripped(values: impl Iterator<Item = String>) -> BTreeSet<String> {
    values
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `_parse_allowlist`: `(panes, names)` from decoded JSON.
pub fn parse_allowlist(data: &Value) -> Result<Sets, DunkError> {
    let Some(object) = data.as_object() else {
        return refused("allowlist must contain a JSON object".into());
    };
    if let Some(lanes) = object.get("lanes").and_then(Value::as_object) {
        if !object.contains_key("panes") {
            let owners = lanes
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .as_object()
                        .is_some_and(|e| e.get("role").and_then(Value::as_str) == Some("owner"))
                })
                .map(|(pane, _)| pane.trim().to_string());
            return Ok((owners.collect(), BTreeSet::new()));
        }
    }
    let list = |key: &str| -> Vec<String> {
        match object.get(key) {
            Some(Value::Array(items)) => items.iter().map(py_text).collect(),
            _ => Vec::new(),
        }
    };
    let (mut panes, mut names) = (list("panes"), list("names"));
    if let Some(Value::Array(entries)) = object.get("superusers") {
        for entry in entries {
            // Python: `_classify(entry)` does `str(entry or "")`, so a falsy entry
            // (null, false, 0, "", [], {}) is the empty string and is dropped, NOT the name
            // "None"/"0"/"False" (review t1303: that was a fail-open for a pane labelled "None").
            let falsy = match entry {
                Value::Null => true,
                Value::Bool(b) => !b,
                Value::Number(n) => n.as_f64() == Some(0.0),
                Value::String(t) => t.is_empty(),
                Value::Array(a) => a.is_empty(),
                Value::Object(o) => o.is_empty(),
            };
            let (kind, value) = classify(&if falsy { String::new() } else { py_text(entry) });
            if kind == "pane" {
                panes.push(value);
            } else {
                names.push(value);
            }
        }
    }
    Ok((stripped(panes.into_iter()), stripped(names.into_iter())))
}

/// `match_allowlist`: the pure membership test shared by cached and freshly-read sets.
pub fn match_allowlist(
    panes: &BTreeSet<String>,
    names: &BTreeSet<String>,
    pane_id: &str,
    pane_info: Option<&Value>,
) -> bool {
    if pane_id.is_empty() {
        return false;
    }
    if panes.contains(pane_id.trim()) {
        return true;
    }
    let wanted: BTreeSet<String> = names
        .iter()
        .filter(|n| !n.trim().is_empty())
        .map(|n| n.trim().to_lowercase())
        .collect();
    if wanted.is_empty() {
        return false;
    }
    ["label", "tab_label", "workspace_label"]
        .iter()
        .any(|field| {
            let value = pane_info
                .and_then(|info| info.get(field))
                .filter(|v| !v.is_null() && py_text(v) != "")
                .map(|v| py_text(v).trim().to_lowercase())
                .unwrap_or_default();
            !value.is_empty() && wanted.contains(&value)
        })
}

fn read_sets(path: &Path) -> Result<Sets, DunkError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let data: Value = serde_json::from_str(&text)
                .map_err(|e| DunkError::Refused(format!("cannot read {}: {e}", path.display())))?;
            parse_allowlist(&data)
        }
        // No registry yet (bootstrap): nobody is an operator. Fail closed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((BTreeSet::new(), BTreeSet::new()))
        }
        Err(e) => refused(format!("cannot read {}: {e}", path.display())),
    }
}

/// Module-level `read_panes_locked(path)`: re-read from disk under a SHARED lock. Raises on
/// unreadable content: callers fail closed (a corrupt allowlist grants nothing).
pub fn read_panes_locked(path: &Path) -> Result<Sets, DunkError> {
    let _guard = Guard::acquire(&lock_path(path), Mode::Shared)?;
    read_sets(path)
}

/// `locked_allowlist(path)`: hold a SHARED lock across check-through-mutation. `body` gets
/// the sets read under the lock and keeps the lock until it returns.
pub fn locked_allowlist<T>(
    path: &Path,
    body: impl FnOnce(&Sets) -> Result<T, DunkError>,
) -> Result<T, DunkError> {
    let _guard = Guard::acquire(&lock_path(path), Mode::Shared)?;
    let sets = read_sets(path)?;
    body(&sets)
}

/// The file-backed [`Allowlist`] the advance transaction holds through its commit.
pub struct FileAllowlist(pub PathBuf);

impl Allowlist for FileAllowlist {
    fn with_locked(
        &self,
        body: &mut dyn FnMut(&dyn Fn(&str, Option<&Value>) -> bool) -> Result<(), DunkError>,
    ) -> Result<(), DunkError> {
        locked_allowlist(&self.0, |(panes, names)| {
            body(&|pane, info| match_allowlist(panes, names, pane, info))
        })
    }
}

/// `Superusers`: pane ids and names allowed to perform destructive operations.
#[derive(Debug)]
pub struct Superusers {
    pub path: PathBuf,
    pub panes: BTreeSet<String>,
    pub names: BTreeSet<String>,
    stamp: Option<(i64, i64, u64)>,
}

impl Superusers {
    pub fn new(path: impl Into<PathBuf>, create: bool) -> Result<Self, DunkError> {
        let mut store = Superusers {
            path: path.into(),
            panes: BTreeSet::new(),
            names: BTreeSet::new(),
            stamp: None,
        };
        if create {
            store.load(true)?;
        }
        Ok(store)
    }

    fn file_stamp(&self) -> Option<(i64, i64, u64)> {
        std::fs::metadata(&self.path)
            .ok()
            .map(|m| (m.mtime(), m.mtime_nsec(), m.len()))
    }

    /// Re-read the allowlist if the file changed under us (2026-10-24: a restart is not a
    /// way to learn the VP edited the file).
    pub fn refresh(&mut self) -> Result<bool, DunkError> {
        let stamp = self.file_stamp();
        if stamp == self.stamp {
            return Ok(false);
        }
        self.load(false)?;
        self.stamp = stamp;
        Ok(true)
    }

    pub fn load(&mut self, create: bool) -> Result<(), DunkError> {
        if !self.path.exists() {
            if create {
                self.seed_defaults()?;
            }
            return Ok(());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| DunkError::Refused(format!("cannot read {}: {e}", self.path.display())))?;
        let data: Value = serde_json::from_str(&text)
            .map_err(|e| DunkError::Refused(format!("cannot read {}: {e}", self.path.display())))?;
        let (panes, names) = parse_allowlist(&data)
            .map_err(|e| DunkError::Refused(format!("{}: {e}", self.path.display())))?;
        self.panes = panes;
        self.names = names;
        self.stamp = self.file_stamp();
        Ok(())
    }

    /// `(panes, names)` re-read from disk under a shared lock.
    pub fn read_panes_locked(&self) -> Result<Sets, DunkError> {
        let _guard = Guard::acquire(&lock_path(&self.path), Mode::Shared)?;
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let data: Value = serde_json::from_str(&text).map_err(|e| {
                    DunkError::Refused(format!("cannot read {}: {e}", self.path.display()))
                })?;
                parse_allowlist(&data)
            }
            // Unlike the module function, a missing file here is an error (Python opens it).
            Err(e) => refused(format!("cannot read {}: {e}", self.path.display())),
        }
    }

    /// First run: the VP name, the owner name, and any panes the setup set.
    pub fn seed_defaults(&mut self) -> Result<(), DunkError> {
        self.names = BTreeSet::from([
            env_or("DUNKINGSHEEP_VP_NAME", "VP of agentry"),
            env_or("DUNKINGSHEEP_OWNER_NAME", "owner"),
        ]);
        self.panes = BTreeSet::new();
        for variable in ["DUNKINGSHEEP_VP_PANE", "DUNKINGSHEEP_OWNER_PANE"] {
            let value = std::env::var(variable).unwrap_or_default();
            if !value.trim().is_empty() {
                self.panes.insert(value.trim().to_string());
            }
        }
        self.save()
    }

    /// Atomic write under an EXCLUSIVE lock, mode 0600. Key order is the Python's
    /// (`panes`, `names`, `updated_at`), indent 2.
    pub fn save(&self) -> Result<(), DunkError> {
        let directory = self
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&directory)?;
        let list = |set: &BTreeSet<String>| -> String {
            if set.is_empty() {
                "[]".to_string()
            } else {
                let items: Vec<String> = set
                    .iter()
                    .map(|s| format!("    {}", serde_json::to_string(s).expect("string")))
                    .collect();
                format!("[\n{}\n  ]", items.join(",\n"))
            }
        };
        let updated = (crate::clock::now_s() * 1000.0).round() / 1000.0;
        let payload = format!(
            "{{\n  \"panes\": {},\n  \"names\": {},\n  \"updated_at\": {:?}\n}}\n",
            list(&self.panes),
            list(&self.names),
            updated
        );
        let _guard = Guard::acquire(&lock_path(&self.path), Mode::Exclusive)?;
        let temporary = directory.join(format!(
            "superusers-{}-{}",
            std::process::id(),
            (crate::clock::now_s() * 1e6) as u64
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
            Ok(())
        })();
        if temporary.exists() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(DunkError::from)
    }

    pub fn entries(&self) -> Vec<String> {
        self.panes
            .iter()
            .chain(self.names.iter())
            .cloned()
            .collect()
    }

    pub fn empty(&self) -> bool {
        self.panes.is_empty() && self.names.is_empty()
    }

    pub fn is_superuser(&self, pane_id: &str, pane_info: Option<&Value>) -> bool {
        match_allowlist(&self.panes, &self.names, pane_id, pane_info)
    }

    pub fn describe(&self) -> Value {
        serde_json::json!({
            "path": self.path.display().to_string(),
            "panes": self.panes.iter().collect::<Vec<_>>(),
            "names": self.names.iter().collect::<Vec<_>>(),
            "empty": self.empty(),
        })
    }

    pub fn add(&mut self, entry: &str) -> Result<String, DunkError> {
        let (kind, value) = classify(entry);
        if value.is_empty() {
            return refused("a pane id or name is required".into());
        }
        if kind == "pane" {
            self.panes.insert(value.clone());
        } else {
            self.names.insert(value.clone());
        }
        self.save()?;
        Ok(value)
    }

    pub fn remove(&mut self, entry: &str) -> Result<bool, DunkError> {
        let (kind, value) = classify(entry);
        if value.is_empty() {
            return refused("a pane id or name is required".into());
        }
        let before = self.entries();
        if kind == "pane" {
            self.panes.remove(&value);
        } else {
            self.names.remove(&value);
        }
        self.save()?;
        Ok(before.contains(&value))
    }
}

#[cfg(test)]
#[path = "superusers_tests.rs"]
mod tests;
