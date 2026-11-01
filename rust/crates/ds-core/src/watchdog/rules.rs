//! `rule_task_blocked_stale` and `rule_task_lease_stale` of `dunk_watchdog.py`.

use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::{finding, minutes, py_float, py_number, TaskData, SOURCE_RULE};
use crate::error::DunkError;

/// A DECLARED marker, not prose inference: a blocked task whose reason starts with this is
/// parked on the owner, not stale. The digest still counts it.
pub const HELD_OWNER_MARKER: &str = "HELD FOR OWNER DECISION";

/// `_is_held`.
pub fn is_held(task: &Value) -> bool {
    task["block_reason"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_uppercase()
        .starts_with(HELD_OWNER_MARKER)
}

fn num(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// `rule_task_blocked_stale`.
pub fn rule_task_blocked_stale(data: &TaskData, rule: &Value, now: f64) -> Vec<Value> {
    let mut last_event: BTreeMap<String, f64> = BTreeMap::new();
    for event in &data.events {
        let id = event["task_id"].as_str().unwrap_or("").to_string();
        let at = num(&event["at"]);
        let entry = last_event.entry(id).or_insert(0.0);
        if at > *entry {
            *entry = at;
        }
    }
    let threshold = &rule["threshold_minutes"];
    let threshold_text = format!("{}min", py_number(threshold));
    let mut out = Vec::new();
    let mut held: Vec<(&Value, f64)> = Vec::new();
    for task in &data.tasks {
        if task["status"] != "blocked" {
            continue;
        }
        let id = task["id"].as_str().unwrap_or("");
        let last = num(&task["updated_at"]).max(last_event.get(id).copied().unwrap_or(0.0));
        let idle = now - last;
        if is_held(task) {
            held.push((task, idle));
            continue;
        }
        if idle >= num(threshold) * 60.0 {
            out.push(finding(
                "task.blocked_stale",
                rule,
                id,
                format!("task {id} blocked with no state change or event"),
                threshold_text.clone(),
                format!("{}min", py_float(minutes(idle))),
                "since last activity".into(),
                json!({
                    "task": id,
                    "assignees": task["assignees"],
                    "block_reason_set": !task["block_reason"].is_null(),
                }),
                SOURCE_RULE,
                false,
            ));
        }
    }
    if !held.is_empty() {
        let oldest = held.iter().map(|(_, idle)| *idle).fold(f64::MIN, f64::max) / 86_400.0;
        let ids: Vec<&Value> = held.iter().map(|(task, _)| &task["id"]).collect();
        let mut digest = finding(
            "task.blocked_stale",
            rule,
            "held-owner",
            format!(
                "{} task(s) held on owner decisions (oldest {oldest:.1}d)",
                held.len()
            ),
            threshold_text,
            format!("{oldest:.1}d"),
            "held by declared marker".into(),
            json!({
                "held": ids,
                "oldest_days": format!("{oldest:.1}").parse::<f64>().unwrap_or(0.0),
            }),
            "digest",
            false,
        );
        digest["severity"] = json!("info");
        out.push(digest);
    }
    out
}

/// `rule_task_lease_stale`. Returns an error where the Python raises: an event that passes the
/// name and age filters is parsed with `json.loads(event["meta"] or "{}")`, so a malformed meta
/// raises, and so does a meta that is valid JSON but not an object (`meta.get` on a list or a
/// number). The evaluator reports that as a rule error instead of a quiet skip (review t1401).
/// Events the filters drop are never parsed, exactly as in Python.
pub fn rule_task_lease_stale(
    data: &TaskData,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let mut out = Vec::new();
    let never_renewed = &rule["never_renewed_minutes"];
    for lease in &data.leases {
        let task = lease["task_id"].as_str().unwrap_or("");
        let pane = lease["pane_id"].as_str().unwrap_or("");
        let key = format!("{task}@{pane}");
        let (expires, heartbeat, claimed) = (
            num(&lease["expires_at"]),
            num(&lease["heartbeat_at"]),
            num(&lease["claimed_at"]),
        );
        if expires <= now {
            out.push(finding(
                "task.lease_stale",
                rule,
                &format!("expired:{key}"),
                format!("lease {key} expired and is not yet reaped"),
                "expires_at<=now".into(),
                format!("expired {}min ago", py_float(minutes(now - expires))),
                "current leases".into(),
                json!({"task": task, "pane": pane}),
                SOURCE_RULE,
                false,
            ));
        } else if heartbeat <= claimed && now - claimed >= num(never_renewed) * 60.0 {
            out.push(finding(
                "task.lease_stale",
                rule,
                &format!("unrenewed:{key}"),
                format!("lease {key} never renewed"),
                format!("{}min", py_number(never_renewed)),
                format!("{}min", py_float(minutes(now - claimed))),
                "since claim".into(),
                json!({"task": task, "pane": pane}),
                SOURCE_RULE,
                false,
            ));
        }
    }
    // A reaped event is history, not a current state: only fresh reaps are news, and each
    // event is reported once (finding once=True). The wider lookback is for the current
    // leases, not for resurrecting old events.
    let lookback = &rule["lookback_minutes"];
    let fresh_value = rule.get("fresh_minutes").unwrap_or(lookback);
    let fresh = num(fresh_value) * 60.0;
    let since = (now - num(lookback) * 60.0).max(now - fresh);
    for event in &data.events {
        let name = event["event"].as_str().unwrap_or("");
        if !matches!(name, "requeued" | "released") || num(&event["at"]) < since {
            continue;
        }
        let meta: Value = match event["meta"].as_str().filter(|s| !s.is_empty()) {
            None => json!({}),
            Some(text) => serde_json::from_str(text).map_err(|error| {
                DunkError::Refused(format!(
                    "task_events {} has malformed meta JSON: {error}",
                    event["id"]
                ))
            })?,
        };
        if !meta.is_object() {
            return Err(DunkError::Refused(format!(
                "task_events {} meta is not an object",
                event["id"]
            )));
        }
        let reason = meta["reason"].as_str().unwrap_or("");
        if reason != "expired" && reason != "pane_gone" {
            continue;
        }
        let task = event["task_id"].as_str().unwrap_or("");
        let age = minutes(now - num(&event["at"]));
        out.push(finding(
            "task.lease_stale",
            rule,
            &format!("reaped:{}", event["id"]),
            format!(
                "task {task} lease reaped ({reason}) {}min ago and {name}",
                py_float(age)
            ),
            "any reap".into(),
            format!("{}min old", py_float(age)),
            format!("{}min fresh window", py_number(fresh_value)),
            json!({
                "task": task,
                "event_id": event["id"],
                "reason": reason,
                "minutes_ago": age,
            }),
            SOURCE_RULE,
            true,
        ));
    }
    Ok(out)
}
