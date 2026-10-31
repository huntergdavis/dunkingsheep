//! `dunk_context.py::Store` read side + `_parse_front`/`_render`/`_slug`/`_days`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use super::ContextError;

pub const CLASSES: [&str; 4] = ["ground-truth", "fact", "provisional", "history"];
pub const AUTHORITIES: [&str; 3] = ["owner", "measured", "agent"];

/// Front-matter keys in the order `_render` writes them.
const RENDER_KEYS: [&str; 12] = [
    "class",
    "subject",
    "type",
    "scope",
    "authority",
    "stated",
    "source",
    "recheck_after",
    "supersedes",
    "superseded_by",
    "retired_at",
    "retired_reason",
];

/// Python's `repr()` of a `str`, for the two error texts that quote one (`{line!r}`).
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn tuple_repr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|item| py_repr(item)).collect();
    format!("({})", inner.join(", "))
}

/// `_slug`: lowercase, runs of non `[a-z0-9]` become `-`, trimmed; `entry` when empty.
pub fn slug(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::new();
    let mut pending = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(ch);
        } else {
            pending = true;
        }
    }
    if out.is_empty() {
        "entry".to_string()
    } else {
        out
    }
}

/// `_days`: `"7d"` -> 7, anything else None. (ASCII digits only: the Python's `\d` also
/// accepts other Unicode digits; no store file has ever carried one.)
pub fn days(value: Option<&str>) -> Option<u64> {
    let text = value.unwrap_or("").trim();
    let digits = text.strip_suffix('d')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Front matter as parsed: values are `None` for `null`, `~` and empty.
pub type Meta = BTreeMap<String, Option<String>>;

/// `_parse_front`: `(meta, body)` or the named refusal.
pub fn parse_front(text: &str) -> Result<(Meta, String), ContextError> {
    if !text.starts_with("---") {
        return Err(ContextError(
            "entry has no front matter (must start with ---)".into(),
        ));
    }
    let parts: Vec<&str> = text.splitn(3, "---").collect();
    if parts.len() < 3 {
        return Err(ContextError(
            "entry front matter is not closed with ---".into(),
        ));
    }
    let front = parts[1];
    let body = parts[2].trim_start_matches('\n').to_string();
    let mut meta = Meta::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(ContextError(format!(
                "bad front-matter line: {}",
                py_repr(line)
            )));
        };
        let value = value.trim();
        let value = if matches!(value, "null" | "~" | "") {
            None
        } else {
            Some(value.to_string())
        };
        meta.insert(key.trim().to_string(), value);
    }
    for key in ["class", "subject", "type", "scope", "authority", "stated"] {
        if meta.get(key).and_then(|value| value.as_deref()).is_none() {
            return Err(ContextError(format!("front matter is missing {key}")));
        }
    }
    let class = meta["class"].as_deref().unwrap_or("");
    if !CLASSES.contains(&class) {
        return Err(ContextError(format!(
            "class must be one of {}",
            tuple_repr(&CLASSES)
        )));
    }
    let authority = meta["authority"].as_deref().unwrap_or("");
    if !AUTHORITIES.contains(&authority) {
        return Err(ContextError(format!(
            "authority must be one of {}",
            tuple_repr(&AUTHORITIES)
        )));
    }
    Ok((meta, body))
}

/// `_render`: the fixed key order, `null` for an absent value, body stripped plus `\n`.
pub fn render(meta: &Meta, body: &str) -> String {
    let mut lines = vec!["---".to_string()];
    for key in RENDER_KEYS {
        let value = meta
            .get(key)
            .and_then(|value| value.as_deref())
            .unwrap_or("null");
        lines.push(format!("{key}: {value}"));
    }
    lines.push("---".to_string());
    format!("{}\n\n{}\n", lines.join("\n"), body.trim())
}

/// One article, or the error entry a file that does not parse becomes
/// (`{"slug", "path", "error"}` in the Python).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub meta: Meta,
    pub slug: String,
    pub path: PathBuf,
    pub body: String,
    pub error: Option<String>,
}

impl Entry {
    /// `entry.get(key)`: the value when present and non-null.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.meta.get(key).and_then(|value| value.as_deref())
    }
}

/// `store_root`: an explicit path, else `.dunkingsheep` (project) or the global dir.
pub fn store_root(path: Option<&Path>, global_store: bool) -> PathBuf {
    let chosen = match path {
        Some(path) => path.to_path_buf(),
        None if global_store => global_dir(),
        None => PathBuf::from(".dunkingsheep"),
    };
    expand_user(&chosen)
}

/// `GLOBAL_DIR`: `$DUNKINGSHEEP_DIR/context`, default `~/.config/dunkingsheep/context`.
pub fn global_dir() -> PathBuf {
    let base = std::env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".config")
                .join("dunkingsheep")
        });
    base.join("context")
}

fn expand_user(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(text.trim_start_matches('~').trim_start_matches('/'));
        }
    }
    path.to_path_buf()
}

pub struct Store {
    pub root: PathBuf,
    pub scope: &'static str,
}

impl Store {
    pub fn new(path: Option<&Path>, global_store: bool) -> Self {
        Store {
            root: store_root(path, global_store),
            scope: if global_store { "global" } else { "project" },
        }
    }

    /// `exists`: a root with any class directory or a README. An absent store is not an
    /// empty one: '0 articles' for a path with no store is a zero where the truth is
    /// 'initialise it'.
    pub fn exists(&self) -> bool {
        if !self.root.is_dir() {
            return false;
        }
        CLASSES.iter().any(|name| self.root.join(name).is_dir())
            || self.root.join("README.md").is_file()
    }

    /// `entries`: every `*.md` under each class directory, class order then sorted path.
    /// A file that does not parse (or read) is an error entry, never a skipped one.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return out;
        }
        for name in CLASSES {
            let folder = self.root.join(name);
            if !folder.is_dir() {
                continue;
            }
            let mut files: Vec<PathBuf> = fs::read_dir(&folder)
                .map(|iter| {
                    iter.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            for file in files {
                let stem = file
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let parsed = fs::read_to_string(&file)
                    .map_err(|error| ContextError(error.to_string()))
                    .and_then(|text| parse_front(&text));
                out.push(match parsed {
                    Ok((meta, body)) => Entry {
                        meta,
                        slug: stem,
                        path: file,
                        body: body.trim().to_string(),
                        error: None,
                    },
                    Err(error) => Entry {
                        meta: Meta::new(),
                        slug: stem,
                        path: file,
                        body: String::new(),
                        error: Some(error.0),
                    },
                });
            }
        }
        out
    }

    /// `find`: the slug match first, then the subject match, over ALL entries.
    pub fn find(&self, subject_or_slug: &str) -> Option<Entry> {
        let entries = self.entries();
        entries
            .iter()
            .find(|e| e.slug == subject_or_slug)
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| e.get("subject") == Some(subject_or_slug))
            })
            .cloned()
    }

    /// `is_retired`: a `retired_at`, or the older dated body marker
    /// `**Retired YYYY-MM-DD**:` at the start of any line.
    pub fn is_retired(entry: &Entry) -> bool {
        if entry.get("retired_at").is_some() {
            return true;
        }
        Regex::new(r"(?m)^\*\*Retired \d{4}-\d{2}-\d{2}\*\*:")
            .expect("static pattern")
            .is_match(&entry.body)
    }

    /// `live_entries`: current heads only (entries()/find() keep the audit trail).
    pub fn live_entries(&self) -> Vec<Entry> {
        self.entries()
            .into_iter()
            .filter(|e| {
                e.error.is_none() && e.get("superseded_by").is_none() && !Self::is_retired(e)
            })
            .collect()
    }

    /// `resolve`: a subject or historical slug to its non-retired chain head. A cycle, a
    /// missing successor or a corrupt link in the chain is a refusal, never a guess.
    pub fn resolve(&self, subject_or_slug: &str) -> Result<Option<Entry>, ContextError> {
        let mut entry = self.find(subject_or_slug);
        let mut visited = BTreeSet::new();
        while let Some(current) = entry {
            if !visited.insert(current.slug.clone()) {
                return Err(ContextError(format!(
                    "supersession cycle at {}",
                    current.slug
                )));
            }
            if let Some(error) = &current.error {
                return Err(ContextError(error.clone()));
            }
            let Some(successor) = current.get("superseded_by").map(str::to_string) else {
                return Ok(if Self::is_retired(&current) {
                    None
                } else {
                    Some(current)
                });
            };
            entry = self.entries().into_iter().find(|e| e.slug == successor);
            if entry.is_none() {
                return Err(ContextError(format!(
                    "supersession target {successor} not found"
                )));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod store_tests;

// -- access log (slice 2) ---------------------------------------------------------------

/// Python `json.dumps(str)` (ensure_ascii): the quoted string with non-ASCII as `\uXXXX`.
fn py_json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buffer = [0u16; 2];
                for unit in c.encode_utf16(&mut buffer) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Store {
    pub fn access_path(&self) -> PathBuf {
        self.root.join("access.jsonl")
    }

    /// `log_access`: kind `read` when someone opened an article, `injected` when it was
    /// delivered. Delivery and reading are different facts and a store that conflates them
    /// cannot say whether it is unused or merely unread. The line is the Python's
    /// `json.dumps` of {slug, at, kind, by}, byte for byte (", " and ": " separators).
    pub fn log_access(&self, slug: &str, by: Option<&str>, kind: &str) {
        use std::io::Write;
        if slug.is_empty() {
            return;
        }
        let by = by
            .map(str::to_string)
            .or_else(|| {
                std::env::var("DUNKINGSHEEP_SELF")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let line = format!(
            "{{\"slug\": {}, \"at\": {:?}, \"kind\": {}, \"by\": {}}}\n",
            py_json_string(slug),
            // `{:?}` keeps the ".0" Python's float repr prints for a whole number
            crate::clock::now_s(),
            py_json_string(kind),
            py_json_string(&by)
        );
        if let Ok(mut handle) = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.access_path())
        {
            let _ = handle.write_all(line.as_bytes());
        }
    }

    /// `reads`: every access line; an unreadable or corrupt file is an empty list.
    pub fn reads(&self) -> Vec<serde_json::Value> {
        let Ok(text) = fs::read_to_string(self.access_path()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str(line) {
                Ok(value) => out.push(value),
                Err(_) => return Vec::new(),
            }
        }
        out
    }
}
