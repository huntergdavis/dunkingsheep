//! T53 (t1400 slice E): the lane rules and the fleet starvation rule of `dunk_watchdog.py`
//! (`lane.working_without_task`, `lane.idle_holding_task`, `fleet.assignment_starvation`) and the
//! inputs they read (`_pane_activity`, `_holders`, `Inputs.starvation_snapshot`, the metrics turn
//! events).
//!
//! The pane inventory is `Option<&Panes>`: `None` is the pane source being disabled, where the Python
//! calls `.items()` / `.get()` on `None` and raises. Those are `Err` here with the Python's text, and
//! the evaluator reports them as self errors (a later slice).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use super::rules_inbox::{InboxDb, Panes};
use super::{finding, rows, TaskData, SOURCE_RULE};
use crate::error::DunkError;

fn cfg_f64(rule: &Value, key: &str) -> f64 {
    rule[key].as_f64().unwrap_or(0.0)
}

/// Python's `re.match`: the pattern must match AT THE START of `text` (not anchored at the end).
fn matches_at_start(pattern: &Regex, text: &str) -> bool {
    pattern.find(text).is_some_and(|m| m.start() == 0)
}

fn compile(pattern: &str) -> Result<Regex, DunkError> {
    Regex::new(pattern).map_err(|error| DunkError::Refused(format!("bad pane_id_pattern: {error}")))
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// `_pane_activity`: pane id -> latest activity time from task events, inbox senders and inbox reads.
/// Read-only; every window is bounded above by `now`.
pub fn pane_activity(
    data: &TaskData,
    inbox: &InboxDb,
    since: f64,
    now: f64,
) -> Result<BTreeMap<String, f64>, DunkError> {
    let mut activity: BTreeMap<String, f64> = BTreeMap::new();
    let mut bump = |who: &str, at: f64| {
        let slot = activity.entry(who.to_string()).or_insert(0.0);
        *slot = slot.max(at);
    };
    for event in &data.events {
        let actor = text(event, "actor");
        let at = event["at"].as_f64().unwrap_or(0.0);
        if !actor.is_empty() && at >= since {
            bump(actor, at);
        }
    }
    let conn = Connection::open_with_flags(inbox.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(crate::db::map_sqlite)?;
    for (sql, what) in [
        (
            "SELECT sender, MAX(created_at) FROM messages WHERE sender IS NOT NULL \
             AND created_at>=? AND created_at<=? GROUP BY sender",
            "sender",
        ),
        (
            "SELECT pane_id, MAX(read_at) FROM messages WHERE pane_id IS NOT NULL \
             AND read_at>=? AND read_at<=? GROUP BY pane_id",
            "pane",
        ),
    ] {
        let mut stmt = conn.prepare(sql).map_err(crate::db::map_sqlite)?;
        let found = stmt
            .query_map([since, now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
            })
            .map_err(crate::db::map_sqlite)?;
        for row in found {
            let (who, at) = row.map_err(crate::db::map_sqlite)?;
            bump(&who, at);
        }
        let _ = what;
    }
    Ok(activity)
}

/// `_holders`: task id -> sorted holder pane ids, from leases and pane-resolvable assignees, for the
/// tasks in `statuses`. `names` is `Inputs.name_to_pane`.
pub fn holders(
    data: &TaskData,
    names: &BTreeMap<String, String>,
    pane_id_pattern: &str,
    statuses: &[&str],
) -> Result<Vec<(String, Vec<String>)>, DunkError> {
    let pattern = compile(pane_id_pattern)?;
    let mut out = Vec::new();
    for task in &data.tasks {
        if !statuses.contains(&text(task, "status")) {
            continue;
        }
        let id = text(task, "id");
        let mut panes: BTreeSet<String> = data
            .leases
            .iter()
            .filter(|lease| text(lease, "task_id") == id)
            .map(|lease| text(lease, "pane_id").to_string())
            .collect();
        for who in task["assignees"].as_array().into_iter().flatten() {
            let who = who.as_str().unwrap_or("");
            let pane = if matches_at_start(&pattern, who) {
                Some(who.to_string())
            } else {
                names.get(who).cloned()
            };
            if let Some(pane) = pane.filter(|p| !p.is_empty()) {
                panes.insert(pane);
            }
        }
        if !panes.is_empty() {
            out.push((id.to_string(), panes.into_iter().collect()));
        }
    }
    Ok(out)
}

fn no_panes(method: &str) -> DunkError {
    DunkError::Refused(format!("'NoneType' object has no attribute '{method}'"))
}

/// A pane that is `working` while it holds no task and has emitted no task event recently.
pub fn rule_lane_working_without_task(
    panes: Option<&Panes>,
    data: &TaskData,
    names: &BTreeMap<String, String>,
    pane_id_pattern: &str,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let panes = panes.ok_or_else(|| no_panes("items"))?;
    let since = now - cfg_f64(rule, "quiet_minutes") * 60.0;
    let task_actors: BTreeSet<&str> = data
        .events
        .iter()
        .filter(|e| !text(e, "actor").is_empty() && e["at"].as_f64().unwrap_or(0.0) >= since)
        .map(|e| text(e, "actor"))
        .collect();
    let holding: BTreeSet<String> = holders(data, names, pane_id_pattern, &["in_progress"])?
        .into_iter()
        .flat_map(|(_, panes)| panes)
        .collect();
    let mut out = Vec::new();
    for (pane, status) in panes {
        if status == "working" && !holding.contains(pane) && !task_actors.contains(pane.as_str()) {
            out.push(finding(
                "lane.working_without_task",
                rule,
                pane,
                format!("pane {pane} is working with no task held and no task event"),
                format!("{}min quiet", super::py_number(&rule["quiet_minutes"])),
                "no task activity".into(),
                "current".into(),
                json!({"pane": pane, "agent_status": status}),
                SOURCE_RULE,
                false,
            ));
        }
    }
    Ok(out)
}

/// A pane holding a task while idle, with no task, inbox or read activity in the quiet window.
pub fn rule_lane_idle_holding_task(
    panes: Option<&Panes>,
    data: &TaskData,
    inbox: &InboxDb,
    names: &BTreeMap<String, String>,
    pane_id_pattern: &str,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let panes = panes.ok_or_else(|| no_panes("get"))?;
    let since = now - cfg_f64(rule, "quiet_minutes") * 60.0;
    let activity = pane_activity(data, inbox, since, now)?;
    let idle: Vec<&str> = rule["idle_statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let holding: Vec<&str> = rule["holding_statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut held = holders(data, names, pane_id_pattern, &holding)?;
    held.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (task_id, holder_panes) in held {
        for pane in holder_panes {
            let Some(status) = panes.get(&pane) else {
                continue;
            };
            if idle.contains(&status.as_str()) && !activity.contains_key(&pane) {
                out.push(finding(
                    "lane.idle_holding_task",
                    rule,
                    &format!("{task_id}@{pane}"),
                    format!("pane {pane} holds task {task_id} while {status} with no activity"),
                    format!("{}min quiet", super::py_number(&rule["quiet_minutes"])),
                    format!("{status}, no activity"),
                    "current".into(),
                    json!({"task": task_id, "pane": pane, "agent_status": status}),
                    SOURCE_RULE,
                    false,
                ));
            }
        }
    }
    Ok(out)
}

/// `Inputs.starvation_snapshot`: the current global backlog and bounded progress; the event history
/// is not loaded.
#[derive(Debug, Clone, Default)]
pub struct Starvation {
    pub tasks: Vec<Value>,
    pub lease_panes: Vec<String>,
    pub progress: Vec<Value>,
}

impl Starvation {
    pub fn load(tasks_db: &Path, now: f64, since: f64) -> Result<Starvation, DunkError> {
        let conn = Connection::open_with_flags(tasks_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(crate::db::map_sqlite)?;
        let mut tasks = rows(
            &conn,
            "SELECT id,scope,status,assignees,created_at,block_reason FROM tasks \
             WHERE status IN ('open','in_progress','blocked') AND created_at<=? ORDER BY id",
            &[&now],
        )?;
        let leases = rows(
            &conn,
            "SELECT pane_id FROM task_leases WHERE claimed_at<=? AND expires_at>?",
            &[&now, &now],
        )?;
        let progress = rows(
            &conn,
            "SELECT actor,event FROM task_events WHERE at>=? AND at<=? \
             AND event IN ('claimed','completed') ORDER BY at,id",
            &[&since, &now],
        )?;
        for task in &mut tasks {
            // `json.loads(task["assignees"] or "[]")` raises on malformed JSON.
            let parsed = match task["assignees"].as_str().filter(|s| !s.is_empty()) {
                None => json!([]),
                Some(raw) => serde_json::from_str::<Value>(raw).map_err(|error| {
                    DunkError::Refused(format!(
                        "task {} has malformed assignees JSON: {error}",
                        text(task, "id")
                    ))
                })?,
            };
            task["assignees"] = parsed;
        }
        Ok(Starvation {
            tasks,
            lease_panes: leases
                .iter()
                .filter_map(|l| l["pane_id"].as_str().map(str::to_string))
                .collect(),
            progress,
        })
    }
}

/// The panes with a turn start/end in the window (`SELECT DISTINCT pane_id FROM events ...`).
pub fn turn_panes(metrics_db: &Path, since: f64, now: f64) -> Result<Vec<String>, DunkError> {
    let conn = Connection::open_with_flags(metrics_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(crate::db::map_sqlite)?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT pane_id FROM events WHERE kind IN ('turn.start','turn.end') \
             AND at>=? AND at<=?",
        )
        .map_err(crate::db::map_sqlite)?;
    let found = stmt
        .query_map([since, now], |r| r.get::<_, Option<String>>(0))
        .map_err(crate::db::map_sqlite)?;
    let mut out = Vec::new();
    for row in found {
        if let Some(pane) = row.map_err(crate::db::map_sqlite)? {
            out.push(pane);
        }
    }
    Ok(out)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Empty ready queues are a supply problem, even while reviewers are busy.
///
/// Only explicitly configured, present worker panes count. Unknown activity is reported separately,
/// never called idle or eligible capacity. Blocked work is evidence for an operator decision, never
/// permission to bypass it.
///
/// `turn_busy` and `panes` are read lazily, after the empty-backlog return, so a store that is
/// unreadable while there is nothing to judge stays quiet, as in the Python.
pub fn rule_fleet_assignment_starvation(
    snapshot: &Starvation,
    turn_busy: &dyn Fn() -> Result<Vec<String>, DunkError>,
    panes: &dyn Fn() -> Result<Option<Panes>, DunkError>,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let since = now - cfg_f64(rule, "quiet_minutes") * 60.0;
    let backlog: Vec<&Value> = snapshot
        .tasks
        .iter()
        .filter(|t| {
            text(t, "scope") == "global"
                && ["open", "blocked"].contains(&text(t, "status"))
                && t["created_at"].as_f64().unwrap_or(f64::MAX) <= since
        })
        .collect();
    if backlog.is_empty() {
        return Ok(Vec::new());
    }
    let mut busy: BTreeSet<String> = snapshot.lease_panes.iter().cloned().collect();
    busy.extend(
        snapshot
            .progress
            .iter()
            .filter_map(|r| r["actor"].as_str().map(str::to_string)),
    );
    busy.extend(turn_busy()?);
    let assigned: BTreeSet<String> = snapshot
        .tasks
        .iter()
        .filter(|t| ["open", "in_progress"].contains(&text(t, "status")))
        .flat_map(|t| strings(&t["assignees"]))
        .collect();
    let panes = panes()?;
    let workers = strings(&rule["worker_panes"]);
    let superusers: BTreeSet<String> = strings(&rule["superuser_panes"]).into_iter().collect();
    let worker_set: BTreeSet<String> = workers
        .into_iter()
        .filter(|p| !superusers.contains(p))
        .collect();
    let mut empty: Vec<(String, String)> = Vec::new();
    if !worker_set.is_empty() {
        let panes = panes.ok_or_else(|| {
            DunkError::Refused("argument of type 'NoneType' is not iterable".into())
        })?;
        for pane in worker_set {
            let Some(status) = panes.get(&pane) else {
                continue;
            };
            if ["idle", "done", "unknown"].contains(&status.as_str())
                && !busy.contains(&pane)
                && !assigned.contains(&pane)
            {
                empty.push((pane, status.clone()));
            }
        }
    }
    let idle: Vec<&String> = empty
        .iter()
        .filter(|(_, s)| s == "idle" || s == "done")
        .map(|(p, _)| p)
        .collect();
    let unknown: Vec<&String> = empty
        .iter()
        .filter(|(_, s)| s == "unknown")
        .map(|(p, _)| p)
        .collect();
    if (empty.len() as f64) < cfg_f64(rule, "min_empty_workers") {
        return Ok(Vec::new());
    }
    let blocked: Vec<&str> = backlog
        .iter()
        .filter(|t| text(t, "status") == "blocked")
        .map(|t| text(t, "id"))
        .collect();
    let unstarted: Vec<&str> = backlog
        .iter()
        .filter(|t| text(t, "status") == "open")
        .map(|t| text(t, "id"))
        .collect();
    let quiet = super::py_number(&rule["quiet_minutes"]);
    let count_event = |name: &str| {
        snapshot
            .progress
            .iter()
            .filter(|r| text(r, "event") == name)
            .count()
    };
    Ok(vec![finding(
        "fleet.assignment_starvation",
        rule,
        "fleet",
        format!(
            "{} worker queues empty while backlog waits ({} confirmed idle, {} activity unknown); \
             superusers: verify unknown activity, rank and assign runnable work or resolve named \
             gates; preserve owner holds",
            empty.len(),
            idle.len(),
            unknown.len()
        ),
        format!(
            "{} empty queues quiet {quiet}min",
            super::py_number(&rule["min_empty_workers"])
        ),
        format!(
            "{} idle; {} unknown; {} open; {} blocked",
            idle.len(),
            unknown.len(),
            unstarted.len(),
            blocked.len()
        ),
        format!("last {quiet}min"),
        json!({
            "idle_workers": idle,
            "activity_unknown_workers": unknown,
            "open_tasks": unstarted.iter().take(20).collect::<Vec<_>>(),
            "blocked_tasks": blocked.iter().take(20).collect::<Vec<_>>(),
            "backlog_count": backlog.len(),
            "claims_in_window": count_event("claimed"),
            "done_records_in_window": count_event("completed"),
            "limits": "Open does not prove runnable; done events may include review/bookkeeping. \
                       This calls for allocation decisions, not automatic reassignment.",
        }),
        SOURCE_RULE,
        false,
    )])
}

#[cfg(test)]
#[path = "rules_lanes_tests.rs"]
mod rules_lanes_tests;
