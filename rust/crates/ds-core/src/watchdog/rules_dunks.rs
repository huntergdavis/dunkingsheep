//! T53 (t1400 slice B): the two dunk rules of `dunk_watchdog.py` and their inputs
//! (`Inputs.dunks`, the `metrics.db` dunk events, `_retired_panes`).
//!
//! The Python rules read `inputs.metrics(sql)`; here the events come through [`DunkEvents`] so a
//! test can fake them and the evaluator (a later slice) wires the read-only `metrics.db` one. Every
//! window is bounded above by `now`, so evaluating at a past `now` never sees the future.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use super::{finding, minutes, py_float, py_number, SOURCE_RULE};
use crate::error::DunkError;
use crate::panes::{all_retired, Retired};

/// One `dunk.sent` / `dunk.skipped` row of `metrics.db`.
#[derive(Debug, Clone, PartialEq)]
pub struct DunkEvent {
    pub id: i64,
    pub at: f64,
    pub kind: String,
}

/// The dunk events of one dunk inside `[since, until]`, ordered by `(at, id)`.
pub trait DunkEvents {
    fn dunk_events(
        &self,
        dunk_id: &str,
        since: f64,
        until: f64,
    ) -> Result<Vec<DunkEvent>, DunkError>;
}

/// `metrics.db`, opened read-only per query like `Inputs.metrics`. An unreadable store is an error
/// the evaluator turns into a self error for the rules that needed it, never a silent pass.
pub struct MetricsDb<'a> {
    pub path: &'a Path,
}

impl DunkEvents for MetricsDb<'_> {
    fn dunk_events(
        &self,
        dunk_id: &str,
        since: f64,
        until: f64,
    ) -> Result<Vec<DunkEvent>, DunkError> {
        let conn = Connection::open_with_flags(self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(crate::db::map_sqlite)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, at, kind FROM events WHERE source=? AND kind IN \
                 ('dunk.sent','dunk.skipped') AND at>=? AND at<=? ORDER BY at, id",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = stmt
            .query_map(rusqlite::params![dunk_id, since, until], |row| {
                Ok(DunkEvent {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    kind: row.get(2)?,
                })
            })
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)
    }
}

/// `Inputs.dunks()`: the `dunkers` list of `dunks.json` (`[]` when the key is absent). A missing or
/// malformed file is an error, as in Python (`json.load` raises).
pub fn load_dunks(path: &Path) -> Result<Vec<Value>, DunkError> {
    let text = std::fs::read_to_string(path)?;
    let parsed: Value = serde_json::from_str(&text)?;
    Ok(parsed
        .get("dunkers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// `_retired_panes`: `{pane_id: record}` from `retired-panes.json`.
pub fn retired_panes(path: &Path) -> BTreeMap<String, Retired> {
    all_retired(path)
        .into_iter()
        .map(|record| (record.pane_id.clone(), record))
        .collect()
}

/// Python's `round(x)` (half to even) as an integer.
fn round_half_even(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// `round(x, ndigits)` the way Python does it: the exact binary value, half to even.
fn round_to(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(0.0)
}

fn dunk_id(dunk: &Value) -> String {
    dunk["id"].as_str().unwrap_or("").to_string()
}

fn sorted_dunks(dunks: &[Value]) -> Vec<&Value> {
    let mut sorted: Vec<&Value> = dunks.iter().collect();
    sorted.sort_by_key(|dunk| dunk_id(dunk));
    sorted
}

fn cfg_f64(rule: &Value, key: &str) -> f64 {
    rule[key].as_f64().unwrap_or(0.0)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `str(x)` of a JSON value the way an f-string prints it (`None` for null).
fn py_text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => py_number(other),
    }
}

/// A dunk that is still skipping. Not one that skipped for a while and stopped.
///
/// d52 paged with 33 consecutive skips and a cumulative 52 - but its last skip was 76 hours old and
/// its target run had ended days before. The streak was real and the alarm was not: a condition that
/// has stopped is not an incident. And a cumulative counter that only grows fires on its own
/// history, so the count is now the CURRENT run (it resets when the dunk sends) with the lifetime
/// figure reporting separately.
pub fn rule_dunk_skip_loop(
    dunks: &[Value],
    events: &dyn DunkEvents,
    retired: &BTreeMap<String, Retired>,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let since = now - cfg_f64(rule, "lookback_hours") * 3600.0;
    let fresh_s = cfg_f64(rule, "fresh_minutes") * 60.0;
    let needed = cfg_f64(rule, "consecutive_skips");
    let mut out = Vec::new();
    for dunk in sorted_dunks(dunks) {
        let id = dunk_id(dunk);
        let rows = events.dunk_events(&id, since, now)?;
        let (mut run, mut best) = (0i64, 0i64);
        let mut run_start: Option<(i64, f64)> = None;
        let (mut best_start, mut best_end): (Option<(i64, f64)>, Option<(i64, f64)>) = (None, None);
        for event in &rows {
            if event.kind == "dunk.skipped" {
                run += 1;
                if run == 1 {
                    run_start = Some((event.id, event.at));
                }
                if run > best {
                    best = run;
                    best_start = run_start;
                    best_end = Some((event.id, event.at));
                }
            } else {
                run = 0;
                run_start = None;
            }
        }
        if (best as f64) < needed {
            continue;
        }
        let target = dunk.get("target_pane_id").and_then(Value::as_str);
        if let Some(record) = target
            .filter(|t| !t.is_empty())
            .and_then(|t| retired.get(t))
        {
            // Not a loop: a dunk aimed at a pane we deliberately retired.
            let target = target.unwrap_or_default();
            let mut item = finding(
                "dunk.skip_loop",
                rule,
                &format!("{id}:obsolete"),
                format!(
                    "dunk {id} targets retired pane {target} ({}) - its streak is not a condition",
                    record.reason
                ),
                "not retired".into(),
                "retired".into(),
                "current".into(),
                json!({"dunk": id, "target_pane": target, "retired_reason": record.reason}),
                SOURCE_RULE,
                true,
            );
            item["severity"] = json!("info");
            out.push(item);
            continue;
        }
        let (Some(start), Some(end)) = (best_start, best_end) else {
            continue;
        };
        if now - end.1 > fresh_s {
            // The streak stopped; say nothing. A page here would describe the past.
            continue;
        }
        // Keyed by the run's FIRST skip, which stays fixed while the run grows, so a continuing
        // loop is one incident under its cooldown, not one per skip.
        let skip_count = dunk.get("skip_count").cloned().unwrap_or(Value::Null);
        let lifetime = dunk
            .get("total_skips")
            .cloned()
            .unwrap_or_else(|| skip_count.clone());
        out.push(finding(
            "dunk.skip_loop",
            rule,
            &format!("{id}:{}", start.0),
            format!(
                "dunk {id} skipped {best} times in a row (last {} min ago)",
                round_half_even((now - end.1) / 60.0)
            ),
            format!("{} consecutive skips", py_number(&rule["consecutive_skips"])),
            format!("{best} consecutive skips"),
            format!("last skip within {}min", py_number(&rule["fresh_minutes"])),
            json!({
                "dunk": id,
                "target_pane": dunk.get("target_pane_id").cloned().unwrap_or(Value::Null),
                "current_streak": run,
                "run_start_hours_ago": round_to((now - start.1) / 3600.0, 1),
                "run_end_hours_ago": round_to((now - end.1) / 3600.0, 1),
                "skips_this_run": skip_count,
                "cumulative_skip_count_lifetime": lifetime,
                "skip_if_unconsumed": dunk.get("skip_if_unconsumed").cloned().unwrap_or(Value::Null),
            }),
            SOURCE_RULE,
            false,
        ));
    }
    Ok(out)
}

/// A running dunk whose next send is missing, or is overdue by `missed_intervals` intervals.
pub fn rule_dunk_overdue(dunks: &[Value], rule: &Value, now: f64) -> Vec<Value> {
    let mut out = Vec::new();
    for dunk in sorted_dunks(dunks) {
        if !truthy(dunk.get("running").unwrap_or(&Value::Null)) {
            continue;
        }
        let id = dunk_id(dunk);
        let interval_minutes = dunk.get("interval_minutes").unwrap_or(&Value::Null);
        // `float(dunk.get("interval_minutes") or 0) * 60`
        let interval = interval_minutes.as_f64().unwrap_or(0.0) * 60.0;
        let due = dunk.get("next_send_at").and_then(Value::as_f64);
        match due {
            None => out.push(finding(
                "dunk.overdue",
                rule,
                &id,
                format!("dunk {id} is running with no scheduled send"),
                "a scheduled send".into(),
                "none".into(),
                "current".into(),
                json!({"dunk": id}),
                SOURCE_RULE,
                false,
            )),
            Some(due)
                if interval > 0.0 && now - due >= cfg_f64(rule, "missed_intervals") * interval =>
            {
                out.push(finding(
                    "dunk.overdue",
                    rule,
                    &id,
                    format!("dunk {id} has not fired past its scheduled send"),
                    format!("{} intervals", py_number(&rule["missed_intervals"])),
                    format!(
                        "{} intervals",
                        py_float(round_to((now - due) / interval, 2))
                    ),
                    format!("interval {}min", py_text(interval_minutes)),
                    json!({"dunk": id, "overdue_minutes": minutes(now - due)}),
                    SOURCE_RULE,
                    false,
                ));
            }
            Some(_) => {}
        }
    }
    out
}

#[cfg(test)]
#[path = "rules_dunks_tests.rs"]
mod rules_dunks_tests;
