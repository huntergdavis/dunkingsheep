//! `dunk_context.py`: `select_stores`, `digest_both`, `_stale_mark` (T50 slice 2).
//!
//! `digest_both` takes the ledger and "now" as arguments (the Python builds a `Ledger()`
//! and reads `datetime.now()` inside), so the output is a pure function of the stores.

use std::path::Path;

use chrono::{Duration, NaiveDate, NaiveDateTime};

use super::ledger::Ledger;
use super::store::{days, Entry, Store};

pub const DEFAULT_CAP: usize = 8192;

/// `select_stores`: `global`, `project`, or both when a project store directory exists;
/// otherwise global only. Order is global then project, as the Python's dict.
pub fn select_stores(project_path: Option<&Path>, which: Option<&str>) -> Vec<(String, Store)> {
    let project = project_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ".dunkingsheep".into());
    let project = Store::new(Some(&project), false);
    match which {
        Some("global") => vec![("global".into(), Store::new(None, true))],
        Some("project") => vec![("project".into(), project)],
        _ if project.root.is_dir() => vec![
            ("global".into(), Store::new(None, true)),
            ("project".into(), project),
        ],
        _ => vec![("global".into(), Store::new(None, true))],
    }
}

/// Python `str.splitlines()[0]`: up to the first line boundary (more than `\n`).
pub fn first_line(text: &str) -> &str {
    let end = text
        .find(|c: char| {
            matches!(
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
            )
        })
        .unwrap_or(text.len());
    &text[..end]
}

/// `datetime.fromisoformat` for the shapes a `stated` field carries: a date, or a naive
/// datetime with `T` or a space. (A tz-aware value raises TypeError against the naive
/// `datetime.now()` in the Python; here it is treated as unparseable, i.e. not stale.)
fn parse_stated(text: &str) -> Option<NaiveDateTime> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0);
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(text, format) {
            return Some(value);
        }
    }
    None
}

/// `_stale_mark`: `" (STALE - re-check before trusting)"` past `recheck_after`, else `""`.
pub fn stale_mark(entry: &Entry, now: NaiveDateTime) -> &'static str {
    let Some(count) = days(entry.get("recheck_after")).filter(|n| *n > 0) else {
        return "";
    };
    let Some(stated) = entry.get("stated").and_then(parse_stated) else {
        return "";
    };
    let Some(horizon) = stated.checked_add_signed(Duration::days(count as i64)) else {
        return "";
    };
    if now > horizon {
        " (STALE - re-check before trusting)"
    } else {
        ""
    }
}

impl Store {
    /// `verified`: does the file on disk match what the daemon wrote? (bootstrap counts.)
    pub fn verified(&self, entry: &Entry, ledger: &mut Ledger) -> bool {
        let Ok(text) = std::fs::read_to_string(&entry.path) else {
            return false;
        };
        matches!(
            ledger
                .status(&self.root, &entry.slug, &text)
                .map(|s| s.as_str()),
            Ok("recorded") | Ok("bootstrap")
        )
    }
}

/// `digest_both`: the session digest. Global ground truths are ALWAYS included (a digest
/// scoped to the working directory would tell a fresh agent in a project there are zero
/// rules); project facts are selected by subject under the byte cap. With `record`, every
/// included entry is logged as `injected`.
pub fn digest_both(
    project_path: Option<&Path>,
    which: Option<&str>,
    max_bytes: usize,
    subject: Option<&str>,
    record: bool,
    ledger: &mut Ledger,
    now: NaiveDateTime,
) -> String {
    let stores = select_stores(project_path, which);
    let mut truths: Vec<(usize, Entry)> = Vec::new();
    let mut relevant: Vec<(usize, Entry)> = Vec::new();
    let mut unverified: Vec<Entry> = Vec::new();
    for (index, (_name, store)) in stores.iter().enumerate() {
        for entry in store.live_entries() {
            if entry.error.is_some() {
                continue;
            }
            if entry.get("class") == Some("ground-truth") {
                if store.verified(&entry, ledger) {
                    truths.push((index, entry));
                } else {
                    unverified.push(entry);
                }
            } else if subject.is_none() || entry.get("subject") == subject {
                relevant.push((index, entry));
            }
        }
    }
    let mut included: Vec<(usize, String)> =
        truths.iter().map(|(i, e)| (*i, e.slug.clone())).collect();
    let mut lines = vec![format!("## Ground truths ({})", truths.len())];
    if truths.is_empty() {
        lines.push("- none".to_string());
    }
    for (_i, e) in &truths {
        lines.push(format!(
            "- [{}] {}{}",
            e.get("subject").unwrap_or(""),
            first_line(&e.body),
            stale_mark(e, now)
        ));
    }
    if !unverified.is_empty() {
        let slugs: Vec<&str> = unverified.iter().map(|e| e.slug.as_str()).collect();
        lines.push(format!(
            "- ({} ground-truth file(s) not written by the daemon were NOT injected: {}; \
             see `dunkingsheep context check`)",
            unverified.len(),
            slugs.join(", ")
        ));
    }
    lines.push(String::new());
    lines.push("## Relevant".to_string());
    let mut used = lines.join("\n").len();
    let mut omitted = 0;
    for (index, entry) in &relevant {
        let line = format!(
            "- [{}/{}/{}] {}",
            stores[*index].0,
            entry.get("class").unwrap_or(""),
            entry.get("subject").unwrap_or(""),
            first_line(&entry.body)
        );
        let stale = if entry.get("class") != Some("ground-truth") {
            stale_mark(entry, now)
        } else {
            ""
        };
        let size = format!("{line}{stale}\n").len();
        if used + size > max_bytes {
            omitted += 1;
            continue;
        }
        lines.push(format!("{line}{stale}"));
        included.push((*index, entry.slug.clone()));
        used += size;
    }
    if omitted > 0 {
        lines.push(format!(
            "omitted: {omitted} entries over the {max_bytes}-byte cap"
        ));
    }
    let text = format!("{}\n", lines.join("\n"));
    if record {
        // A dashboard rendering the digest is a VIEW of what agents receive, not a
        // delivery to an agent: recording it would make injected_7d count polls.
        for (index, slug) in included {
            stores[index].1.log_access(&slug, None, "injected");
        }
    }
    text
}

#[cfg(test)]
#[path = "digest_tests.rs"]
mod digest_tests;
