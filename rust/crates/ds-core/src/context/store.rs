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
