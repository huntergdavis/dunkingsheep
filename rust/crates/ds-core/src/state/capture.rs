//! `capture` (`dunk_state.capture`): one capture, a row per agent lane, fields from argv, the
//! process environment, the activity record and the task leases. Slice 6c.
//!
//! t496: ONE /proc walk per capture, not two per lane. `session_of` and the env lookup both accept
//! a prebuilt pid -> children map; the Python once gave neither one, so every lane rebuilt the
//! whole map twice (62 full /proc scans for 31 lanes, the largest single cost of a capture). The
//! tests pin the call COUNT, not the wall clock.
//!
//! What the Python's measurement does NOT establish (reviewer t500 drew this line): that the scan
//! was the sole cause of the observed "no answer after 15s" failure, or that no daemon-wide lock
//! was contended during it.
//!
//! Everything the Python reaches through the flock (the lane list, the activity record, the task
//! leases) is a [`CaptureEnv`] method.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

use super::proc::{descendants, read_bound, ProcReader};
use super::session::session_of;
use super::{flag, project_of, StateStore, MODEL_FLAGS, THINKING_FLAGS};
use crate::error::DunkError;

/// The activity answer a row carries (`dunk_activity.activity(...)`: `state`, `source`, `reason`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LaneActivity {
    pub state: Option<String>,
    pub source: Option<String>,
    pub reason: Option<String>,
}

/// What `capture` reads from the flock.
pub trait CaptureEnv {
    /// `flock.resolve_lanes()`: lane rows (`pane_id`, `agent`, `pane_pid`, `cwd`, ...).
    fn resolve_lanes(&self) -> Vec<Value>;
    /// `dunk_activity.activity(lane, claude_status, turns, now)`.
    fn activity(&self, lane: &Value, now: f64) -> LaneActivity;
    /// `flock.tasks.leases_for_pane(pane_id, now)[0]["task_id"]`; `None` for no lease or any
    /// error (the Python swallows it).
    fn lease_task_id(&self, pane_id: &str, now: f64) -> Option<String>;
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `launches.json`'s `panes` map, each entry with its `pane_id`; an unreadable file is empty.
fn read_launches(config_dir: &Path) -> BTreeMap<String, Value> {
    let parsed: Option<Value> = std::fs::read_to_string(config_dir.join("launches.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let mut launches = BTreeMap::new();
    if let Some(panes) = parsed
        .as_ref()
        .and_then(|p| p.get("panes"))
        .and_then(Value::as_object)
    {
        for (pane_id, entry) in panes {
            let mut entry = entry.as_object().cloned().unwrap_or_default();
            entry.insert("pane_id".into(), json!(pane_id));
            launches.insert(pane_id.clone(), Value::Object(entry));
        }
    }
    launches
}

fn argv_of(launch: &Value) -> Vec<String> {
    launch
        .get("argv")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn pid_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// `capture(flock, source, now, store)`: `{"at", "rows", "source"}`.
pub fn capture(
    env: &dyn CaptureEnv,
    reader: &dyn ProcReader,
    store: &StateStore,
    config_dir: &Path,
    source: &str,
    now: Option<f64>,
) -> Result<Value, DunkError> {
    let now = now
        .filter(|n| *n != 0.0)
        .unwrap_or_else(crate::clock::now_s);
    let launches = read_launches(config_dir);
    let (kids, starts) = reader.proc_snapshot(); // ONE /proc walk per capture
    let mut rows: Vec<Value> = Vec::new();
    for lane in env.resolve_lanes() {
        if !lane.get("agent").is_some_and(truthy) {
            continue;
        }
        let pane_id = lane
            .get("pane_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let launch = launches.get(&pane_id).cloned();
        let has_launch = launch.as_ref().is_some_and(truthy);
        let launch = launch.unwrap_or_else(|| json!({}));
        let argv = argv_of(&launch);
        let mut model = flag(&argv, &MODEL_FLAGS);
        let mut thinking = flag(&argv, &THINKING_FLAGS);
        let recorded = launch
            .get("session")
            .filter(|v| truthy(v))
            .or_else(|| launch.get("session_id").filter(|v| truthy(v)))
            .cloned()
            .unwrap_or(Value::Null);
        let cwd = lane.get("cwd").and_then(Value::as_str);
        let pane_pid = lane.get("pane_pid");
        let (session, session_reason) = session_of(
            reader,
            pane_pid,
            Some(&json!({"session": recorded})),
            Some(&kids),
            cwd,
            Some(&starts),
        );
        let mut environment: BTreeMap<String, String> = BTreeMap::new();
        if let Some(root) = pane_pid.filter(|v| truthy(v)).and_then(pid_of) {
            let mut candidates = vec![root];
            candidates.extend(descendants(root, &kids));
            for candidate in candidates {
                let (found, held) =
                    read_bound(reader, candidate, Some(&starts), |p| reader.env_of(p));
                let Some(found) = found.filter(|_| held) else {
                    // t505/t500: a recycled pid would hand this pane another lane's environment
                    // (session, model, effort). Validated before AND after the read, so the
                    // check-to-read race is closed.
                    continue;
                };
                if found.get("PI_SESSION_ID").is_some_and(|v| !v.is_empty())
                    || found.get("PI_MODEL").is_some_and(|v| !v.is_empty())
                {
                    environment = found;
                    break;
                }
            }
        }
        let env_matches = !environment.is_empty()
            && (session.as_deref().is_none_or(str::is_empty)
                || environment.get("PI_SESSION_ID").map(String::as_str) == session.as_deref());
        if model.as_deref().is_none_or(str::is_empty) && env_matches {
            if let Some(value) = environment.get("PI_MODEL").filter(|v| !v.is_empty()) {
                model = Some(value.clone());
            }
        }
        if thinking.as_deref().is_none_or(str::is_empty) && env_matches {
            if let Some(value) = environment
                .get("PI_REASONING_LEVEL")
                .filter(|v| !v.is_empty())
            {
                thinking = Some(value.clone());
            }
        }
        let record = env.activity(&lane, now);
        let task_id = env.lease_task_id(&pane_id, now);
        let model = model.filter(|m| !m.is_empty());
        let thinking = thinking.filter(|t| !t.is_empty());
        let model_reason = if model.is_some() {
            Value::Null
        } else if has_launch {
            json!("no --model flag in the argv, and the process environment names no model")
        } else {
            json!("no launch row for this pane: no argv to read a model from, and the process environment names no model")
        };
        let thinking_reason = if thinking.is_some() {
            Value::Null
        } else if has_launch {
            json!("no --thinking flag in the argv, and the process environment names no reasoning level")
        } else {
            json!("no launch row for this pane: no argv to read a thinking level from, and the process environment names no reasoning level")
        };
        rows.push(json!({
            "pane_id": pane_id,
            "project": project_of(cwd),
            "harness": lane.get("agent").cloned().unwrap_or(Value::Null),
            "cwd": lane.get("cwd").cloned().unwrap_or(Value::Null),
            "argv": launch.get("argv").cloned().unwrap_or(Value::Null),
            "session_id": session,
            // Kept even when a session WAS resolved: a lane whose session came from a live file
            // while its environment claimed another one must still say so.
            "session_reason": session_reason,
            "model": model,
            "model_reason": model_reason,
            "thinking": thinking,
            "thinking_reason": thinking_reason,
            "state": record.state,
            "state_source": record.source,
            "state_reason": record.reason,
            "task_id": task_id,
            "source": source,
        }));
    }
    store.add(&rows, now)?;
    Ok(json!({"at": now, "rows": rows.len(), "source": source}))
}
