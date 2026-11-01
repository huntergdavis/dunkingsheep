//! `tick`, the state file and `summary` (`dunk_dispatch.tick`, `_load_state`, `_write_state`,
//! `_dispatched_in_window`, `summary`): fill idle lanes from the standby queue.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use super::queue::{DispatchError, Queue};
use super::{dispatch_text, lane_plan, py_str, truthy};
use crate::error::DunkError;

/// `tick`'s keyword arguments (the Python's `state_path=None` means "do not write").
pub struct Tick<'a> {
    pub now: Option<f64>,
    pub dry_run: bool,
    pub lanes: Option<&'a BTreeSet<String>>,
    pub state_path: Option<&'a Path>,
    /// Board items derived for this tick (`derive_board_items`).
    pub derived: &'a [Value],
}

/// `_load_state`: an unreadable or malformed file is an empty state.
pub fn load_state(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// `_dispatched_in_window`: how many sends this hour, from the history this module keeps.
pub fn dispatched_in_window(path: &Path, now: f64, window_s: f64) -> usize {
    load_state(path)
        .get("history")
        .and_then(Value::as_array)
        .map_or(0, |history| {
            history
                .iter()
                .filter(|entry| {
                    entry.get("at").and_then(Value::as_f64).unwrap_or(0.0) >= now - window_s
                })
                .count()
        })
}

/// `_write_state`: atomic, and it keeps a small history so "dispatched this hour" is real.
pub fn write_state(state: &mut Value, path: &Path) -> Result<(), DunkError> {
    let previous = load_state(path);
    let tick_at = state["tick_at"].as_f64().unwrap_or(0.0);
    let at = |entry: &Value| entry.get("at").and_then(Value::as_f64).unwrap_or(0.0);
    let mut history: Vec<Value> = previous
        .get("history")
        .and_then(Value::as_array)
        .map(|h| {
            h.iter()
                .filter(|entry| at(entry) >= tick_at - 24.0 * 3600.0)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for record in state["dispatched"].as_array().cloned().unwrap_or_default() {
        if record.get("sent").is_some_and(truthy) {
            history.push(
                json!({"at": tick_at, "pane_id": record["pane_id"], "item": record["item"],
                "task_id": record.get("task_id").cloned().unwrap_or(Value::Null)}),
            );
        }
    }
    // Count AFTER this tick's sends: the number the owner reads includes them.
    state["dispatched_1h"] = json!(history
        .iter()
        .filter(|entry| at(entry) >= tick_at - 3600.0)
        .count());
    state["history"] = Value::Array(history);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    // indent=2, insertion order (the Python does not sort keys here), no trailing newline.
    let text = serde_json::to_string_pretty(state)?;
    let temporary = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn count(plan: &Value, key: &str) -> usize {
    plan[key].as_array().map_or(0, Vec::len)
}

fn trimmed(plan: &Value, key: &str) -> Value {
    Value::Array(
        plan[key]
            .as_array()
            .map(|rows| rows.iter().map(|r| json!({"pane_id": r["pane_id"], "name": r["name"], "reason": r["reason"]})).collect())
            .unwrap_or_default(),
    )
}

/// `tick(queue, panes, ...)`: fill idle lanes from the standby queue. Returns the state it wrote.
///
/// `send(pane, text)` must be the guarded send path (the inbox nudge), so the dispatcher inherits
/// the typing hold and the dead-session refusal rather than typing over anything itself. Its
/// error text is recorded on the dispatch record (a refused send is a fact, not a crash).
pub fn tick(
    queue: &Queue,
    panes: &[Value],
    delivery: &dyn Fn(&Value) -> Value,
    unread: &dyn Fn(&Value) -> i64,
    send: &mut dyn FnMut(&str, &str) -> Result<Value, String>,
    options: &Tick<'_>,
) -> Result<Value, DispatchError> {
    let now = options
        .now
        .filter(|n| *n != 0.0)
        .unwrap_or_else(crate::clock::now_s);
    let plan = lane_plan(panes, delivery, unread, options.lanes);
    // Derived board items first: they are the real backlog. The hand queue is for work that is not
    // a task yet, so it comes second rather than starving it.
    let mut pending_derived: VecDeque<Value> = options.derived.iter().cloned().collect();
    let mut dispatched: Vec<Value> = Vec::new();
    let ready = plan["ready"].as_array().cloned().unwrap_or_default();
    for lane in &ready {
        let pane_id = lane["pane_id"].as_str().unwrap_or_default();
        let item = match pending_derived.pop_front() {
            Some(mut item) => {
                if !item.get("id").is_some_and(truthy) {
                    item["id"] = json!(format!("board:{}", py_str(item.get("task_id"))));
                }
                item
            }
            None => match queue.next_for(pane_id)? {
                Some(item) => item,
                None => break,
            },
        };
        let text = dispatch_text(&item);
        let mut record = json!({"pane_id": pane_id, "name": lane["name"], "item": item["id"],
            "task_id": item.get("task_id").cloned().unwrap_or(Value::Null), "text": text, "sent": false});
        if !options.dry_run {
            let outcome = (|| -> Result<(), String> {
                let message_id = send(pane_id, &text)?;
                record["message_id"] = message_id.clone();
                record["sent"] = json!(true);
                if !item.get("derived").is_some_and(truthy) {
                    let item_id = item["id"]
                        .as_i64()
                        .ok_or_else(|| format!("no standby item {}", py_str(item.get("id"))))?;
                    let message = message_id.as_str().map(str::to_string);
                    queue
                        .take(item_id, pane_id, message.as_deref(), Some(now))
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            })();
            if let Err(error) = outcome {
                // A refused send is a fact, not a crash.
                record["error"] = json!(error);
            }
        }
        dispatched.push(record);
    }

    let pending = queue.items(true)?;
    let unfilled_derived = !pending_derived.is_empty();
    let filled: BTreeSet<String> = dispatched
        .iter()
        .filter(|r| r["sent"] == true)
        .filter_map(|r| r["pane_id"].as_str().map(str::to_string))
        .collect();
    let idle_unfilled: Vec<String> = ready
        .iter()
        .filter_map(|lane| lane["pane_id"].as_str())
        .filter(|pane| !filled.contains(*pane))
        .map(str::to_string)
        .collect();
    let sent_count = dispatched.iter().filter(|r| r["sent"] == true).count();
    let derived_dispatched = dispatched
        .iter()
        .filter(|r| r["sent"] == true && py_str(r.get("item")).starts_with("board:"))
        .count();
    let mut state = json!({
        "tick_at": now,
        "dry_run": options.dry_run,
        "lanes": count(&plan, "ready") + count(&plan, "has_mail") + count(&plan, "blocked") + count(&plan, "dead") + count(&plan, "working"),
        "ready": count(&plan, "ready"),
        "working": count(&plan, "working"),
        "has_mail": count(&plan, "has_mail"),
        "blocked": trimmed(&plan, "blocked"),
        "dead": trimmed(&plan, "dead"),
        "dispatched": dispatched,
        "dispatched_count": sent_count,
        "idle_unfilled": idle_unfilled,
        "queue_depth": pending.len(),
        "derived_available": options.derived.len(),
        "derived_dispatched": derived_dispatched,
        // The honest answer when lanes sit idle: there is no open unblocked work anywhere - not in
        // the board and not in the hand queue.
        "queue_empty": pending.is_empty() && !unfilled_derived && options.derived.is_empty() && !idle_unfilled.is_empty(),
    });
    state["dispatched_1h"] = json!(0);
    if let Some(path) = options.state_path {
        write_state(&mut state, path)?; // computes dispatched_1h from the history
    }
    Ok(state)
}

/// `summary(state)`: the three numbers the owner asked for, one line each.
pub fn summary(state: &Value) -> String {
    let list = |key: &str| {
        state
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let idle = list("idle_unfilled");
    let blocked = list("blocked");
    let dead = list("dead");
    let reasons: BTreeSet<String> = blocked
        .iter()
        .map(|item| {
            format!(
                "{}: {}",
                py_str(item.get("name")),
                py_str(item.get("reason"))
            )
        })
        .collect();
    let reasons: Vec<String> = reasons.into_iter().collect();
    let mut lines = vec![
        format!("idle lanes: {}", idle.len()),
        format!(
            "dispatched this hour: {}",
            py_str(state.get("dispatched_1h"))
        ),
        format!(
            "blocked lanes: {}{}",
            blocked.len(),
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", reasons.join("; "))
            }
        ),
    ];
    if !dead.is_empty() {
        let names: Vec<String> = dead.iter().map(|item| py_str(item.get("name"))).collect();
        lines.push(format!("dead lanes: {} ({})", dead.len(), names.join(", ")));
    }
    if state.get("queue_empty").is_some_and(truthy) {
        lines.push(format!(
            "the standby queue is empty and {} lane(s) are idle",
            idle.len()
        ));
    }
    lines.join("\n")
}
