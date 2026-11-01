//! `dunk_context.py::Store` read side + `_parse_front`/`_render`/`_slug`/`_days`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

use super::ContextError;

pub const CLASSES: [&str; 4] = ["ground-truth", "fact", "provisional", "history"];
pub const AUTHORITIES: [&str; 3] = ["owner", "measured", "agent"];
/// `TYPES`.
pub const TYPES: [&str; 8] = [
    "principle",
    "decision",
    "reference",
    "lesson",
    "measurement",
    "incident",
    "rule",
    "wiki",
];
/// Types only the owner may write (docs/FLAT_FLEET.md 8): a rule is a standing instruction,
/// and standing instructions come from one person.
pub const OWNER_ONLY_TYPES: [&str; 2] = ["rule", "principle"];

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

// -- write side (slice 3) ---------------------------------------------------------------

use super::ledger::Ledger;

/// `date.today().isoformat()`.
fn today() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

fn io_error(path: &Path, error: std::io::Error) -> ContextError {
    ContextError(format!("{}: {error}", path.display()))
}

/// `add`'s keyword arguments. `authority` defaults to `agent` in the Python.
#[derive(Debug, Clone, Default)]
pub struct AddArgs<'a> {
    pub cls: &'a str,
    pub subject: &'a str,
    pub type_: &'a str,
    pub authority: Option<&'a str>,
    pub source: &'a str,
    pub stated: Option<&'a str>,
    pub recheck_after: Option<&'a str>,
    pub supersedes: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub by: Option<&'a str>,
}

/// `supersede`'s `**meta`: only the keys the caller passed override the old entry's.
#[derive(Debug, Clone, Default)]
pub struct SupersedeMeta<'a> {
    pub authority: Option<&'a str>,
    pub source: Option<&'a str>,
    /// `Some(None)` = passed as `None` (explicitly cleared), `None` = not passed.
    pub recheck_after: Option<Option<&'a str>>,
}

/// What `add` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub slug: String,
    pub path: PathBuf,
    pub cls: String,
}

impl Store {
    /// `_record`: log what the daemon just wrote, when a ledger is attached.
    fn record_write(
        &self,
        ledger: &mut Option<&mut Ledger>,
        path: &Path,
        slug: &str,
        meta: &Meta,
        by: Option<&str>,
    ) {
        if let Some(ledger) = ledger.as_deref_mut() {
            if let Ok(text) = std::fs::read_to_string(path) {
                let get = |key: &str| meta.get(key).and_then(|v| v.as_deref());
                let _ = ledger.record(&self.root, slug, &text, get("class"), get("authority"), by);
            }
        }
    }

    /// `adopt`: bootstrap, record every live entry as-is. The owner's one-shot.
    pub fn adopt(&self, ledger: &mut Ledger, by: &str) -> Vec<String> {
        let mut adopted = Vec::new();
        for entry in self.live_entries() {
            let Ok(text) = std::fs::read_to_string(&entry.path) else {
                continue;
            };
            let recorded = ledger.record(
                &self.root,
                &entry.slug,
                &text,
                entry.get("class"),
                entry.get("authority"),
                Some(by),
            );
            if recorded.is_ok() {
                adopted.push(entry.slug.clone());
            }
        }
        adopted
    }

    /// `ensure`: create the root, the four class directories and the README.
    pub fn ensure(&self) -> Result<&Path, ContextError> {
        std::fs::create_dir_all(&self.root).map_err(|e| io_error(&self.root, e))?;
        for name in CLASSES {
            let dir = self.root.join(name);
            if !dir.is_dir() {
                std::fs::create_dir_all(&dir).map_err(|e| io_error(&dir, e))?;
            }
        }
        let readme = self.root.join("README.md");
        if !readme.exists() {
            std::fs::write(
                &readme,
                "# .dunkingsheep context store\n\nDeprecated: see vp-of-agentry/docs/CONTEXT_STORE.md.\n",
            )
            .map_err(|e| io_error(&readme, e))?;
        }
        Ok(&self.root)
    }

    /// `add`. A ground truth has authority owner: an agent proposes a fact and the owner
    /// promotes it.
    pub fn add(
        &self,
        body: &str,
        args: &AddArgs,
        mut ledger: Option<&mut Ledger>,
    ) -> Result<Added, ContextError> {
        let authority = args.authority.unwrap_or("agent");
        if !CLASSES.contains(&args.cls) {
            return Err(ContextError(format!(
                "class must be one of {}",
                tuple_repr(&CLASSES)
            )));
        }
        if !AUTHORITIES.contains(&authority) {
            return Err(ContextError(format!(
                "authority must be one of {}",
                tuple_repr(&AUTHORITIES)
            )));
        }
        if !TYPES.contains(&args.type_) {
            return Err(ContextError(format!(
                "type must be one of {}; got {}",
                tuple_repr(&TYPES),
                py_repr(args.type_)
            )));
        }
        if args.cls == "ground-truth" && authority != "owner" {
            return Err(ContextError(
                "a ground truth has authority owner; an agent proposes a fact and the owner \
                 promotes it"
                    .to_string(),
            ));
        }
        self.ensure()?;
        let stated = args.stated.filter(|s| !s.is_empty()).map(str::to_string);
        let mut slug_text = match args.slug.filter(|s| !s.is_empty()) {
            Some(slug) => slug.to_string(),
            None => format!(
                "{}-{}",
                stated.clone().unwrap_or_else(today),
                slug(args.subject)
            ),
        };
        let folder = self.root.join(args.cls);
        std::fs::create_dir_all(&folder).map_err(|e| io_error(&folder, e))?;
        let mut path = folder.join(format!("{slug_text}.md"));
        let mut supersedes = args
            .supersedes
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if path.exists() {
            // A supersede on the same day derives the same slug, so the guarantee
            // "supersession is the only way an entry changes" was unimplementable within a
            // day: the new file collided with the old one. A superseding entry gets a
            // distinct, deterministic slug instead.
            if supersedes.is_none() {
                return Err(ContextError(format!("entry {slug_text} already exists")));
            }
            let mut counter = 2;
            while folder.join(format!("{slug_text}-{counter}.md")).exists() {
                counter += 1;
            }
            slug_text = format!("{slug_text}-{counter}");
            path = folder.join(format!("{slug_text}.md"));
        }
        if let Some(target) = supersedes.clone() {
            let Some(old) = self.resolve(&target)? else {
                return Err(ContextError(format!("supersede target {target} not found")));
            };
            supersedes = Some(old.slug.clone());
            let mut meta = old.meta.clone();
            meta.insert("superseded_by".into(), Some(slug_text.clone()));
            std::fs::write(&old.path, render(&meta, &old.body))
                .map_err(|e| io_error(&old.path, e))?;
            self.record_write(&mut ledger, &old.path, &old.slug, &meta, args.by);
        }
        let mut meta = Meta::new();
        let put = |meta: &mut Meta, key: &str, value: Option<String>| {
            meta.insert(key.to_string(), value);
        };
        put(&mut meta, "class", Some(args.cls.to_string()));
        put(&mut meta, "subject", Some(args.subject.to_string()));
        put(&mut meta, "type", Some(args.type_.to_string()));
        put(&mut meta, "scope", Some(self.scope.to_string()));
        put(&mut meta, "authority", Some(authority.to_string()));
        put(&mut meta, "stated", Some(stated.unwrap_or_else(today)));
        put(&mut meta, "source", Some(args.source.to_string()));
        put(
            &mut meta,
            "recheck_after",
            match args.recheck_after {
                Some(value) => Some(value.to_string()),
                None if args.cls == "provisional" => Some("7d".to_string()),
                None => None,
            },
        );
        put(&mut meta, "supersedes", supersedes);
        put(&mut meta, "superseded_by", None);
        std::fs::write(&path, render(&meta, body)).map_err(|e| io_error(&path, e))?;
        self.record_write(&mut ledger, &path, &slug_text, &meta, args.by);
        Ok(Added {
            slug: slug_text,
            path,
            cls: args.cls.to_string(),
        })
    }

    /// `supersede`: a new entry that replaces `slug`, keeping its class and subject.
    pub fn supersede(
        &self,
        slug_or_subject: &str,
        new_body: &str,
        by: Option<&str>,
        meta: &SupersedeMeta,
        ledger: Option<&mut Ledger>,
    ) -> Result<Added, ContextError> {
        let Some(old) = self.resolve(slug_or_subject)? else {
            return Err(ContextError(format!("entry {slug_or_subject} not found")));
        };
        let class = old.get("class").unwrap_or_default().to_string();
        let subject = old.get("subject").unwrap_or_default().to_string();
        let type_ = old
            .get("type")
            .filter(|t| !t.is_empty())
            .unwrap_or("reference")
            .to_string();
        let authority = meta
            .authority
            .map(str::to_string)
            .or_else(|| {
                old.get("authority")
                    .filter(|a| !a.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "agent".to_string());
        let source = meta
            .source
            .filter(|s| !s.is_empty())
            .or_else(|| old.get("source").filter(|s| !s.is_empty()))
            .unwrap_or("")
            .to_string();
        let recheck = match meta.recheck_after {
            Some(passed) => passed.map(str::to_string),
            None => old.get("recheck_after").map(str::to_string),
        };
        self.add(
            new_body,
            &AddArgs {
                cls: &class,
                subject: &subject,
                type_: &type_,
                authority: Some(&authority),
                source: &source,
                recheck_after: recheck.as_deref(),
                supersedes: Some(&old.slug),
                by,
                ..Default::default()
            },
            ledger,
        )
    }

    /// `retire`: mark an entry retired and append the reason to its body.
    pub fn retire(
        &self,
        slug_or_subject: &str,
        reason: &str,
        by: Option<&str>,
        mut ledger: Option<&mut Ledger>,
    ) -> Result<(String, String), ContextError> {
        let Some(entry) = self.resolve(slug_or_subject)? else {
            return Err(ContextError(format!("entry {slug_or_subject} not found")));
        };
        let mut meta = entry.meta.clone();
        let day = today();
        meta.insert("retired_at".into(), Some(day.clone()));
        meta.insert("retired_reason".into(), Some(reason.replace('\n', " ")));
        let body = format!("{}\n\n**Retired {day}**: {reason}\n", entry.body);
        std::fs::write(&entry.path, render(&meta, &body)).map_err(|e| io_error(&entry.path, e))?;
        self.record_write(&mut ledger, &entry.path, &entry.slug, &meta, by);
        Ok((entry.slug, reason.to_string()))
    }
}

// -- check / stats / entry_stats / digest / report (slice 4) ----------------------------

use serde_json::{json, Map, Value};

fn problem(slug: &str, text: &str) -> Value {
    json!({"slug": slug, "problem": text})
}

/// `{"slug", "warning": True, "problem"}` (the key order three call sites use).
fn warning(slug: &str, text: &str) -> Value {
    json!({"slug": slug, "warning": true, "problem": text})
}

/// Python `round()`: ties go to the even integer.
fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let diff = value - floor;
    let base = floor as i64;
    if (diff - 0.5).abs() < f64::EPSILON {
        if base % 2 == 0 {
            base
        } else {
            base + 1
        }
    } else {
        value.round() as i64
    }
}

/// `pct(values, q)` over a sorted list.
fn pct(values: &[i64], q: f64) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let last = values.len() as i64 - 1;
    let index = round_half_even(q * last as f64).min(last).max(0);
    json!(values[index as usize])
}

fn kind_of(record: &Value) -> &str {
    record.get("kind").and_then(Value::as_str).unwrap_or("read")
}

fn slug_of(record: &Value) -> Option<&str> {
    record.get("slug").and_then(Value::as_str)
}

fn at_of(record: &Value) -> f64 {
    record.get("at").and_then(Value::as_f64).unwrap_or(0.0)
}

impl Store {
    fn ledger_err(error: rusqlite::Error) -> ContextError {
        ContextError(error.to_string())
    }

    /// `check`: refusals (hard) and warnings over every entry.
    pub fn check(&self, ledger: &mut Ledger) -> Result<Value, ContextError> {
        use super::digest::parse_stated;
        let mut problems: Vec<Value> = Vec::new();
        let mut counts: BTreeMap<&str, i64> = CLASSES.iter().map(|c| (*c, 0)).collect();
        if ledger.count().map_err(Self::ledger_err)? == 0 {
            problems.push(warning(
                "-",
                "ledger is empty (bootstrap): every entry is trusted until `dunkingsheep \
                 context ledger adopt` runs",
            ));
        }
        let now = chrono::Local::now().naive_local();
        for entry in self.entries() {
            if let Some(error) = &entry.error {
                problems.push(problem(&entry.slug, error));
                continue;
            }
            let class = entry.get("class").unwrap_or_default();
            *counts.entry(class_static(class)).or_insert(0) += 1;
            let authority = entry.get("authority");
            let type_ = entry.get("type");
            let recheck = entry.get("recheck_after");
            if class == "ground-truth" && authority != Some("owner") {
                problems.push(problem(
                    &entry.slug,
                    "ground truth with authority != owner (refused)",
                ));
            }
            if class == "ground-truth"
                && !type_.is_some_and(|t| OWNER_ONLY_TYPES.contains(&t) || t == "decision")
            {
                let shown = type_.map(py_repr).unwrap_or_else(|| "None".to_string());
                problems.push(warning(
                    &entry.slug,
                    &format!("ground truth typed {shown}; expected rule/principle/decision"),
                ));
            }
            if class == "ground-truth"
                && recheck.is_none_or(str::is_empty)
                && !Store::is_retired(&entry)
            {
                problems.push(warning(
                    &entry.slug,
                    "ground truth has no recheck_after (rules go stale too)",
                ));
            }
            let text = fs::read_to_string(&entry.path).map_err(|e| io_error(&entry.path, e))?;
            match ledger
                .status(&self.root, &entry.slug, &text)
                .map_err(Self::ledger_err)?
                .as_str()
            {
                "unrecorded" => problems.push(problem(
                    &entry.slug,
                    "not written by the daemon (no ledger row); a ground truth here is NOT injected",
                )),
                "modified" => problems.push(problem(
                    &entry.slug,
                    "edited on disk after the daemon wrote it; NOT injected",
                )),
                _ => {}
            }
            if class == "fact" && recheck.is_none_or(str::is_empty) {
                problems.push(problem(&entry.slug, "fact has no recheck_after"));
            }
            if let Some(target) = entry.get("supersedes").filter(|s| !s.is_empty()) {
                if !self.root.join(class).join(format!("{target}.md")).exists() {
                    problems.push(problem(
                        &entry.slug,
                        &format!("supersedes missing entry {target}"),
                    ));
                }
            }
            if class != "ground-truth" {
                if let Some(count) = days(recheck).filter(|n| *n > 0) {
                    match entry.get("stated").and_then(parse_stated) {
                        Some(stated) => {
                            let horizon =
                                stated.checked_add_signed(chrono::Duration::days(count as i64));
                            if horizon.is_some_and(|h| now > h) {
                                problems.push(json!({
                                    "slug": entry.slug,
                                    "problem": format!(
                                        "stale: past recheck_after {}",
                                        recheck.unwrap_or_default()
                                    ),
                                    "warning": true,
                                }));
                            }
                        }
                        None => problems.push(problem(&entry.slug, "unparseable stated date")),
                    }
                }
            }
        }
        let is_warning = |p: &&Value| p.get("warning").is_some();
        let refusals: Vec<&Value> = problems.iter().filter(|p| !is_warning(p)).collect();
        let stale: Vec<&Value> = problems.iter().filter(is_warning).collect();
        let mut counts_json = Map::new();
        for class in CLASSES {
            counts_json.insert(class.to_string(), json!(counts[class]));
        }
        Ok(json!({
            "counts": counts_json,
            "problems": problems,
            "refusals": refusals,
            "stale": stale,
        }))
    }

    /// `stats`: per-class counts and ages, access totals, never-read and never-delivered.
    pub fn stats(&self) -> Result<Value, ContextError> {
        let entries = self.live_entries();
        let today = chrono::Local::now().date_naive();
        let mut counts = Map::new();
        let mut ages = Map::new();
        for class in CLASSES {
            let of_class: Vec<&Entry> = entries
                .iter()
                .filter(|e| e.get("class") == Some(class))
                .collect();
            counts.insert(class.to_string(), json!(of_class.len()));
            let mut days_old = Vec::new();
            for entry in of_class {
                let stated = entry.get("stated").unwrap_or_default();
                let date = chrono::NaiveDate::parse_from_str(stated, "%Y-%m-%d").map_err(|_| {
                    ContextError(format!("Invalid isoformat string: {}", py_repr(stated)))
                })?;
                days_old.push((today - date).num_days());
            }
            days_old.sort_unstable();
            ages.insert(
                class.to_string(),
                json!({"median": pct(&days_old, 0.5), "p90": pct(&days_old, 0.9)}),
            );
        }
        let reads = self.reads();
        let now = crate::clock::now_s();
        let window = now - 7.0 * 86400.0;
        let recent = reads
            .iter()
            .filter(|r| at_of(r) >= window && kind_of(r) == "read")
            .count();
        let injected = reads
            .iter()
            .filter(|r| at_of(r) >= window && kind_of(r) == "injected")
            .count();
        let read_count = reads.iter().filter(|r| kind_of(r) == "read").count();
        let seen: BTreeSet<Option<&str>> = reads
            .iter()
            .filter(|r| kind_of(r) == "read")
            .map(|r| slug_of(r))
            .collect();
        let delivered: BTreeSet<Option<&str>> = reads.iter().map(|r| slug_of(r)).collect();
        let mut never_read: Vec<&str> = entries
            .iter()
            .filter(|e| !seen.contains(&Some(e.slug.as_str())))
            .map(|e| e.slug.as_str())
            .collect();
        never_read.sort_unstable();
        let mut never_delivered: Vec<&str> = entries
            .iter()
            .filter(|e| !delivered.contains(&Some(e.slug.as_str())))
            .map(|e| e.slug.as_str())
            .collect();
        never_delivered.sort_unstable();
        Ok(json!({
            "counts": counts,
            "age_days": ages,
            "reads_7d": recent,
            "reads_total": reads.len(),
            "injected_7d": injected,
            "injected_total": reads.len() - read_count,
            "never_read": never_read,
            "never_read_count": never_read.len(),
            "never_delivered": never_delivered,
            "never_delivered_count": never_delivered.len(),
        }))
    }

    /// `entry_stats`: per-article access counts: what was delivered, what was opened, when.
    pub fn entry_stats(&self, ledger: &mut Ledger) -> Result<Vec<Value>, ContextError> {
        #[derive(Default)]
        struct Slot {
            reads: i64,
            injected: i64,
            last_read: Option<f64>,
            last_injected: Option<f64>,
        }
        let mut counters: BTreeMap<String, Slot> = BTreeMap::new();
        for record in self.reads() {
            let Some(slug) = slug_of(&record).filter(|s| !s.is_empty()) else {
                continue;
            };
            let slot = counters.entry(slug.to_string()).or_default();
            let injected = kind_of(&record) == "injected";
            let at = record
                .get("at")
                .and_then(Value::as_f64)
                .filter(|a| *a != 0.0);
            let (count, last) = if injected {
                (&mut slot.injected, &mut slot.last_injected)
            } else {
                (&mut slot.reads, &mut slot.last_read)
            };
            *count += 1;
            if let Some(at) = at {
                if last.is_none_or(|current| at > current) {
                    *last = Some(at);
                }
            }
        }
        let stale_slugs: BTreeSet<String> = self.check(ledger)?["stale"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("slug").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut rows = Vec::new();
        for entry in self.live_entries() {
            let empty = Slot::default();
            let slot = counters.get(&entry.slug).unwrap_or(&empty);
            rows.push(json!({
                "slug": entry.slug,
                "class": entry.get("class"),
                "subject": entry.get("subject"),
                "authority": entry.get("authority"),
                "stated": entry.get("stated"),
                "reads": slot.reads,
                "injected": slot.injected,
                "last_read": slot.last_read,
                "last_injected": slot.last_injected,
                "stale": stale_slugs.contains(&entry.slug),
            }));
        }
        Ok(rows)
    }

    /// `Store.digest`: the single-store digest (`digest_both` is the multi-store one).
    pub fn digest(&self, max_bytes: usize, subject: Option<&str>) -> String {
        use super::digest::{first_line, stale_mark};
        let entries = self.live_entries();
        let truths: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.get("class") == Some("ground-truth"))
            .collect();
        let others: Vec<&Entry> = entries
            .iter()
            .filter(|e| {
                e.get("class") != Some("ground-truth")
                    && (subject.is_none() || e.get("subject") == subject)
            })
            .collect();
        let mut included: Vec<&Entry> = truths.clone();
        let mut parts = vec![format!("## Ground truths ({})", truths.len())];
        if truths.is_empty() {
            parts.push("- none".to_string());
        }
        for entry in &truths {
            parts.push(format!(
                "- [{}] {}",
                entry.get("subject").unwrap_or("None"),
                first_line(&entry.body)
            ));
        }
        parts.push(String::new());
        parts.push("## Relevant".to_string());
        let mut used = parts.join("\n").len();
        let mut omitted = 0;
        let now = chrono::Local::now().naive_local();
        for entry in others {
            let line = format!(
                "- [{}/{}] {}",
                entry.get("class").unwrap_or("None"),
                entry.get("subject").unwrap_or("None"),
                first_line(&entry.body)
            );
            let size = line.len() + 1;
            let stale = stale_mark(entry, now);
            if used + size + stale.len() > max_bytes {
                omitted += 1;
                continue;
            }
            parts.push(format!("{line}{stale}"));
            included.push(entry);
            used += size + stale.len();
        }
        if omitted > 0 {
            parts.push(format!(
                "omitted: {omitted} entries over the {max_bytes}-byte cap"
            ));
        }
        let text = format!("{}\n", parts.join("\n"));
        for entry in included {
            self.log_access(&entry.slug, None, "injected");
        }
        text
    }
}

/// The `&'static str` of a validated class name (entries are validated at parse time).
fn class_static(class: &str) -> &'static str {
    CLASSES
        .iter()
        .copied()
        .find(|c| *c == class)
        .unwrap_or("fact")
}

/// `report`: stats/check/entry count over every selected store, always labelled.
pub fn report(project_path: Option<&Path>, which: Option<&str>) -> Result<Value, ContextError> {
    let mut out = Map::new();
    for (name, store) in super::digest::select_stores(project_path, which) {
        let mut ledger = Ledger::new(None);
        let check = store.check(&mut ledger)?;
        out.insert(
            name,
            json!({
                "path": store.root.display().to_string(),
                "stats": store.stats()?,
                "check": check,
                "entries": store.live_entries().len(),
            }),
        );
    }
    Ok(Value::Object(out))
}

// -- the disposable FTS index (slice 5) --------------------------------------------------

/// `INDEX_COLUMNS`: the columns `search()` reads. Files are truth, the FTS index is a
/// disposable cache, so a cache carrying anything else is disposable too.
pub const INDEX_COLUMNS: [&str; 5] = ["slug", "subject", "body", "class", "scope"];

/// `_PROBE`: the one query shape `search()` uses. Probing it is how a cache proves it can
/// ANSWER; column names alone do not (an ordinary table with the same five columns passes a
/// column check and then dies on MATCH, CoS review 2026-10-28).
const PROBE: &str = "SELECT slug FROM entries WHERE entries MATCH ? ORDER BY rank LIMIT 1";
/// A bare word on purpose: FTS5 parses the MATCH operand as a query, and a hyphen reads as
/// an operator.
const PROBE_TERM: &str = "probe";

/// `_FTS5_HEADER`: an FTS5 table is created by exactly this header.
fn fts5_header() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)^\s*CREATE\s+VIRTUAL\s+TABLE\s+["\[]?entries["\]]?\s+USING\s+fts5\s*\("#)
            .expect("static regex")
    })
}

fn open_readonly(path: &Path) -> Result<rusqlite::Connection, rusqlite::Error> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA query_only = ON")?;
    Ok(connection)
}

fn sql_error(error: rusqlite::Error) -> ContextError {
    ContextError(error.to_string())
}

impl Store {
    pub fn index_path(&self) -> PathBuf {
        self.root.join(".index.db")
    }

    /// `index_compatible`: true only when the cached index is an FTS5 table that can answer
    /// the search query this code runs. Three bounded controls (the fts5 creation header, the
    /// exact column tuple, one probe of the MATCH/rank shape), and any doubt rebuilds: a cache
    /// another schema built is disposable. Drift is ordinary (an older release, another lane's
    /// build, a half-written file) and the answer is always a rebuild, never an error.
    pub fn index_compatible(&self, path: &Path) -> bool {
        let check = || -> Result<bool, rusqlite::Error> {
            let connection = open_readonly(path)?;
            let sql: Option<Option<String>> = connection
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'entries'",
                    [],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })?;
            match sql.flatten() {
                Some(sql) if !sql.is_empty() && fts5_header().is_match(&sql) => {}
                _ => return Ok(false),
            }
            let mut statement = connection.prepare("PRAGMA table_info(entries)")?;
            let columns: Vec<String> = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<_, _>>()?;
            if columns != INDEX_COLUMNS {
                return Ok(false);
            }
            let mut probe = connection.prepare(PROBE)?;
            let _ = probe
                .query_map([PROBE_TERM], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(true)
        };
        check().unwrap_or(false)
    }

    /// `index`: rebuild the cache from the files. Returns `(indexed, path)`; a ground truth
    /// whose authority is not owner is refused by the indexer, not downgraded.
    pub fn index(&self) -> Result<(usize, PathBuf), ContextError> {
        self.ensure()?;
        let path = self.index_path();
        if path.exists() {
            fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
        }
        let connection = rusqlite::Connection::open(&path).map_err(sql_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql_error)?;
        let _: String = connection
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(sql_error)?;
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE entries USING fts5(slug, subject, body, \
                 class UNINDEXED, scope UNINDEXED)",
            )
            .map_err(sql_error)?;
        let entries = self.entries();
        for entry in &entries {
            if entry.error.is_some() {
                continue;
            }
            if entry.get("class") == Some("ground-truth") && entry.get("authority") != Some("owner")
            {
                continue;
            }
            connection
                .execute(
                    "INSERT INTO entries(slug, subject, body, class, scope) VALUES (?,?,?,?,?)",
                    rusqlite::params![
                        entry.slug,
                        entry.get("subject"),
                        entry.body,
                        entry.get("class"),
                        entry.get("scope"),
                    ],
                )
                .map_err(sql_error)?;
        }
        drop(connection);
        let indexed = entries.iter().filter(|e| e.error.is_none()).count();
        Ok((indexed, path))
    }

    /// `search`: rebuild first when the cache is missing or cannot answer, then MATCH.
    pub fn search(
        &self,
        query: &str,
        cls: Option<&str>,
        scope: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, ContextError> {
        let path = self.index_path();
        if !path.exists() || !self.index_compatible(&path) {
            self.index()?;
        }
        let connection = open_readonly(&path).map_err(sql_error)?;
        let mut sql =
            "SELECT slug, subject, class, scope FROM entries WHERE entries MATCH ?".to_string();
        let mut params: Vec<rusqlite::types::Value> = vec![query.to_string().into()];
        if let Some(cls) = cls.filter(|c| !c.is_empty()) {
            sql.push_str(" AND class = ?");
            params.push(cls.to_string().into());
        }
        if let Some(scope) = scope.filter(|s| !s.is_empty()) {
            sql.push_str(" AND scope = ?");
            params.push(scope.to_string().into());
        }
        sql.push_str(" ORDER BY rank LIMIT ?");
        params.push(limit.into());
        let run = || -> Result<Vec<[Option<String>; 4]>, rusqlite::Error> {
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
            })?;
            rows.collect()
        };
        let rows = run().map_err(|e| ContextError(format!("index search failed: {e}")))?;
        for row in &rows {
            if let Some(slug) = &row[0] {
                self.log_access(slug, None, "read");
            }
        }
        Ok(rows
            .into_iter()
            .map(|r| json!({"slug": r[0], "subject": r[1], "class": r[2], "scope": r[3]}))
            .collect())
    }
}
