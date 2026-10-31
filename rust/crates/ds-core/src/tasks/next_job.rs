//! T13 (first slice): the durable handoff records of `dunk_tasks.Tasks`
//! (`record_next_job_handoff`, `next_job_handoffs`, `active_handoffs`, `update_next_job`).
//!
//! NOT in this slice: `advance_next_job` and `dunk_next_job.NextJobs`. `advance` calls
//! `dunk_dispatch.next_job_plan` (dispatch.rs, not in Rust yet) and the allowlist match
//! (`match_allowlist`/`read_panes_locked`, dunk_superusers); `NextJobs` drives `Flock`
//! (inbox, outbox, turns). Those need their own modules first; see the follow-up task.
//!
//! A handoff is the merged `meta` of the newest `next_job_handoff` event per `message_id`
//! plus `task_id`, `pane_id` and `at` from the row, so it is a JSON map like the Python's
//! dict.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::Tasks;
use crate::db;
use crate::error::DunkError;

/// A handoff: the Python dict.
pub type Handoff = Map<String, Value>;

/// `Tasks.NEXT_JOB_EVENT`: NOT `handoff`, which already means a PR handoff.
pub const NEXT_JOB_EVENT: &str = "next_job_handoff";

/// `NON_BLOCKING_HANDOFF_STATES` (t461): the ONLY state that stops blocking, compared
/// EXACTLY. `CANCELLED` or ` cancelled ` still block, because the delivery worker skips
/// only the exact string and the two readers must agree (reviewer t462 [P2]). Everything
/// else, including an unknown or empty state, blocks: it might still become a typed prompt.
pub const NON_BLOCKING_HANDOFF_STATES: [&str; 1] = ["cancelled"];

/// What `record_next_job_handoff` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct HandoffRecord {
    pub task_id: String,
    pub pane_id: String,
    pub from_task: Option<String>,
    pub message_id: Option<String>,
    pub attempt: i64,
    pub reason: Option<String>,
    pub at: f64,
}

fn record_in(
    conn: &Connection,
    task_id: &str,
    pane_id: &str,
    from_task: Option<&str>,
    message_id: Option<&str>,
    attempt: i64,
    reason: Option<&str>,
    now: f64,
    details: &Map<String, Value>,
) -> Result<HandoffRecord, DunkError> {
    let (scope, priority, status): (String, String, String) = conn
        .query_row(
            "SELECT scope, priority, status FROM tasks WHERE id = ?",
            params![task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(db::map_sqlite)?
        .ok_or_else(|| DunkError::Refused(format!("no task with id {task_id:?}")))?;
    let from_task = from_task.filter(|s| !s.is_empty()).map(str::to_string);
    let message_id = message_id.filter(|s| !s.is_empty()).map(str::to_string);
    let mut meta = Map::new();
    meta.insert("from_task".into(), json!(from_task));
    meta.insert("message_id".into(), json!(message_id));
    meta.insert("attempt".into(), json!(attempt));
    meta.insert("reason".into(), json!(reason));
    for (key, value) in details {
        meta.insert(key.clone(), value.clone());
    }
    conn.execute(
        "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, meta) \
         VALUES (?,?,?,?,?,?,?,?,?)",
        params![task_id, NEXT_JOB_EVENT, now, pane_id, scope, priority, None::<String>, status,
                Value::Object(meta).to_string()],
    )
    .map_err(db::map_sqlite)?;
    Ok(HandoffRecord {
        task_id: task_id.to_string(),
        pane_id: pane_id.to_string(),
        from_task,
        message_id,
        attempt,
        reason: reason.map(str::to_string),
        at: now,
    })
}

/// Latest durable state of each handoff, oldest first by first appearance (a Python dict
/// keeps a key's first position when its value is replaced).
fn handoffs_in(conn: &Connection, pane_id: Option<&str>) -> Result<Vec<Handoff>, DunkError> {
    let mut stmt = conn
        .prepare("SELECT task_id, actor, at, meta FROM task_events WHERE event = ? ORDER BY id")
        .map_err(db::map_sqlite)?;
    let rows = stmt
        .query_map(params![NEXT_JOB_EVENT], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(db::map_sqlite)?;
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, Handoff> = Default::default();
    for row in rows {
        let (task_id, actor, at, meta) = row.map_err(db::map_sqlite)?;
        let meta: Handoff = match serde_json::from_str::<Value>(meta.as_deref().unwrap_or("{}")) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let message_id = match meta.get("message_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        if !meta.contains_key("state") {
            continue; // older notes are not executable intents
        }
        if let Some(wanted) = pane_id {
            if actor.as_deref() != Some(wanted) {
                continue;
            }
        }
        let mut handoff = meta;
        handoff.insert("task_id".into(), json!(task_id));
        handoff.insert("pane_id".into(), json!(actor));
        handoff.insert("at".into(), json!(at));
        if !latest.contains_key(&message_id) {
            order.push(message_id.clone());
        }
        latest.insert(message_id, handoff);
    }
    Ok(order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect())
}

impl Tasks {
    /// `record_next_job_handoff`: `pane_id` was handed `task_id` after completing
    /// `from_task` (t372). The SHAPE (`from_task` and `message_id`) is what makes a row a
    /// handoff to the acceptance instrument, so a stray `task note --event handoff` cannot
    /// satisfy clause 4.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_next_job_handoff(
        &self,
        task_id: &str,
        pane_id: &str,
        from_task: Option<&str>,
        message_id: Option<&str>,
        attempt: i64,
        reason: Option<&str>,
        now: Option<f64>,
        details: Option<Map<String, Value>>,
    ) -> Result<HandoffRecord, DunkError> {
        let (task_id, pane_id) = (task_id.to_string(), pane_id.to_string());
        let from_task = from_task.map(str::to_string);
        let message_id = message_id.map(str::to_string);
        let reason = reason.map(str::to_string);
        let now = now.unwrap_or_else(crate::clock::now_s);
        let details = details.unwrap_or_default();
        self.writer
            .call(move |conn| {
                record_in(
                    conn,
                    &task_id,
                    &pane_id,
                    from_task.as_deref(),
                    message_id.as_deref(),
                    attempt,
                    reason.as_deref(),
                    now,
                    &details,
                )
            })
            .await
    }

    /// `next_job_handoffs`: the latest durable state of each handoff, terminal receipts
    /// included.
    pub async fn next_job_handoffs(
        &self,
        pane_id: Option<&str>,
    ) -> Result<Vec<Handoff>, DunkError> {
        let pane = pane_id.map(str::to_string);
        self.writer
            .call(move |conn| handoffs_in(conn, pane.as_deref()))
            .await
    }

    /// `active_handoffs`: handoffs that still count as "a prompt is on its way" for this
    /// pane. A CANCELLED historical intent is excluded (t461): it used to veto live work
    /// forever. Everything else blocks, failing closed on unknown states.
    pub async fn active_handoffs(
        &self,
        pane_id: &str,
        owned_task_ids: &[String],
    ) -> Result<Vec<Handoff>, DunkError> {
        let all = self.next_job_handoffs(Some(pane_id)).await?;
        Ok(all
            .into_iter()
            .filter(|h| {
                let task = h.get("task_id").and_then(Value::as_str).unwrap_or("");
                if !owned_task_ids.iter().any(|o| o == task) {
                    return false;
                }
                // Exact comparison, deliberately: see NON_BLOCKING_HANDOFF_STATES.
                match h.get("state").and_then(Value::as_str) {
                    Some(state) => !NON_BLOCKING_HANDOFF_STATES.contains(&state),
                    None => true,
                }
            })
            .collect())
    }

    /// `update_next_job`: append progress under the writer, preserving the immutable chain.
    /// `expected` makes it a compare-and-swap: a mismatch returns `None` and writes
    /// nothing. The handoff must exist (the Python raises `StopIteration`).
    pub async fn update_next_job(
        &self,
        handoff: &Handoff,
        expected: Option<Map<String, Value>>,
        changes: Map<String, Value>,
    ) -> Result<Option<Handoff>, DunkError> {
        let pane = handoff
            .get("pane_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let message_id = handoff
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.writer
            .call(move |conn| {
                let tx = conn.unchecked_transaction().map_err(db::map_sqlite)?;
                let mut latest = handoffs_in(&tx, Some(&pane))?
                    .into_iter()
                    .find(|h| {
                        h.get("message_id").and_then(Value::as_str) == Some(message_id.as_str())
                    })
                    .ok_or_else(|| {
                        DunkError::Refused(format!(
                            "no next-job handoff {message_id:?} for {pane:?}"
                        ))
                    })?;
                if let Some(expected) = &expected {
                    if expected.iter().any(|(k, v)| latest.get(k) != Some(v)) {
                        tx.commit().map_err(db::map_sqlite)?;
                        return Ok(None);
                    }
                }
                for (key, value) in changes {
                    latest.insert(key, value);
                }
                let text = |k: &str| latest.get(k).and_then(Value::as_str).map(str::to_string);
                let details: Map<String, Value> = latest
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "task_id" | "pane_id" | "at"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                record_in(
                    &tx,
                    text("task_id").as_deref().unwrap_or(""),
                    text("pane_id").as_deref().unwrap_or(""),
                    text("from_task").as_deref(),
                    text("message_id").as_deref(),
                    latest.get("attempt").and_then(Value::as_i64).unwrap_or(0),
                    text("reason").as_deref(),
                    crate::clock::now_s(),
                    &details,
                )?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(Some(latest))
            })
            .await
    }
}

#[cfg(test)]
#[path = "next_job_tests.rs"]
mod next_job_tests;
