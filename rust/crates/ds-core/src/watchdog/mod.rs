//! T53 (first slice): `dunk_watchdog.py` findings and the two task rules.
//!
//! In this slice: the finding shape (`finding`), `_minutes`, `_sort_key`, the read-only
//! task-store loader (`Inputs.tasks`), `rule_task_blocked_stale` and `rule_task_lease_stale`.
//!
//! NOT in this slice: config validation (`_validate`, `load_config`), the other `rule_*`
//! functions, `evaluate`, the digest/escalation classes, the confirm path, state, `run`, and
//! `dunk_watchdog_deadman.py`. A finding is a `serde_json::Value` object; its KEYS come out
//! sorted (no `preserve_order`, the owner's open decision), which `json.dumps` of the Python
//! dict would not do, so a golden comparison must compare parsed values, not bytes.

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::path::Path;

use crate::error::DunkError;

pub mod config;
pub mod rules;
pub mod rules_inbox;

pub const SOURCE_RULE: &str = "watchdog-rule";

/// `SEVERITY_RANK`: info < warning < critical; anything else ranks below `info` (-1).
pub fn severity_rank(severity: &str) -> i64 {
    match severity {
        "info" => 0,
        "warning" => 1,
        "critical" => 2,
        _ => -1,
    }
}

/// `_sort_key`: most severe first, then rule, then key.
pub fn sort_findings(findings: &mut [Value]) {
    findings.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                -severity_rank(v["severity"].as_str().unwrap_or("")),
                v["rule"].as_str().unwrap_or("").to_string(),
                v["key"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
}

/// Python's `str(float)` for the magnitudes the watchdog prints (`5.0`, `0.2`, `1234.5`):
/// Rust's `{:?}` keeps the `.0` that `{}` drops. NOT mirrored: exponent forms (`1e+21`).
pub fn py_float(value: f64) -> String {
    format!("{value:?}")
}

/// `str(x)` of a JSON number as the config holds it: an integer prints bare (`60`), a float
/// as a Python float (`0.01`, `60.0`).
pub fn py_number(value: &Value) -> String {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        Value::Number(n) => py_float(n.as_f64().unwrap_or(0.0)),
        other => other.to_string(),
    }
}

/// `_minutes`: `round(seconds / 60.0, 1)`. Python rounds the exact binary value half to even
/// (`round(0.25, 1) == 0.2`); Rust's `{:.1}` formats the exact value the same way, where
/// `(x * 10.0).round() / 10.0` would give 0.3.
pub fn minutes(seconds: f64) -> f64 {
    format!("{:.1}", seconds / 60.0).parse().unwrap_or(0.0)
}

/// `finding(...)`: one finding. `threshold`, `measured` and `window` are the value+unit strings
/// printed on every dry-run line. `once` marks an immutable event: reported once ever, not
/// again after a cooldown.
#[allow(clippy::too_many_arguments)]
pub fn finding(
    rule_id: &str,
    rule: &Value,
    key: &str,
    summary: String,
    threshold: String,
    measured: String,
    window: String,
    evidence: Value,
    source: &str,
    once: bool,
) -> Value {
    let mut item = Map::new();
    item.insert("rule".into(), json!(rule_id));
    item.insert("key".into(), json!(key));
    item.insert("severity".into(), rule["severity"].clone());
    item.insert("class".into(), rule["class"].clone());
    item.insert("source".into(), json!(source));
    item.insert("summary".into(), json!(summary));
    item.insert("threshold".into(), json!(threshold));
    item.insert("measured".into(), json!(measured));
    item.insert("window".into(), json!(window));
    item.insert("evidence".into(), evidence);
    if once {
        item.insert("once".into(), json!(true));
    }
    Value::Object(item)
}

/// `Inputs.tasks()`: the task store as the rules read it. Read-only; the events are bounded
/// above by `now`, so evaluating at a past `now` never sees the future.
#[derive(Debug, Clone, Default)]
pub struct TaskData {
    pub tasks: Vec<Value>,
    pub leases: Vec<Value>,
    pub events: Vec<Value>,
}

fn sql_value(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(_) => Value::Null,
    }
}

fn rows(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Value>, DunkError> {
    let mut stmt = conn.prepare(sql).map_err(crate::db::map_sqlite)?;
    let names: Vec<String> = stmt.column_names().iter().map(|n| n.to_string()).collect();
    let mapped = stmt
        .query_map(params, |row| {
            let mut object = Map::new();
            for (index, name) in names.iter().enumerate() {
                object.insert(name.clone(), sql_value(row.get_ref(index)?));
            }
            Ok(Value::Object(object))
        })
        .map_err(crate::db::map_sqlite)?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::db::map_sqlite)
}

impl TaskData {
    /// Read `tasks.db` read-only. An unreadable store is an error the evaluator turns into a
    /// self error for the rules that needed it, never a silent pass.
    pub fn load(tasks_db: &Path, now: f64) -> Result<TaskData, DunkError> {
        let conn = Connection::open_with_flags(tasks_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(crate::db::map_sqlite)?;
        let mut tasks = rows(&conn, "SELECT * FROM tasks ORDER BY id", &[])?;
        let leases = rows(
            &conn,
            "SELECT * FROM task_leases ORDER BY task_id, pane_id",
            &[],
        )?;
        let events = rows(
            &conn,
            "SELECT * FROM task_events WHERE at<=? ORDER BY at, id",
            &[&now],
        )?;
        for task in &mut tasks {
            // `json.loads(task.get("assignees") or "[]")`: absent or empty is `[]`, but a
            // malformed value RAISES in Python, so the load fails and the rules that needed
            // the store are reported as errors, never passed silently (review t1401).
            let parsed = match task["assignees"].as_str().filter(|s| !s.is_empty()) {
                None => json!([]),
                Some(text) => serde_json::from_str::<Value>(text).map_err(|error| {
                    DunkError::Refused(format!(
                        "task {} has malformed assignees JSON: {error}",
                        task["id"].as_str().unwrap_or("?")
                    ))
                })?,
            };
            task["assignees"] = parsed;
        }
        Ok(TaskData {
            tasks,
            leases,
            events,
        })
    }
}

#[cfg(test)]
#[path = "rules_tests.rs"]
mod rules_tests;
