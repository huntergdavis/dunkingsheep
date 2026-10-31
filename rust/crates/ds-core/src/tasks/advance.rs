//! T13 (slice 2): `Tasks.advance_next_job` (dunk_tasks.py:1018) - complete A, choose and
//! claim B, journal the handoff, ALL in one `BEGIN IMMEDIATE` transaction.
//!
//! Two collaborators are not ported yet and are INJECTED, so this slice is testable and
//! the real ones plug in later without changing the transaction:
//!
//! * the planner (`dunk_dispatch.next_job_plan`; dispatch.rs is not in Rust) is a
//!   [`Planner`] closure over a [`PlanInput`];
//! * the operator allowlist (`read_panes_locked` / `match_allowlist` /
//!   `locked_allowlist`, dunk_superusers) is an [`Allowlist`] whose `with_locked` holds the
//!   file lock for the duration of the callback.
//!
//! No socket or terminal I/O happens inside the transaction. Ownership is rechecked on the
//! freshly-read row INSIDE the transaction (t423 P2), and for a completion the allowlist is
//! re-read under its lock and the lock is held THROUGH the commit (LATE authorization,
//! t423/t430): a refusal rolls status, events and handoff back together.

use std::sync::Arc;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Map, Value};

use super::next_job::{handoffs_in, record_in, NON_BLOCKING_HANDOFF_STATES};
use super::{decode_list, decorate_conn, default_lease_s, leases_for_conn, Decorated, Task, Tasks};
use crate::db;
use crate::error::DunkError;

/// What `next_job_plan` is given (its keyword arguments).
pub struct PlanInput<'a> {
    pub trigger: &'a str,
    pub pane_id: &'a str,
    pub tasks: &'a [Decorated],
    pub unread: i64,
    pub pending_delivery: bool,
    pub now: f64,
    pub dispatch_reason: Option<&'a str>,
}

/// The plan dict: at least `action` (`claim`/`skip`/...), and `task_id` for a claim.
pub type Plan = Map<String, Value>;

/// `dunk_dispatch.next_job_plan`, injected.
pub type Planner = dyn Fn(&PlanInput<'_>) -> Plan + Send + Sync;

/// The superusers allowlist. `with_locked` holds the lock while `body` runs and hands it a
/// matcher `(pane_id, pane_info) -> allowed`. An `Err` (corrupt or unreadable allowlist)
/// grants nothing.
pub trait Allowlist: Send + Sync {
    fn with_locked(
        &self,
        body: &mut dyn FnMut(&dyn Fn(&str, Option<&Value>) -> bool) -> Result<(), DunkError>,
    ) -> Result<(), DunkError>;
}

/// `operator_proof`: the claim that `pane_id` is an operator.
#[derive(Clone)]
pub struct OperatorProof {
    pub pane_id: String,
    pub pane_info: Option<Value>,
    pub allowlist: Arc<dyn Allowlist>,
}

/// `advance_next_job`'s keyword arguments.
#[derive(Default, Clone)]
pub struct AdvanceOptions {
    pub from_task: Option<String>,
    pub caller: Map<String, Value>,
    pub unread: i64,
    pub pending_delivery: bool,
    pub dispatch_reason: Option<String>,
    pub operator_proof: Option<OperatorProof>,
}

/// What `advance_next_job` returns: the plan dict, plus `task` for a completion.
#[derive(Debug, Clone)]
pub struct Advance {
    pub plan: Plan,
    pub task: Option<Task>,
}

fn refuse<T>(text: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(text))
}

/// `uuid4().hex`: 32 random hex digits (no `uuid` crate is approved).
fn random_hex() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes = nanos.to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn require(conn: &Connection, id: &str) -> Result<Task, DunkError> {
    let row = conn
        .query_row(
            "SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at FROM tasks WHERE id = ?",
            params![id],
            super::row_to_task,
        )
        .optional()
        .map_err(db::map_sqlite)?;
    row.ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))
}

fn owners_of(conn: &Connection, task: &Task, now: f64) -> Result<Vec<String>, DunkError> {
    let mut owners = decode_list(Some(&task.assignees));
    for lease in leases_for_conn(conn, &task.id, now)? {
        if !lease.pane_id.is_empty() && !owners.contains(&lease.pane_id) {
            owners.push(lease.pane_id);
        }
    }
    Ok(owners)
}

fn owner_text(owners: &[String]) -> String {
    let mut sorted = owners.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        "no one recorded".into()
    } else {
        sorted.join(", ")
    }
}

/// `_claim_locked`: the lease, the status, the attempt and the event.
fn claim_locked(
    tx: &Transaction<'_>,
    chosen: &Decorated,
    pane: &str,
    lease_s: f64,
    caller: &Map<String, Value>,
    now: f64,
) -> Result<(), DunkError> {
    let mut assignees = chosen.assignees.clone();
    let added = !assignees.iter().any(|a| a == pane);
    if added {
        assignees.push(pane.to_string());
    }
    let pid = caller.get("pid").and_then(Value::as_i64);
    let start = caller.get("start").and_then(Value::as_str);
    let name = caller.get("name").and_then(Value::as_str);
    tx.execute(
        "INSERT OR REPLACE INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at, caller_pid, caller_start, caller_name, assignee_added) VALUES (?,?,?,?,?,?,?,?,?)",
        params![chosen.task.id, pane, now, now, now + lease_s, pid, start, name, added as i64],
    )
    .map_err(db::map_sqlite)?;
    tx.execute(
        "UPDATE tasks SET status = 'in_progress', assignees = ?, attempts = attempts + 1, updated_at = ?, block_reason = NULL WHERE id = ?",
        params![json!(assignees).to_string(), now, chosen.task.id],
    )
    .map_err(db::map_sqlite)?;
    tx.execute(
        "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms, meta) VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![chosen.task.id, "claimed", now, pane, chosen.task.scope, chosen.task.priority,
                chosen.task.status, "in_progress", ((now - chosen.task.created_at) * 1000.0) as i64,
                json!({"lease_seconds": lease_s, "caller_pid": pid, "caller_name": name}).to_string()],
    )
    .map_err(db::map_sqlite)?;
    Ok(())
}

fn active_in(
    conn: &Connection,
    pane: &str,
    owned: &[String],
) -> Result<Vec<Map<String, Value>>, DunkError> {
    Ok(handoffs_in(conn, Some(pane))?
        .into_iter()
        .filter(|h| {
            let task = h.get("task_id").and_then(Value::as_str).unwrap_or("");
            owned.iter().any(|o| o == task)
                && match h.get("state").and_then(Value::as_str) {
                    Some(state) => !NON_BLOCKING_HANDOFF_STATES.contains(&state),
                    None => true,
                }
        })
        .collect())
}

fn advance_in(
    conn: &mut Connection,
    pane: &str,
    trigger: &str,
    opts: &AdvanceOptions,
    planner: &Planner,
) -> Result<Advance, DunkError> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db::map_sqlite)?;
    let now = crate::clock::now_s();
    let mut from_task = opts.from_task.clone().filter(|s| !s.is_empty());
    let completion: Option<f64>;
    if trigger == "completion" {
        let id = from_task.clone().unwrap_or_default();
        let row = require(&tx, &id)?;
        if row.status == "done" {
            tx.commit().map_err(db::map_sqlite)?;
            let mut plan = Plan::new();
            plan.insert("action".into(), json!("skip"));
            plan.insert("reason".into(), json!("already_completed"));
            return Ok(Advance {
                plan,
                task: Some(row),
            });
        }
        // P2 (t423 REVISE): ownership is rechecked on the freshly-read row INSIDE this
        // transaction, and operator privilege on a fresh allowlist read under its lock.
        let owners = owners_of(&tx, &row, now)?;
        let mut fresh_operator = false;
        if let Some(proof) = opts
            .operator_proof
            .as_ref()
            .filter(|p| p.pane_id == pane && !p.pane_id.is_empty())
        {
            let mut allowed = false;
            let read = proof.allowlist.with_locked(&mut |matches| {
                allowed = matches(pane, proof.pane_info.as_ref());
                Ok(())
            });
            // A corrupt allowlist grants nothing.
            fresh_operator = read.is_ok() && allowed;
        }
        if !owners.iter().any(|o| o == pane) && !fresh_operator {
            return refuse(format!(
                "task {id} is not owned by {pane} (owners: {}); completion refused with no effects",
                owner_text(&owners)
            ));
        }
        tx.execute(
            "UPDATE tasks SET status='done', completed_at=?, completed_by=?, updated_at=?, cancelled_at=NULL, block_reason=NULL WHERE id=?",
            params![now, pane, now, id],
        )
        .map_err(db::map_sqlite)?;
        // t891: completion arrives here, not through update(), so the lease is released
        // here too, or the lane that just finished stays "busy" for the lease's length.
        let released = tx
            .execute("DELETE FROM task_leases WHERE task_id = ?", params![id])
            .map_err(db::map_sqlite)?;
        if released > 0 {
            tx.execute(
                "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_to, meta) VALUES (?,?,?,?,?,?,?,?)",
                params![id, "lease_released", now, pane, row.scope, row.priority, "done",
                        json!({"reason": "task completed", "leases_removed": released}).to_string()],
            )
            .map_err(db::map_sqlite)?;
        }
        tx.execute(
            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms) VALUES (?,?,?,?,?,?,?,?,?)",
            params![id, "completed", now, pane, row.scope, row.priority, row.status, "done",
                    ((now - row.created_at) * 1000.0) as i64],
        )
        .map_err(db::map_sqlite)?;
        completion = Some(now);
    } else if let Some(id) = &from_task {
        let completed_at: Option<f64> = tx
            .query_row(
                "SELECT completed_at FROM tasks WHERE id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db::map_sqlite)?
            .ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
        completion = completed_at;
    } else {
        let previous: Option<(String, f64)> = tx
            .query_row(
                "SELECT task_id, at FROM task_events WHERE event='completed' AND actor=? ORDER BY id DESC LIMIT 1",
                params![pane],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db::map_sqlite)?;
        match previous {
            Some((task, at)) => {
                from_task = Some(task);
                completion = Some(at);
            }
            None => completion = None,
        }
    }

    // A concurrent end/restart sees the committed handoff and cannot advance past it.
    let ids: Vec<String> = {
        let mut stmt = tx
            .prepare("SELECT id FROM tasks WHERE scope IN (?, ?) AND status IN ('open','in_progress') ORDER BY created_at, id")
            .map_err(db::map_sqlite)?;
        let rows = stmt
            .query_map(params!["global", pane], |r| r.get::<_, String>(0))
            .map_err(db::map_sqlite)?;
        rows.collect::<Result<_, _>>().map_err(db::map_sqlite)?
    };
    let mut tasks = Vec::new();
    for id in &ids {
        tasks.push(decorate_conn(&tx, id, Some(pane), now)?);
    }
    let owned: Vec<String> = tasks
        .iter()
        .filter(|t| t.assignees.iter().any(|a| a == pane))
        .map(|t| t.task.id.clone())
        .collect();
    let active = active_in(&tx, pane, &owned)?;
    let held = tasks
        .iter()
        .any(|t| t.task.status == "in_progress" && t.claimed_by.iter().any(|c| c == pane));
    let mut plan = planner(&PlanInput {
        trigger,
        pane_id: pane,
        tasks: &tasks,
        unread: opts.unread,
        pending_delivery: opts.pending_delivery || !active.is_empty() || held,
        now,
        dispatch_reason: opts.dispatch_reason.as_deref(),
    });
    plan.insert("from_task".into(), json!(from_task));
    plan.insert("completion_at".into(), json!(completion));
    plan.insert("caller".into(), Value::Object(opts.caller.clone()));
    if plan.get("action").and_then(Value::as_str) == Some("claim") {
        let wanted = plan.get("task_id").and_then(Value::as_str).unwrap_or("");
        let chosen = tasks
            .iter()
            .find(|t| t.task.id == wanted)
            .ok_or_else(|| DunkError::Refused(format!("planner chose unknown task {wanted:?}")))?;
        // Revalidate the policy choice with the existing claim rules.
        if !super::claimable_for(chosen, pane) {
            plan.insert("action".into(), json!("skip"));
            plan.insert("reason".into(), json!("claim_unavailable"));
        } else {
            claim_locked(
                &tx,
                chosen,
                pane,
                default_lease_s(),
                &opts.caller,
                crate::clock::now_s(),
            )?;
            let message_id = format!("nj-{}", random_hex());
            let mut handoff = Map::new();
            handoff.insert("from_task".into(), json!(from_task));
            handoff.insert("completion_at".into(), json!(completion));
            handoff.insert("message_id".into(), json!(message_id));
            handoff.insert("attempt".into(), json!(0));
            handoff.insert("reason".into(), Value::Null);
            handoff.insert("state".into(), json!("pending"));
            handoff.insert("trigger".into(), json!(trigger));
            handoff.insert("caller".into(), Value::Object(opts.caller.clone()));
            record_in(
                &tx,
                &chosen.task.id,
                pane,
                from_task.as_deref(),
                Some(&message_id),
                0,
                None,
                crate::clock::now_s(),
                &handoff,
            )?;
            handoff.insert("task_id".into(), json!(chosen.task.id));
            handoff.insert("pane_id".into(), json!(pane));
            plan.insert("handoff".into(), Value::Object(handoff));
        }
    }
    let decision: Map<String, Value> = plan
        .iter()
        .filter(|(k, _)| k.as_str() != "handoff")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    tx.execute(
        "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
        params![
            from_task.clone().unwrap_or_default(),
            "next_job_decision",
            now,
            pane,
            Value::Object(decision).to_string()
        ],
    )
    .map_err(db::map_sqlite)?;

    // LATE authorization, held through commit (t423/t430): re-read ownership plus the
    // allowlist and keep the lock until the commit, so no writer lands between
    // verification and the durable decision. Refusal drops `tx`: zero effects.
    let late = opts
        .operator_proof
        .as_ref()
        .filter(|_| trigger == "completion" && !pane.is_empty() && from_task.is_some());
    match late {
        Some(proof) => {
            let id = from_task.clone().unwrap_or_default();
            let mut tx = Some(tx);
            let mut refusal: Option<DunkError> = None;
            let locked = proof.allowlist.with_locked(&mut |matches| {
                let held = tx.take().expect("transaction present");
                let outcome = (|| -> Result<(), DunkError> {
                    let late_row = require(&held, &id)?;
                    let owners = owners_of(&held, &late_row, crate::clock::now_s())?;
                    if !owners.iter().any(|o| o == pane) && !matches(pane, proof.pane_info.as_ref()) {
                        return refuse(format!(
                            "task {id} authorization changed during completion (owners now: {}); completion refused with no effects",
                            owner_text(&owners)
                        ));
                    }
                    Ok(())
                })();
                match outcome {
                    Ok(()) => held.commit().map_err(db::map_sqlite),
                    Err(error) => {
                        refusal = Some(error);
                        drop(held); // rollback
                        Ok(())
                    }
                }
            });
            if let Some(error) = refusal {
                return Err(error);
            }
            // An unreadable allowlist is not a grant; ownership still decides.
            if let Some(held) = tx.take() {
                locked?;
                held.commit().map_err(db::map_sqlite)?;
            } else {
                locked?;
            }
        }
        None => tx.commit().map_err(db::map_sqlite)?,
    }
    Ok(Advance { plan, task: None })
}

impl Tasks {
    /// `advance_next_job`: complete A, choose/claim B and journal its inbox id in ONE
    /// transaction. Every trigger uses the same decision while holding SQLite's writer
    /// lock; the journal is an outbox intent (inbox materialisation is idempotent).
    pub async fn advance_next_job(
        &self,
        pane_id: &str,
        trigger: &str,
        options: AdvanceOptions,
        planner: Arc<Planner>,
    ) -> Result<Advance, DunkError> {
        let is_completion = trigger == "completion";
        let (pane, trigger) = (pane_id.to_string(), trigger.to_string());
        let from = options.from_task.clone();
        let mut advance = self
            .writer
            .call(move |conn| advance_in(conn, &pane, &trigger, &options, planner.as_ref()))
            .await?;
        // The Python returns `plan["task"] = self.get(from_task)` after the commit.
        if is_completion && advance.task.is_none() {
            if let Some(id) = from {
                advance.task = Some(self.get(&id).await?);
            }
        }
        Ok(advance)
    }
}

#[cfg(test)]
#[path = "advance_tests.rs"]
mod tests;
