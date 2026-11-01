//! T53 (t1400 slice C): the four inbox rules of `dunk_watchdog.py` (`inbox.undeliverable`,
//! `inbox.unread_live`, `inbox.unread_dead`, `inbox.ambiguous_sender`) and their inputs
//! (`Inputs.inbox`, `Inputs.name_to_pane`, `_mailbox_groups`, `_group_evidence`, `_queued`).
//!
//! `inbox.db` is opened read-only per query like the Python and every window is bounded above by
//! `now`. The pane inventory is passed in as `Option<&BTreeMap<pane_id, status>>`: `None` is the
//! pane source being disabled, which three of these rules treat differently on purpose.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use super::{finding, py_number, SOURCE_RULE};
use crate::error::DunkError;
use crate::panes::mailbox_retired;

/// Pane id -> `agent_status` (`unknown` when the pane reports none), as `Inputs.panes()` builds it.
pub type Panes = BTreeMap<String, String>;

/// A queued message: `(id, mailbox, created_at, pane_id)`.
pub type Queued = (String, String, f64, Option<String>);

/// `inbox.db`, opened read-only per query. An unreadable store is an error the evaluator turns into
/// a self error for the rules that needed it, never a silent pass.
pub struct InboxDb<'a> {
    pub path: &'a Path,
}

impl InboxDb<'_> {
    fn connect(&self) -> Result<Connection, DunkError> {
        Connection::open_with_flags(self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(crate::db::map_sqlite)
    }

    /// `(id, mailbox, created_at, reason)` of undeliverable mail created at or before `now`.
    fn undeliverable(
        &self,
        now: f64,
    ) -> Result<Vec<(String, String, f64, Option<String>)>, DunkError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, mailbox, created_at, reason FROM messages \
                 WHERE state='undeliverable' AND created_at<=? ORDER BY created_at, id",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = stmt
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)
    }

    /// `_queued`: queued mail created at or before `limit`.
    pub fn queued(&self, limit: f64) -> Result<Vec<Queued>, DunkError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, mailbox, created_at, pane_id FROM messages WHERE state='queued' \
                 AND created_at<=? ORDER BY created_at, id",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = stmt
            .query_map([limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)
    }

    /// `Inputs.name_to_pane`: mailbox name -> pane id from the inbox's own routing history (the
    /// latest message wins, ties broken by id). Read-only, no heuristics.
    pub fn name_to_pane(&self, now: f64) -> Result<BTreeMap<String, String>, DunkError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT mailbox, pane_id FROM messages WHERE pane_id IS NOT NULL AND created_at<=? \
                 ORDER BY created_at, id",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = stmt
            .query_map([now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(crate::db::map_sqlite)?;
        let mut mapping = BTreeMap::new();
        for row in rows {
            let (mailbox, pane) = row.map_err(crate::db::map_sqlite)?;
            mapping.insert(mailbox, pane);
        }
        Ok(mapping)
    }

    /// `(id, sender, created_at)` of mail with a sender inside `[since, until]`.
    fn senders(&self, since: f64, until: f64) -> Result<Vec<(String, String, f64)>, DunkError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, sender, created_at FROM messages WHERE sender IS NOT NULL \
                 AND created_at>=? AND created_at<=? ORDER BY created_at, id",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = stmt
            .query_map([since, until], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)
    }
}

struct Group {
    count: usize,
    ids: Vec<String>,
    reasons: BTreeMap<String, i64>,
    oldest: f64,
    newest: f64,
}

/// `_mailbox_groups`: keyed by mailbox (a `BTreeMap`, because every caller iterates `sorted(...)`).
fn mailbox_groups(rows: &[(String, String, f64, Option<String>)]) -> BTreeMap<String, Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (id, mailbox, created_at, reason) in rows {
        let group = groups.entry(mailbox.clone()).or_insert_with(|| Group {
            count: 0,
            ids: Vec::new(),
            reasons: BTreeMap::new(),
            oldest: *created_at,
            newest: *created_at,
        });
        group.count += 1;
        group.ids.push(id.clone());
        // `reason or "-"`: null and the empty string both count as "-".
        let key = reason
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "-".into());
        *group.reasons.entry(key).or_insert(0) += 1;
        group.oldest = group.oldest.min(*created_at);
        group.newest = group.newest.max(*created_at);
    }
    groups
}

/// `round(x, 1)` the way Python does it: the exact binary value, half to even.
fn round1(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(0.0)
}

fn group_evidence(mailbox: &str, group: &Group, now: f64) -> Value {
    let samples: Vec<&String> = group.ids.iter().take(3).collect();
    json!({
        "mailbox": mailbox,
        "sample_ids": samples,
        "reasons": group.reasons,
        "oldest_hours_ago": round1((now - group.oldest) / 3600.0),
        "newest_hours_ago": round1((now - group.newest) / 3600.0),
    })
}

fn cfg_f64(rule: &Value, key: &str) -> f64 {
    rule[key].as_f64().unwrap_or(0.0)
}

/// `inbox.undeliverable`: a backlog of undeliverable mail for one mailbox, old enough that the
/// 24h auto-archive did not catch it.
pub fn rule_inbox_undeliverable(
    inbox: &InboxDb,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let rows = inbox.undeliverable(now)?;
    let mut out = Vec::new();
    for (mailbox, group) in &mailbox_groups(&rows) {
        let age_hours = (now - group.oldest) / 3600.0;
        if group.count as f64 >= cfg_f64(rule, "min_messages")
            && age_hours >= cfg_f64(rule, "min_age_hours")
        {
            out.push(finding(
                "inbox.undeliverable",
                rule,
                mailbox,
                format!("undeliverable message backlog for {mailbox}"),
                format!(
                    "{} msgs older than {}h",
                    py_number(&rule["min_messages"]),
                    py_number(&rule["min_age_hours"])
                ),
                format!("{} msgs, oldest {age_hours:.1}h", group.count),
                "current backlog".into(),
                group_evidence(mailbox, group, now),
                SOURCE_RULE,
                false,
            ));
        }
    }
    Ok(out)
}

/// Queued rows as `(id, mailbox, created_at, None)` groups input, minus retired mailboxes.
fn live_rows(
    rows: impl Iterator<Item = Queued>,
    retired_path: &Path,
) -> Vec<(String, String, f64, Option<String>)> {
    rows.map(|(id, mailbox, at, _pane)| (id, mailbox, at, None))
        .filter(|(_, mailbox, _, _)| !mailbox_retired(mailbox, retired_path))
        .collect()
}

/// Unread mail in a lane that is IDLE: a message it never saw.
///
/// A lane that is working through a long job has a queue, not a miss - paging on it asks somebody to
/// interrupt the work to read mail that will be read when the job lands. So the lane's own state is
/// the first condition and the age is the second. With the pane source disabled (`panes` is `None`)
/// the state is unknown to us, and the rule keeps its old behaviour (page, and say liveness is
/// unknown) rather than silently going quiet.
pub fn rule_inbox_unread_live(
    inbox: &InboxDb,
    panes: Option<&Panes>,
    retired_path: &Path,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let idle: Vec<String> = rule["idle_statuses"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_lowercase))
                .collect()
        })
        .unwrap_or_default();
    let queued = inbox.queued(now - cfg_f64(rule, "age_minutes") * 60.0)?;
    let kept = queued.into_iter().filter(|(_, _, _, pane)| match panes {
        None => true,
        Some(panes) => pane
            .as_ref()
            .and_then(|p| panes.get(p))
            .is_some_and(|status| idle.contains(&status.to_lowercase())),
    });
    let rows = live_rows(kept, retired_path);
    let mut out = Vec::new();
    for (mailbox, group) in &mailbox_groups(&rows) {
        if group.count as f64 >= cfg_f64(rule, "min_messages") {
            let mut evidence = group_evidence(mailbox, group, now);
            let unknown = "unknown (pane source disabled)";
            evidence["pane_liveness"] = json!(if panes.is_none() { unknown } else { "live" });
            // `panes.get(mailbox) or "idle"`: the mailbox NAME is looked up as a pane id, as the
            // Python does, so this is usually "idle".
            evidence["pane_state"] = json!(match panes {
                None => unknown.to_string(),
                Some(panes) => panes
                    .get(mailbox)
                    .filter(|status| !status.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "idle".into()),
            });
            out.push(finding(
                "inbox.unread_live",
                rule,
                mailbox,
                format!("unread mail in {mailbox}"),
                format!(
                    "{} msgs older than {}min",
                    py_number(&rule["min_messages"]),
                    py_number(&rule["age_minutes"])
                ),
                format!("{} msgs", group.count),
                "current".into(),
                evidence,
                SOURCE_RULE,
                false,
            ));
        }
    }
    Ok(out)
}

/// Unread mail queued for a pane that no longer exists. ONLY MAIL THAT HAS HAD TIME TO BE READ: a
/// message delivered 30 seconds ago is not evidence its lane is dead; one nobody has touched for ten
/// minutes is.
///
/// With the pane source disabled the Python raises (`pane not in None` is a `TypeError`), which the
/// evaluator reports as a self error; here that is an `Err` with the same text, and only when there
/// is a message to judge.
pub fn rule_inbox_unread_dead(
    inbox: &InboxDb,
    panes: Option<&Panes>,
    retired_path: &Path,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let min_age = rule
        .get("min_age_minutes")
        .and_then(Value::as_f64)
        .unwrap_or(10.0);
    let mut dead = Vec::new();
    for row in inbox.queued(now - min_age * 60.0)? {
        let Some(pane) = row.3.as_ref().filter(|p| !p.is_empty()) else {
            continue;
        };
        let Some(panes) = panes else {
            return Err(DunkError::Refused(
                "argument of type 'NoneType' is not iterable".into(),
            ));
        };
        if !panes.contains_key(pane) {
            dead.push(row);
        }
    }
    let rows = live_rows(dead.into_iter(), retired_path);
    let mut out = Vec::new();
    for (mailbox, group) in &mailbox_groups(&rows) {
        if group.count as f64 >= cfg_f64(rule, "min_messages") {
            out.push(finding(
                "inbox.unread_dead",
                rule,
                mailbox,
                format!("unread mail queued for dead pane mailbox {mailbox}"),
                format!("{} msgs", py_number(&rule["min_messages"])),
                format!("{} msgs", group.count),
                "current".into(),
                group_evidence(mailbox, group, now),
                SOURCE_RULE,
                false,
            ));
        }
    }
    Ok(out)
}

/// Python's `re.match`: the pattern must match AT THE START of `text` (not anchored at the end).
fn matches_at_start(pattern: &Regex, text: &str) -> bool {
    pattern.find(text).is_some_and(|m| m.start() == 0)
}

/// `repr()` of a Python str: single quotes unless the text has a `'` and no `"`.
fn py_str_repr(text: &str) -> String {
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
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A sender that is a mailbox NAME resolving to a pane rather than a pane id: replies to it are
/// ambiguous. `pane_id_pattern` is `config["panes"]["pane_id_pattern"]`; `panes` is the live
/// inventory when `require_live_pane` is set (and `None` when the source is disabled).
pub fn rule_inbox_ambiguous_sender(
    inbox: &InboxDb,
    pane_id_pattern: &str,
    panes: Option<&Panes>,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let pattern = Regex::new(pane_id_pattern)
        .map_err(|error| DunkError::Refused(format!("bad pane_id_pattern: {error}")))?;
    let names: BTreeMap<String, String> = inbox
        .name_to_pane(now)?
        .into_iter()
        .filter(|(name, _)| !matches_at_start(&pattern, name))
        .collect();
    let panes = if rule["require_live_pane"].as_bool().unwrap_or(false) {
        panes
    } else {
        None
    };
    let lookback = cfg_f64(rule, "lookback_hours");
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, sender, _at) in inbox.senders(now - lookback * 3600.0, now)? {
        if matches_at_start(&pattern, &sender) || !names.contains_key(&sender) {
            continue;
        }
        if let Some(panes) = panes {
            if !panes.contains_key(&names[&sender]) {
                continue;
            }
        }
        groups.entry(sender).or_default().push(id);
    }
    let samples = rule["sample_ids"].as_u64().unwrap_or(0) as usize;
    let mut out = Vec::new();
    for (sender, ids) in &groups {
        let pane = &names[sender];
        out.push(finding(
            "inbox.ambiguous_sender",
            rule,
            sender,
            format!(
                "sender {} is a mailbox name resolving to pane {pane}, not a pane id",
                py_str_repr(sender)
            ),
            "1 msg".into(),
            format!("{} msgs", ids.len()),
            format!("{}h lookback", py_number(&rule["lookback_hours"])),
            json!({
                "sender": sender,
                "resolves_to_pane": pane,
                "sample_ids": ids.iter().take(samples).collect::<Vec<_>>(),
                "pane_liveness": if panes.is_none() { "unchecked" } else { "live" },
            }),
            SOURCE_RULE,
            false,
        ));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "rules_inbox_tests.rs"]
mod rules_inbox_tests;
