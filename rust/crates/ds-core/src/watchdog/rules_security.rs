//! T53 (t1400 slice F1): `security.secret_text` of `dunk_watchdog.py`, with the scan's inputs
//! (`_text_records`, `_secret_hits`).
//!
//! The scan covers inbox bodies, task titles/bodies and dunk texts (VP i19821). This is a deliberate
//! widening: the watchdog otherwise never reads message bodies, and it NEVER EMITS them: a finding
//! carries only `(kind, id, sha256 of the matching line)`, never the match.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::rules_dunks::load_dunks;
use super::{finding, py_number, TaskData, SOURCE_RULE};
use crate::error::DunkError;

/// `(kind, id, text)`: one piece of scanned content.
pub type Record = (String, String, String);

/// Python's `str.splitlines()`: splits on `\n`, `\r`, `\r\n`, `\v`, `\f`, `\x1c`-`\x1e`, `\x85`,
/// ` ` and ` `, and drops a trailing empty line. Rust's `str::lines` splits only on `\n`
/// and `\r\n`, which would let a credential hide behind a form feed.
pub fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        out.push(&text[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                end = j + 1;
            }
        }
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `_secret_hits`: the sha256 of every line a pattern matches, minus the allowlist.
fn secret_hits(text: &str, patterns: &[Regex], allow: &BTreeSet<String>) -> BTreeSet<String> {
    let mut hits = BTreeSet::new();
    for line in py_splitlines(text) {
        if patterns.iter().any(|p| p.is_match(line)) {
            let digest = format!("{:x}", Sha256::digest(line.as_bytes()));
            if !allow.contains(&digest) {
                hits.insert(digest);
            }
        }
    }
    hits
}

/// `_text_records`: inbox bodies in `[since, now]`, then every task's `title\nbody`, then dunk texts
/// sorted by id. A store that cannot be read is an `Err`, never a silent pass.
pub fn text_records(
    inbox_db: &Path,
    tasks_db: &Path,
    dunks_json: &Path,
    since: f64,
    now: f64,
) -> Result<Vec<Record>, DunkError> {
    let mut out: Vec<Record> = Vec::new();
    let conn = Connection::open_with_flags(inbox_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(crate::db::map_sqlite)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, body FROM messages WHERE created_at>=? AND created_at<=? \
             ORDER BY created_at, id",
        )
        .map_err(crate::db::map_sqlite)?;
    let rows = stmt
        .query_map([since, now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(crate::db::map_sqlite)?;
    for row in rows {
        let (id, body) = row.map_err(crate::db::map_sqlite)?;
        out.push(("inbox".into(), id, body.unwrap_or_default()));
    }
    for task in &TaskData::load(tasks_db, now)?.tasks {
        let field = |key: &str| match &task[key] {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            other => other.to_string(),
        };
        out.push((
            "task".into(),
            field("id"),
            format!("{}\n{}", field("title"), field("body")),
        ));
    }
    let mut dunks = load_dunks(dunks_json)?;
    dunks.sort_by_key(|d| d["id"].as_str().unwrap_or("").to_string());
    for dunk in &dunks {
        out.push((
            "dunk".into(),
            dunk["id"].as_str().unwrap_or("").to_string(),
            dunk["text"].as_str().unwrap_or("").to_string(),
        ));
    }
    Ok(out)
}

fn scan(
    records: &dyn Fn() -> Result<Vec<Record>, DunkError>,
    patterns: &[Regex],
    allow: &BTreeSet<String>,
) -> Result<BTreeSet<(String, String, String)>, DunkError> {
    let mut found = BTreeSet::new();
    for (kind, id, text) in records()? {
        for digest in secret_hits(&text, patterns, allow) {
            found.insert((kind.clone(), id.clone(), digest));
        }
    }
    Ok(found)
}

/// `security.secret_text`: credential-shaped lines, reported as hashes only.
///
/// `records` reads the content (see [`text_records`]) and is called TWICE when the first scan finds
/// something: the second, independent read must agree before anything can page (spec section 2).
pub fn rule_security_secret_text(
    records: &dyn Fn(f64) -> Result<Vec<Record>, DunkError>,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let patterns = rule["patterns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|p| {
            Regex::new(p).map_err(|e| DunkError::Refused(format!("bad secret pattern {p:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let allow: BTreeSet<String> = rule["allowlist_sha256"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let lookback = rule["lookback_hours"].as_f64().unwrap_or(0.0);
    let since = now - lookback * 3600.0;
    let first = scan(&|| records(since), &patterns, &allow)?;
    if first.is_empty() {
        return Ok(Vec::new());
    }
    // Re-verify on a second, independent read before anything can page (spec section 2).
    let second = scan(&|| records(since), &patterns, &allow)?;
    let mut out = Vec::new();
    for (kind, ident, digest) in first.intersection(&second) {
        out.push(finding(
            "security.secret_text",
            rule,
            &format!("{kind}:{ident}:{}", &digest[..16]),
            format!("a credential-shaped line in {kind} {ident}"),
            "1 confirmed match".into(),
            "1 match (re-verified)".into(),
            format!("{}h lookback (inbox)", py_number(&rule["lookback_hours"])),
            json!({"kind": kind, "id": ident, "sha256": digest}),
            SOURCE_RULE,
            false,
        ));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "rules_security_tests.rs"]
mod rules_security_tests;
