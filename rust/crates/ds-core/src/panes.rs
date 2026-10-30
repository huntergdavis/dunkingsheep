//! Retired panes: the ones we deliberately stopped feeding (`dunk_panes.py`).
//!
//! A pane can be superseded (a replacement was started elsewhere) or replaced by
//! machinery (the dispatcher took over a PM's job). Its mailbox is then mail nobody
//! will ever read, and paging about it every four hours is an alarm that cannot be
//! satisfied - the failure mode this module exists to remove.
//!
//! Retiring is bookkeeping, not deletion: the mail stays as the record of what was
//! sent, the pane stops counting as idle or ready, and the fact that it is retired is
//! reported instead of silently vanishing.
//!
//! Two rules carried over from the Python, both of them earned:
//!
//! * a retirement needs a REASON - an unexplained silence is a bug, so `retire`
//!   refuses without one;
//! * a mailbox belongs to a retired pane "however it is spelled", because the table
//!   holds `name (w9:pH)`, `w9:pH` and plain names for the same thing, and a retire
//!   that only recorded labels left mailbox NAMES alerting (`crackdown-codex`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_json;
use crate::config;
use crate::error::DunkError;
use crate::lock;

/// The file this module owns, under the config directory (`DUNKINGSHEEP_DIR`).
pub fn default_path(env: &config::Env) -> Result<PathBuf, DunkError> {
    Ok(config::config_dir(env)?.join("retired-panes.json"))
}

/// One retired pane. Field order and names match the Python's dict, and the map is a
/// `BTreeMap` so `serde_json` writes sorted keys like `sort_keys=True` does.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Retired {
    pub pane_id: String,
    pub reason: String,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub mailboxes: Vec<String>,
    pub retired_at: f64,
}

/// Serialize with the keys in ALPHABETICAL order, because that is what the Python
/// writes: `json.dump(data, indent=2, sort_keys=True)` sorts at every level, and a
/// derived `Serialize` would use declaration order instead. Measured against the
/// Python's own bytes in `panes_tests::the_written_json_matches_the_pythons_bytes`.
impl Serialize for Retired {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(6))?;
        map.serialize_entry("by", &self.by)?;
        map.serialize_entry("labels", &self.labels)?;
        map.serialize_entry("mailboxes", &self.mailboxes)?;
        map.serialize_entry("pane_id", &self.pane_id)?;
        map.serialize_entry("reason", &self.reason)?;
        map.serialize_entry("retired_at", &self.retired_at)?;
        map.end()
    }
}

/// The store: pane id -> record, sorted.
pub type Records = BTreeMap<String, Retired>;

/// `_load`: a missing or unreadable file means nothing is retired, never an error.
///
/// PER-RECORD TOLERANCE (t1017): the Python keeps every well-formed record when ONE is
/// malformed, because its consumers skip non-dict values. Deserialising straight into
/// `Records` failed the whole parse on one bad record, and every retirement then stopped
/// protecting anything - 'retired' silently turned back into 'paging every four hours'
/// - in a file a human edits. So parse to raw values and convert record by record,
/// skipping (and logging) the bad ones. A wholly unparseable file still reads as empty.
pub fn load(path: &Path) -> Records {
    let raw: BTreeMap<String, serde_json::Value> =
        atomic_json::read_or_default(path).unwrap_or_default();
    let mut records = Records::new();
    for (key, value) in raw {
        match serde_json::from_value::<Retired>(value) {
            Ok(record) => {
                records.insert(key, record);
            }
            Err(error) => eprintln!("skipping malformed retired-pane record {key:?}: {error}"),
        }
    }
    records
}

/// `_write`: exclusive lock, then an atomic replace.
pub fn write(path: &Path, records: &Records) -> Result<(), DunkError> {
    // The Python's identity_guard gives up after 5 s with 'identity record is busy'; an
    // unbounded wait would hang the writer behind a wedged holder instead of refusing
    // (t980 finding 3).
    lock::with_exclusive_timeout(&lock_path(path), std::time::Duration::from_secs(5), || {
        atomic_json::write(path, records)
    })
}

fn lock_path(path: &Path) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(".lock");
    PathBuf::from(text)
}

/// Mark a pane retired. The reason is required: an unexplained silence is a bug.
pub fn retire(
    pane_id: &str,
    reason: &str,
    by: Option<&str>,
    labels: &[&str],
    mailboxes: &[&str],
    path: &Path,
    now: Option<f64>,
) -> Result<Retired, DunkError> {
    let pane_id = pane_id.trim();
    if pane_id.is_empty() {
        return Err(DunkError::Refused("a pane id is required".to_string()));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(DunkError::Refused(
            "a reason is required (e.g. superseded-by:wW:p7)".to_string(),
        ));
    }
    let mut records = load(path);
    let mut names: Vec<String> = labels
        .iter()
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty())
        .collect();
    names.sort();
    names.dedup();
    // MAILBOXES, not only labels: mail addressed to a dead pane is filed under a NAME
    // ('crackdown-codex'), which is not a pane id and not always the tab label.
    let mut boxes: Vec<String> = mailboxes
        .iter()
        .map(|box_name| box_name.trim().to_string())
        .filter(|box_name| !box_name.is_empty())
        .collect();
    boxes.sort();
    boxes.dedup();
    let record = Retired {
        pane_id: pane_id.to_string(),
        reason: reason.to_string(),
        by: by.map(|value| value.to_string()),
        labels: names,
        mailboxes: boxes,
        // The Python writes `now or time.time()`, and 0 is FALSY there: a caller that
        // passes 0 gets the clock, not a record that `valid_retirements` drops as
        // undated (a retirement that silently protects nothing - t980 finding 2).
        retired_at: now.filter(|value| *value > 0.0).unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs_f64())
                .unwrap_or(0.0)
        }),
    };
    records.insert(pane_id.to_string(), record.clone());
    write(path, &records)?;
    Ok(record)
}

/// Bring a pane back. Refuses when it was not retired, like the Python.
pub fn unretire(pane_id: &str, path: &Path) -> Result<Retired, DunkError> {
    let mut records = load(path);
    let removed = records
        .remove(pane_id)
        .ok_or_else(|| DunkError::Refused(format!("{pane_id} is not retired")))?;
    write(path, &records)?;
    Ok(removed)
}

/// Every retired record, ordered by pane id (`all_retired`).
pub fn all_retired(path: &Path) -> Vec<Retired> {
    load(path).into_values().collect()
}

/// Is this exact pane id retired? (`is_retired`).
pub fn is_retired(pane_id: &str, path: &Path) -> bool {
    load(path).contains_key(pane_id.trim())
}

/// The pane id inside a mailbox spelling, if there is one (`PANE_IN_MAILBOX`).
fn pane_in_mailbox(text: &str) -> Option<&str> {
    // [A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+ - found without a regex crate: scan for ":p"
    // and widen both ways over the allowed alphabet.
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b':' && bytes[index + 1] == b'p' {
            let mut start = index;
            while start > 0 && allowed(bytes[start - 1] as char) {
                start -= 1;
            }
            let mut end = index + 2;
            while end < bytes.len() && allowed(bytes[end] as char) {
                end += 1;
            }
            if start < index && end > index + 2 {
                return Some(&text[start..end]);
            }
        }
        index += 1;
    }
    None
}

/// Drop ANY trailing parenthetical: `re.sub(r"\s*\([^)]*\)\s*$", "", text)`.
///
/// This is `mailbox_retired`'s rule, and it is BROADER than the archive rule below on
/// purpose: a mailbox row can read `PM (par-a)`, where `par-a` is neither a pane id nor
/// a registered label, and the Python still treats `PM` as the mailbox name. The first
/// version of this port used the narrow rule for both, which diverged on 40 of 107 live
/// spellings (t980 finding 1) - always in the direction "Python suppresses the alert,
/// Rust keeps it".
fn plain_any_parenthetical(value: &str) -> String {
    let text = value.trim();
    if !text.ends_with(')') {
        return text.to_string();
    }
    match text.rfind('(') {
        Some(open) => text[..open].trim().to_string(),
        None => text.to_string(),
    }
}

/// `name (w9:pH)` -> `name`: drop a trailing parenthetical PANE ID.
///
/// This is `retired_mailboxes`' rule (`_ARCHIVE_SUFFIX`), which must stay strict: a
/// suffix that is not a pane id cannot be resolved against the retired set, so the
/// candidate is left unresolved rather than archived on a name match.
fn plain(value: &str) -> String {
    let text = value.trim();
    if let Some(open) = text.rfind('(') {
        if text.ends_with(')') {
            let inside = &text[open + 1..text.len() - 1];
            let trimmed = inside.trim();
            let is_pane = trimmed
                .strip_prefix('%')
                .map(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
                || pane_in_mailbox(trimmed)
                    .map(|found| found == trimmed)
                    .unwrap_or(false);
            if is_pane {
                return text[..open].trim().to_string();
            }
        }
    }
    text.to_string()
}

/// True when a mailbox belongs to a retired pane, however it is spelled
/// (`mailbox_retired`).
pub fn mailbox_retired(mailbox: &str, path: &Path) -> bool {
    let records = load(path);
    if records.is_empty() {
        return false;
    }
    let text = mailbox.trim();
    if records.contains_key(text) {
        return true;
    }
    if let Some(pane) = pane_in_mailbox(text) {
        if records.contains_key(pane) {
            return true;
        }
    }
    let bare = plain_any_parenthetical(text);
    records.values().any(|record| {
        record
            .labels
            .iter()
            .any(|label| label == text || *label == bare)
            || record
                .mailboxes
                .iter()
                .any(|box_name| box_name == text || *box_name == bare)
    })
}

/// A reasonless or undated record cannot withdraw identity protection
/// (`valid_retirements`).
pub fn valid_retirements(records: &[Retired]) -> Vec<Retired> {
    records
        .iter()
        .filter(|record| {
            !record.pane_id.trim().is_empty()
                && !record.reason.trim().is_empty()
                && record.retired_at.is_finite()
                && record.retired_at > 0.0
        })
        .cloned()
        .collect()
}

/// Every name a record answers to (`retirement_aliases`).
pub fn retirement_aliases(record: &Retired) -> Vec<String> {
    let mut aliases = vec![record.pane_id.clone()];
    for value in record.labels.iter().chain(record.mailboxes.iter()) {
        if !value.trim().is_empty() {
            aliases.push(value.clone());
        }
    }
    aliases
}

/// Exact database keys with positive retirement evidence, safe to age-archive
/// (`retired_mailboxes`).
///
/// Stricter than alert suppression: a contradictory pane suffix, a currently live
/// pane, a reused name or a quarantined recipient must remain unresolved. No
/// age/name-prefix inference can establish retirement.
pub fn retired_mailboxes(
    candidates: &[String],
    records: &[Retired],
    live_panes: &[String],
    protected: &[String],
) -> Vec<String> {
    let live: std::collections::HashSet<&String> = live_panes.iter().collect();
    let mut protected_set: std::collections::HashSet<String> = protected
        .iter()
        .map(|value| plain(value).to_lowercase())
        .collect();
    let mut retired: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in valid_retirements(records) {
        let pane = record.pane_id.clone();
        let aliases = retirement_aliases(&record);
        if live.contains(&pane) {
            protected_set.extend(
                aliases
                    .iter()
                    .filter(|name| !name.is_empty())
                    .map(|name| plain(name).to_lowercase()),
            );
            continue;
        }
        retired.insert(pane);
        names.extend(
            aliases
                .iter()
                .filter(|name| !name.is_empty())
                .map(|n| plain(n)),
        );
    }
    let mut selected: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for value in candidates {
        let text = value.trim();
        let name = plain(text);
        if text.is_empty()
            || text.starts_with("unrouted:")
            || protected_set.contains(&name.to_lowercase())
        {
            continue;
        }
        let suffix = {
            // `name (w9:pH)` -> the pane id in the suffix
            let trimmed = text.trim_end();
            if trimmed.ends_with(')') {
                trimmed.rfind('(').and_then(|open| {
                    let inside = trimmed[open + 1..trimmed.len() - 1].trim();
                    let is_pane = inside
                        .strip_prefix('%')
                        .map(|digits| {
                            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                        })
                        .unwrap_or(false)
                        || pane_in_mailbox(inside)
                            .map(|found| found == inside)
                            .unwrap_or(false);
                    if is_pane {
                        Some(inside.to_string())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        };
        let eligible = match suffix {
            Some(pane) => retired.contains(&pane),
            None => {
                let is_bare_pane = pane_in_mailbox(text)
                    .map(|found| found == text)
                    .unwrap_or(false)
                    || text
                        .strip_prefix('%')
                        .map(|digits| {
                            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
                        })
                        .unwrap_or(false);
                if is_bare_pane {
                    retired.contains(text)
                } else {
                    names.contains(text)
                }
            }
        };
        if eligible {
            // retain the exact stored key for the SQL update
            // The Python returns a SET: the same key twice is one archive target.
            selected.insert(value.clone());
        }
    }
    selected.into_iter().collect()
}

#[cfg(test)]
#[path = "panes_tests.rs"]
mod panes_tests;
