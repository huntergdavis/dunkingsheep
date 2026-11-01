//! `dispatch.rs` (T54, slice 5a): the dispatcher's PURE planners, from `dunk_dispatch.py`:
//! `lane_plan`, `derive_board_items`, `dispatch_text`, `claimed_tasks`, `claimed_work`,
//! `turn_end_plan`, `turn_end_text`.
//!
//! No model and no I/O: everything is injected, so the guardrails can be read and tested without
//! a daemon. The timer is the fallback, not the mechanism: `turn_end_plan` dispatches on the turn
//! end the harness already reports, because turns last seconds and timers fire in hours.
//!
//! Tasks, panes and plans are JSON objects (insertion-ordered like the Python dicts), the shape the
//! board and the wire already use.
//!
//! Slice 5b adds `next_job_plan`, `assigned_open_tasks`, `delivery_retry_plan` here, and the sqlite
//! [`queue::Queue`] and [`tick`]/`summary` (with the state file) as submodules.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

pub mod queue;
pub mod tick;

pub use queue::{DispatchError, Queue};
pub use tick::{dispatched_in_window, load_state, summary, tick, write_state, Tick};

/// `PRIORITY_ORDER`.
pub fn priority_order(name: &str) -> i64 {
    match name {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 9,
    }
}

/// `HELD_PREFIX`: kept as documentation of the marker: a held task carries a block_reason like any
/// other blocked task, and the dispatcher skips every block_reason.
pub const HELD_PREFIX: &str = "HELD FOR OWNER DECISION";

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// `TURN_END_MIN_GAP_S` (`DUNKINGSHEEP_TURN_END_MIN_GAP_S`, default 30), read per call here.
pub fn turn_end_min_gap_s() -> f64 {
    env_f64("DUNKINGSHEEP_TURN_END_MIN_GAP_S", 30.0)
}

/// `TURN_END_TASK_COOLDOWN_S` (`DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S`, default 300).
///
/// THE SAME TASK IS NOT RE-PROMPTED ON EVERY TURN END: a lane that ends a 20-second turn 30 times
/// while holding one task must not become a nudge loop. This is a LOOP GUARD, not a cadence, and
/// it is deliberately SHORT (5 minutes): a long value would re-create the hourly-timer problem one
/// layer down, because a cooldown only suppresses prompts - it cannot wake an idle lane.
pub fn turn_end_task_cooldown_s() -> f64 {
    env_f64("DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S", 300.0)
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

/// Python's `str()` of a JSON value (`None` for null/absent).
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `str(task.get(key) or default)`.
fn str_or(task: &Value, key: &str, default: &str) -> String {
    match task.get(key).filter(|v| truthy(v)) {
        Some(value) => py_str(Some(value)),
        None => default.to_string(),
    }
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(0.0)
}

/// `" ".join(text.split())`.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `lane_plan(panes, delivery, unread, lanes)`: classify every lane for one tick. Pure.
///
/// A ready pane with unread mail is reported as `has_mail`, not as ready: it already has an
/// assignment waiting. An unknown state is reported as blocked, never ready.
pub fn lane_plan(
    panes: &[Value],
    delivery: &dyn Fn(&Value) -> Value,
    unread: &dyn Fn(&Value) -> i64,
    lanes: Option<&BTreeSet<String>>,
) -> Value {
    let mut out: BTreeMap<&str, Vec<Value>> = ["ready", "has_mail", "blocked", "dead", "working"]
        .into_iter()
        .map(|k| (k, Vec::new()))
        .collect();
    for pane in panes {
        let Some(pane_id) = pane
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        if lanes.is_some_and(|set| !set.contains(pane_id)) {
            continue;
        }
        let state = delivery(pane);
        let name = state.get("state").and_then(Value::as_str);
        let reason = state.get("reason").cloned().unwrap_or(Value::Null);
        let label = pane
            .get("agent_name")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(pane_id));
        let entry = json!({"pane_id": pane_id, "name": label, "reason": reason});
        let bucket = match name {
            Some("dead") => "dead",
            Some("blocked") => "blocked",
            Some("working") => "working",
            Some("ready") => {
                let waiting = unread(pane);
                if waiting != 0 {
                    let mut with_mail = entry.clone();
                    with_mail["unread"] = json!(waiting);
                    out.get_mut("has_mail").expect("bucket").push(with_mail);
                } else {
                    out.get_mut("ready").expect("bucket").push(entry);
                }
                continue;
            }
            _ => {
                let mut blocked = entry.clone();
                blocked["reason"] = if truthy(&reason) {
                    reason
                } else {
                    json!("unknown state")
                };
                out.get_mut("blocked").expect("bucket").push(blocked);
                continue;
            }
        };
        out.get_mut(bucket).expect("bucket").push(entry);
    }
    json!({
        "ready": out["ready"], "has_mail": out["has_mail"], "blocked": out["blocked"],
        "dead": out["dead"], "working": out["working"],
    })
}

/// `derive_board_items`: standby items DERIVED from the task board, as `(items, coarse)`.
///
/// The hand-written queue is a hand-maintained list, and a hand-maintained list is a future
/// omission, so the board is the primary source and `queue empty` means `no open unblocked work`.
/// A task is dispatchable when it is open, has no block reason (held for an owner decision and
/// waiting on a vendor are both waiting on somebody), has no unfinished dependency, has no live
/// lease, is not in the hand queue, and was not dispatched inside the cooldown.
pub fn derive_board_items(
    tasks: &[Value],
    dispatched_task_ids: &BTreeSet<String>,
    now: Option<f64>,
    coarse_chars: usize,
) -> (Vec<Value>, Vec<String>) {
    let now = now
        .filter(|n| *n != 0.0)
        .unwrap_or_else(crate::clock::now_s);
    let done: BTreeSet<String> = tasks
        .iter()
        .filter(|task| matches!(py_str(task.get("status")).as_str(), "done" | "cancelled"))
        .map(|task| py_str(task.get("id")))
        .collect();
    let mut items: Vec<(i64, f64, Value)> = Vec::new();
    let mut coarse = Vec::new();
    for task in tasks {
        if str_or(task, "status", "") != "open" {
            continue;
        }
        if !str_or(task, "block_reason", "").trim().is_empty() {
            continue;
        }
        let blocked_by_dependency = task
            .get("depends_on")
            .and_then(Value::as_array)
            .is_some_and(|deps| deps.iter().any(|dep| !done.contains(&py_str(Some(dep)))));
        if blocked_by_dependency {
            continue;
        }
        if task.get("leases").is_some_and(truthy) {
            continue; // somebody is on it
        }
        let task_id = py_str(task.get("id"));
        if dispatched_task_ids.contains(&task_id) {
            continue; // dispatched recently; do not stack
        }
        let title = squash(&str_or(task, "title", ""));
        if title.is_empty() {
            continue;
        }
        let priority = priority_order(&py_str(task.get("priority")));
        let created_at = task
            .get("created_at")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(now));
        if title.chars().count() > coarse_chars {
            // An umbrella too broad to act on: a signal to split it, not a reason to keep a
            // parallel hand-written list.
            coarse.push(task_id.clone());
        }
        let sort_key = created_at.as_f64().unwrap_or(0.0);
        items.push((
            priority,
            sort_key,
            json!({"id": format!("board:{task_id}"), "task_id": task_id, "brief": title,
                "lane": null, "derived": true, "priority": priority, "created_at": created_at}),
        ));
    }
    items.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    (items.into_iter().map(|(_, _, item)| item).collect(), coarse)
}

/// `dispatch_text`: what the lane is sent. One line, the task id first so the reply can cite it.
pub fn dispatch_text(item: &Value) -> String {
    let prefix = match item.get("task_id").filter(|v| truthy(v)) {
        Some(task_id) => format!("[standby {}] ", py_str(Some(task_id))),
        None => "[standby] ".to_string(),
    };
    prefix + &squash(&py_str(item.get("brief")))
}

/// `claimed_tasks`: the tasks `pane_id` has CLAIMED (holds a live lease on), in the order to offer
/// them.
///
/// CLAIMED, NOT ASSIGNED (CoS ruling, 2026-10-27). An umbrella or programme task sits
/// assigned-and-open for days, so prompting every named assignee would turn the trigger into a
/// loop in exactly the lanes that must stay quiet. A live lease held by THIS pane means the lane
/// said "I am working on this now". A task another pane has leased is that pane's. Every other
/// exclusion is a reason a nudge would be noise: not in_progress, a declared block reason, or an
/// unfinished dependency. Order: priority, then age.
pub fn claimed_tasks(tasks: &[Value], pane_id: &str) -> Vec<Value> {
    if pane_id.is_empty() {
        return Vec::new();
    }
    let finished: BTreeSet<String> = tasks
        .iter()
        .filter(|task| str_or(task, "status", "") == "done")
        .map(|task| py_str(task.get("id")))
        .collect();
    let mut claimed: Vec<&Value> = Vec::new();
    for task in tasks {
        if str_or(task, "status", "") != "in_progress" {
            continue;
        }
        if !str_or(task, "block_reason", "").trim().is_empty() {
            continue;
        }
        let holds = task
            .get("leases")
            .and_then(Value::as_array)
            .is_some_and(|leases| {
                leases
                    .iter()
                    .any(|lease| lease.is_object() && py_str(lease.get("pane_id")) == pane_id)
            });
        if !holds {
            continue;
        }
        // The task store resolves dependencies outside the current view. A completed
        // prerequisite is absent from an open-task list; absence is not a blocker.
        let blocked = match task.get("blocked_by") {
            Some(blocked_by) => truthy(blocked_by),
            None => task
                .get("depends_on")
                .and_then(Value::as_array)
                .is_some_and(|deps| {
                    deps.iter()
                        .any(|dep| !finished.contains(&py_str(Some(dep))))
                }),
        };
        if blocked {
            continue;
        }
        claimed.push(task);
    }
    claimed.sort_by(|a, b| {
        priority_order(&py_str(a.get("priority")))
            .cmp(&priority_order(&py_str(b.get("priority"))))
            .then(
                number(a.get("created_at"))
                    .partial_cmp(&number(b.get("created_at")))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    claimed.into_iter().cloned().collect()
}

/// `claimed_work`: the first task the lane has claimed, or `None`.
pub fn claimed_work(tasks: &[Value], pane_id: &str) -> Option<Value> {
    claimed_tasks(tasks, pane_id).into_iter().next()
}

/// `turn_end_plan`'s keyword arguments.
#[derive(Debug, Clone, Default)]
pub struct TurnEnd<'a> {
    pub pane_id: &'a str,
    pub tasks: &'a [Value],
    /// `None` means the count could not be established, and an unknown is a skip.
    pub unread: Option<i64>,
    pub last_nudge_at: Option<f64>,
    pub task_nudges: BTreeMap<String, f64>,
    pub now: Option<f64>,
    pub min_gap_s: Option<f64>,
    pub task_cooldown_s: Option<f64>,
}

/// `turn_end_plan`: decide whether a turn end earns a prompt. Pure: everything is injected.
///
/// Returns `{"trigger", "pane_id", "action", "reason", "item"}`; `action` is "nudge" or "skip" and
/// `reason` is a machine-readable code on a skip: `no_pane`, `unread_unknown` (the mailbox could
/// not be read, so "already fed" is unknown), `unread_mail`, `no_open_work` (no CLAIMED task),
/// `cooldown` (this pane was prompted < min_gap_s ago), `task_cooldown` (every task the lane holds
/// was prompted < task_cooldown_s ago).
pub fn turn_end_plan(input: &TurnEnd<'_>) -> Value {
    let now = input.now.unwrap_or_else(crate::clock::now_s);
    let min_gap_s = input.min_gap_s.unwrap_or_else(turn_end_min_gap_s);
    let task_cooldown_s = input
        .task_cooldown_s
        .unwrap_or_else(turn_end_task_cooldown_s);
    let mut plan = json!({"trigger": "turn.end", "pane_id": input.pane_id, "action": "skip",
        "reason": null, "item": null});
    if input.pane_id.is_empty() {
        plan["reason"] = json!("no_pane");
        return plan;
    }
    let Some(unread) = input.unread else {
        plan["reason"] = json!("unread_unknown");
        return plan;
    };
    if unread != 0 {
        plan["reason"] = json!("unread_mail");
        return plan;
    }
    let mut held = claimed_tasks(input.tasks, input.pane_id);
    if held.is_empty() {
        plan["reason"] = json!("no_open_work");
        return plan;
    }
    if let Some(last) = input.last_nudge_at {
        if min_gap_s > 0.0 && now - last < min_gap_s {
            plan["reason"] = json!("cooldown");
            return plan;
        }
    }
    // A task prompted a moment ago is being worked on already; a DIFFERENT task the lane also
    // holds has not been mentioned, so it is still eligible.
    if task_cooldown_s > 0.0 {
        held.retain(|task| {
            !input
                .task_nudges
                .get(&py_str(task.get("id")))
                .is_some_and(|at| now - at < task_cooldown_s)
        });
        if held.is_empty() {
            plan["reason"] = json!("task_cooldown");
            return plan;
        }
    }
    plan["action"] = json!("nudge");
    plan["item"] = held.swap_remove(0);
    plan
}

/// `turn_end_text`: what a lane that ended its turn holding work is sent. The task id leads so
/// the reply can cite it, and the trigger is stated in the text the lane reads.
pub fn turn_end_text(task: &Value) -> String {
    let task_id = py_str(task.get("id"));
    let title = squash(&str_or(task, "title", ""));
    let status = str_or(task, "status", "open");
    let priority = str_or(task, "priority", "low");
    [
        format!("[turn-end {task_id}] {title}"),
        format!("trigger: turn.end - you ended your turn still holding this task ({status}, priority {priority})."),
        "Carry on, or close it out:".to_string(),
        format!("  dunkingsheep task note {task_id} --reason \"<what you did / did NOT verify>\""),
        format!("  dunkingsheep task complete {task_id}"),
    ]
    .join("\n")
}

/// `assigned_open_tasks`: open tasks ASSIGNED to `pane_id`, in board order, excluding blocked or
/// gated ones.
///
/// This is the eligibility the owner's order needs and the CoS ruling withheld: it is used ONLY at
/// the moment a lane goes free, and the caller then takes the work through the atomic claim
/// (task_next), so a lane never receives a prompt for work it has not claimed.
pub fn assigned_open_tasks(tasks: &[Value], pane_id: &str) -> Vec<Value> {
    let mut out: Vec<&Value> = tasks
        .iter()
        .filter(|task| py_str(task.get("status")) == "open")
        .filter(|task| {
            !(task.get("blocked_by").is_some_and(truthy)
                || task.get("block_reason").is_some_and(truthy)
                || task.get("allocation_waiting").is_some_and(truthy))
        })
        .filter(|task| {
            !pane_id.is_empty()
                && task
                    .get("assignees")
                    .and_then(Value::as_array)
                    .is_some_and(|list| list.iter().any(|a| py_str(Some(a)) == pane_id))
        })
        .collect();
    out.sort_by(|a, b| {
        priority_order(&py_str(a.get("priority")))
            .cmp(&priority_order(&py_str(b.get("priority"))))
            .then(
                number(a.get("created_at"))
                    .partial_cmp(&number(b.get("created_at")))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(py_str(a.get("id")).cmp(&py_str(b.get("id"))))
    });
    out.into_iter().cloned().collect()
}

/// `next_job_plan`'s keyword arguments.
#[derive(Debug, Clone, Default)]
pub struct NextJob<'a> {
    pub trigger: &'a str,
    pub pane_id: &'a str,
    pub tasks: &'a [Value],
    /// `None` means the count could not be established; an unknown is a skip.
    pub unread: Option<i64>,
    pub pending_delivery: bool,
    pub last_nudge_at: Option<f64>,
    pub task_nudges: BTreeMap<String, f64>,
    pub now: Option<f64>,
    pub min_gap_s: Option<f64>,
    pub task_cooldown_s: Option<f64>,
    pub dispatch_reason: Option<&'a str>,
}

/// `next_job_plan`: decide what a lane does the moment it goes free. Pure: everything is
/// injected.
///
/// t372: the TRIGGER changes, not the eligibility rule (pi-review-1 i31321). On a task COMPLETION
/// (and on a genuine turn end) a lane looks for its next job by calling the existing atomic claim
/// - it does not get nudged for work it has not claimed.
///
/// Returns `{"trigger", "pane_id", "action", "reason", "task_id"}` where action is "claim" or
/// "skip". Skip reasons are machine-readable and are the contract: `no_pane`; `disabled` /
/// `not_allowed` / `readiness_unknown` / `unavailable_lane` (the operator controls refuse the
/// lane or resolution failed closed, passed in as `dispatch_reason`); `unread_unknown`;
/// `unread_mail`; `pending_delivery`; `no_assigned_work` (clause 5: quiet lanes stay quiet, and
/// this is recorded - reviewer/CoS/VP lanes with nothing assigned must record explicit no-work
/// rather than poll); `cooldown`; `task_cooldown`.
pub fn next_job_plan(input: &NextJob<'_>) -> Value {
    let now = input.now.unwrap_or_else(crate::clock::now_s);
    let min_gap_s = input.min_gap_s.unwrap_or_else(turn_end_min_gap_s);
    let task_cooldown_s = input
        .task_cooldown_s
        .unwrap_or_else(turn_end_task_cooldown_s);
    let mut plan = json!({"trigger": input.trigger, "pane_id": input.pane_id, "action": "skip",
        "reason": null, "task_id": null});
    if input.pane_id.is_empty() {
        plan["reason"] = json!("no_pane");
        return plan;
    }
    if let Some(reason) = input.dispatch_reason.filter(|r| !r.is_empty()) {
        plan["reason"] = json!(reason);
        return plan;
    }
    let Some(unread) = input.unread else {
        plan["reason"] = json!("unread_unknown");
        return plan;
    };
    if unread != 0 {
        plan["reason"] = json!("unread_mail");
        return plan;
    }
    if input.pending_delivery {
        plan["reason"] = json!("pending_delivery");
        return plan;
    }
    let candidates = assigned_open_tasks(input.tasks, input.pane_id);
    if candidates.is_empty() {
        plan["reason"] = json!("no_assigned_work");
        return plan;
    }
    if let Some(last) = input.last_nudge_at {
        if min_gap_s > 0.0 && now - last < min_gap_s {
            plan["reason"] = json!("cooldown");
            return plan;
        }
    }
    let fresh: Vec<&Value> = candidates
        .iter()
        .filter(|task| {
            now - input
                .task_nudges
                .get(&py_str(task.get("id")))
                .copied()
                .unwrap_or(0.0)
                >= task_cooldown_s
        })
        .collect();
    let Some(first) = fresh.first() else {
        plan["reason"] = json!("task_cooldown");
        return plan;
    };
    plan["action"] = json!("claim");
    plan["task_id"] = json!(py_str(first.get("id")));
    plan
}

/// `delivery_retry_plan`: one bounded retry policy, with the dispatch rate floor and submit
/// ceiling. The caller supplies the attempt ceiling; no second submit ladder lives here. A crash
/// after reserving an attempt consumes that attempt, never resets it.
pub fn delivery_retry_plan(
    attempt: i64,
    last_at: Option<f64>,
    now: f64,
    max_attempts: i64,
    min_gap_s: Option<f64>,
) -> Value {
    let gap = min_gap_s.unwrap_or_else(turn_end_min_gap_s);
    if attempt >= max_attempts {
        return json!({"action": "stop", "reason": "retry_exhausted"});
    }
    let next_at = last_at.unwrap_or(0.0) + gap.max(0.0) * 2f64.powi((attempt - 1).max(0) as i32);
    if attempt != 0 && now < next_at {
        return json!({"action": "wait", "reason": "retry_backoff", "next_at": next_at});
    }
    json!({"action": "attempt", "reason": null})
}

#[cfg(test)]
mod tests;
