//! All-or-nothing batch claim (t3581, child A of t1982): `Tasks::claim_batch`.
//!
//! `claim_next` takes the next eligible task from a queue. A batch takes an explicit, ordered
//! set of task ids, and either every member is claimed or none is. The read, the validation
//! and every write run under ONE `BEGIN IMMEDIATE` transaction, and every member is checked
//! before the first write: a refused batch writes no lease, no status change and no event.
//!
//! A member must pass the tests `claim_next` applies (`claimable`, the kinds filter, the
//! author exclusion, the busy refusal) plus the batch's own: the ids are present and distinct,
//! each is in scope (`global` or the caller's own pane), and no prerequisite OUTSIDE the batch
//! is still outstanding. A prerequisite that is itself a member does not hold its dependant
//! back, because the members are claimed together and their order is the work order (child B).

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use super::{
    claimable, decode_list, default_lease_s, dependency_state_conn, refusal, Candidate,
    ClaimOptions, Refusal, Task, Tasks,
};
use crate::db;
use crate::error::DunkError;

/// What `claim_batch` returned: every member claimed, in the order given, or a refusal.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimBatchOutcome {
    Claimed {
        /// The recorded batch (`task_batches.batch_id`).
        batch_id: i64,
        tasks: Vec<Task>,
        lease_seconds: f64,
        expires_at: f64,
    },
    Refused {
        refusal: Refusal,
    },
}

/// One member as read inside the transaction.
struct Member {
    candidate: Candidate,
    scope: String,
    priority: String,
    created_at: f64,
    kind: String,
    author: Option<String>,
}

fn plain(reason: &'static str, task_ids: Vec<String>) -> Refusal {
    Refusal {
        reason,
        task_ids,
        owned_by: None,
    }
}

impl Tasks {
    /// Atomically claim every task in `ids` for `pane_id`, or none of them (`claim_batch`).
    ///
    /// The order of `ids` is kept: the claimed tasks come back in that order and their
    /// `claimed` events are written in it. An empty batch, a repeated id, an unknown id, a
    /// member another pane owns or holds, an unmet outside prerequisite, or a busy pane
    /// (unless `options.allow_busy`) refuses the whole batch with its reason and writes nothing.
    pub async fn claim_batch(
        &self,
        ids: &[String],
        pane_id: &str,
        options: ClaimOptions,
    ) -> Result<ClaimBatchOutcome, DunkError> {
        if pane_id.is_empty() {
            return Err(DunkError::Refused(
                "claiming a task requires a caller pane id".into(),
            ));
        }
        let lease_s = options
            .lease_seconds
            .filter(|s| *s != 0.0)
            .unwrap_or_else(default_lease_s);
        if lease_s <= 0.0 {
            return Err(DunkError::Refused("lease_seconds must be positive".into()));
        }
        let wanted: Vec<String> = ids.to_vec();
        let pane = pane_id.to_string();
        let now = crate::clock::now_s();
        let claimed = self
            .writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let members = match validate(&tx, &wanted, &pane, &options, now)? {
                    Ok(members) => members,
                    Err(refusal) => {
                        // Nothing has been written: dropping the transaction rolls it back.
                        eprintln!(
                            "task batch claim refused: pane={pane} reason={} tasks={}",
                            refusal.reason,
                            if refusal.task_ids.is_empty() {
                                "-".to_string()
                            } else {
                                refusal.task_ids.join(",")
                            }
                        );
                        return Ok(Err(refusal));
                    }
                };
                for member in &members {
                    write_claim(&tx, member, &pane, lease_s, options.caller.as_ref(), now)?;
                }
                let claimed: Vec<String> = members.iter().map(|m| m.candidate.id.clone()).collect();
                // The membership and its order are recorded in the same transaction as the claim.
                let batch_id = record_batch(&tx, &pane, &claimed, now)?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(Ok((batch_id, claimed)))
            })
            .await?;
        match claimed {
            Ok((batch_id, ids)) => {
                let mut tasks = Vec::with_capacity(ids.len());
                for id in &ids {
                    tasks.push(self.get(id).await?);
                }
                Ok(ClaimBatchOutcome::Claimed {
                    batch_id,
                    tasks,
                    lease_seconds: lease_s,
                    expires_at: now + lease_s,
                })
            }
            Err(refusal) => Ok(ClaimBatchOutcome::Refused { refusal }),
        }
    }
}

/// Every check that can refuse a batch, all of them before the first write. The inner
/// `Err` is a refusal; the outer `Err` is a store error.
fn validate(
    tx: &rusqlite::Transaction<'_>,
    wanted: &[String],
    pane: &str,
    options: &ClaimOptions,
    now: f64,
) -> Result<Result<Vec<Member>, Refusal>, DunkError> {
    if wanted.is_empty() {
        return Ok(Err(plain("empty", Vec::new())));
    }
    // A repeated id would claim one task twice in one batch.
    let mut seen = HashSet::new();
    let mut repeated: Vec<String> = Vec::new();
    for id in wanted {
        if !seen.insert(id.as_str()) && !repeated.contains(id) {
            repeated.push(id.clone());
        }
    }
    if !repeated.is_empty() {
        return Ok(Err(plain("duplicate", repeated)));
    }
    let mut members: Vec<Member> = Vec::with_capacity(wanted.len());
    let mut missing: Vec<String> = Vec::new();
    for id in wanted {
        let found = tx
            .query_row(
                "SELECT scope, priority, created_at, status, kind, author, assignees, multi_assignee FROM tasks WHERE id = ?",
                params![id],
                |row| {
                    let assignees: Option<String> = row.get(6)?;
                    Ok(Member {
                        candidate: Candidate {
                            id: id.clone(),
                            status: row.get(3)?,
                            assignees: decode_list(assignees.as_deref()),
                            multi_assignee: row.get::<_, i64>(7)? != 0,
                            ..Default::default()
                        },
                        scope: row.get(0)?,
                        priority: row.get(1)?,
                        created_at: row.get(2)?,
                        kind: row.get(4)?,
                        author: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(db::map_sqlite)?;
        match found {
            Some(member) => members.push(member),
            None => missing.push(id.clone()),
        }
    }
    if !missing.is_empty() {
        return Ok(Err(plain("missing", missing)));
    }
    // A task scoped to another pane is that pane's private queue, not this caller's to take.
    let foreign: Vec<String> = members
        .iter()
        .filter(|m| m.scope != "global" && m.scope != pane)
        .map(|m| m.candidate.id.clone())
        .collect();
    if !foreign.is_empty() {
        return Ok(Err(plain("scope", foreign)));
    }
    // A member already in an active batch belongs to that batch's claim, not to this one.
    let mut conflicting = Vec::new();
    for member in &members {
        if active_batch_of(tx, &member.candidate.id)?.is_some() {
            conflicting.push(member.candidate.id.clone());
        }
    }
    if !conflicting.is_empty() {
        return Ok(Err(plain("batch_conflict", conflicting)));
    }
    if !options.allow_busy {
        let mut stmt = tx
            .prepare("SELECT DISTINCT lease.task_id FROM task_leases AS lease JOIN tasks AS task ON task.id = lease.task_id WHERE lease.pane_id = ? AND lease.expires_at > ? AND task.status = 'in_progress' ORDER BY lease.task_id")
            .map_err(db::map_sqlite)?;
        let busy: Vec<String> = stmt
            .query_map(params![pane, now], |r| r.get::<_, String>(0))
            .map_err(db::map_sqlite)?
            .collect::<Result<_, _>>()
            .map_err(db::map_sqlite)?;
        drop(stmt);
        if !busy.is_empty() {
            return Ok(Err(plain("busy", busy)));
        }
    }
    // Only prerequisites OUTSIDE the batch hold a member back (see the module doc).
    let member_ids: Vec<String> = members.iter().map(|m| m.candidate.id.clone()).collect();
    for member in members.iter_mut() {
        let id = member.candidate.id.clone();
        let mut stmt = tx
            .prepare("SELECT pane_id FROM task_leases WHERE task_id = ? AND expires_at > ?")
            .map_err(db::map_sqlite)?;
        member.candidate.leased_by = stmt
            .query_map(params![id, now], |r| r.get::<_, String>(0))
            .map_err(db::map_sqlite)?
            .collect::<Result<_, _>>()
            .map_err(db::map_sqlite)?;
        drop(stmt);
        member.candidate.blocked_by = dependency_state_conn(tx, &id)?
            .1
            .blocked_by
            .into_iter()
            .filter(|dep| !member_ids.contains(dep))
            .collect();
        member.candidate.allocation_waiting =
            allocation_waiting(tx, &id, &member.candidate.assignees)?;
    }
    let failing: Vec<Candidate> = members
        .iter()
        .filter(|m| !eligible(m, pane, options))
        .map(|m| m.candidate.clone())
        .collect();
    if !failing.is_empty() {
        return Ok(Err(refusal(&failing, Some(pane))));
    }
    Ok(Ok(members))
}

/// The per-member rules `claim_next` applies to its chosen row.
fn eligible(member: &Member, pane: &str, options: &ClaimOptions) -> bool {
    member.candidate.blocked_by.is_empty()
        && options
            .kinds
            .as_ref()
            .is_none_or(|kinds| kinds.is_empty() || kinds.contains(&member.kind))
        && !options
            .exclude_author
            .as_ref()
            .is_some_and(|who| member.author.as_ref() == Some(who))
        && claimable(&member.candidate, Some(pane))
}

/// Whether the member's ready allocation offer is still waiting on its current assignees. The
/// allocation table may not exist yet (the Python tolerates that), so a missing table is `false`.
fn allocation_waiting(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    assignees: &[String],
) -> Result<bool, DunkError> {
    let offer: Option<String> = match tx
        .query_row(
            "SELECT owners FROM allocation_offers WHERE task_id = ? AND state = 'ready'",
            params![id],
            |r| r.get(0),
        )
        .optional()
    {
        Ok(found) => found,
        Err(error) if error.to_string().contains("no such table") => None,
        Err(error) => return Err(db::map_sqlite(error)),
    };
    Ok(match offer {
        Some(owners) => {
            let mut mine = assignees.to_vec();
            mine.sort();
            mine == decode_list(Some(&owners))
        }
        None => false,
    })
}

/// The writes for one member, the same ones `claim_next` makes: the lease, the status and
/// attempt, and the `claimed` event.
fn write_claim(
    tx: &rusqlite::Transaction<'_>,
    member: &Member,
    pane: &str,
    lease_s: f64,
    caller: Option<&(i64, Option<String>, Option<String>)>,
    now: f64,
) -> Result<(), DunkError> {
    let id = member.candidate.id.as_str();
    let mut assignees = member.candidate.assignees.clone();
    let added = !assignees.iter().any(|a| a == pane);
    if added {
        assignees.push(pane.to_string());
    }
    let (pid, start, name) = match caller {
        Some((p, s, n)) => (Some(*p), s.clone(), n.clone()),
        None => (None, None, None),
    };
    tx.execute(
        "INSERT OR REPLACE INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at, caller_pid, caller_start, caller_name, assignee_added) VALUES (?,?,?,?,?,?,?,?,?)",
        params![id, pane, now, now, now + lease_s, pid, start, name, added as i64],
    )
    .map_err(db::map_sqlite)?;
    tx.execute(
        "UPDATE tasks SET status = 'in_progress', assignees = ?, attempts = attempts + 1, updated_at = ?, block_reason = NULL WHERE id = ?",
        params![json!(assignees).to_string(), now, id],
    )
    .map_err(db::map_sqlite)?;
    tx.execute(
        "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms, meta) VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            id,
            "claimed",
            now,
            pane,
            member.scope,
            member.priority,
            member.candidate.status,
            "in_progress",
            ((now - member.created_at) * 1000.0) as i64,
            json!({
                "lease_seconds": crate::github::python_float_json_value(lease_s)
                    .expect("SQLite accepted task lease value"),
                "caller_pid": pid,
                "caller_name": name,
            })
            .to_string(),
        ],
    )
    .map_err(db::map_sqlite)?;
    Ok(())
}

/// Record the batch and its ordered members (t3582). Runs inside the claim's transaction, so a
/// refused batch records nothing and a claimed batch is always recorded.
fn record_batch(
    tx: &rusqlite::Transaction<'_>,
    pane: &str,
    members: &[String],
    now: f64,
) -> Result<i64, DunkError> {
    tx.execute(
        "INSERT INTO task_batches (pane_id, created_at, state) VALUES (?, ?, 'active')",
        params![pane, now],
    )
    .map_err(db::map_sqlite)?;
    let batch_id = tx.last_insert_rowid();
    for (position, id) in members.iter().enumerate() {
        tx.execute(
            "INSERT INTO task_batch_members (batch_id, position, task_id, active) VALUES (?, ?, ?, 1)",
            params![batch_id, position as i64, id],
        )
        .map_err(db::map_sqlite)?;
    }
    Ok(batch_id)
}

/// The active batch a task belongs to, if any.
pub(super) fn active_batch_of(conn: &Connection, task_id: &str) -> Result<Option<i64>, DunkError> {
    conn.query_row(
        "SELECT batch_id FROM task_batch_members WHERE task_id = ? AND active = 1",
        params![task_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(db::map_sqlite)
}

/// The active members of `batch_id` that `pane` holds a live lease on, in batch order.
pub(super) fn batch_members_held_by(
    conn: &Connection,
    batch_id: i64,
    pane: &str,
) -> Result<Vec<String>, DunkError> {
    let mut stmt = conn
        .prepare(
            "SELECT m.task_id FROM task_batch_members AS m \
             JOIN task_leases AS l ON l.task_id = m.task_id AND l.pane_id = ? \
             WHERE m.batch_id = ? AND m.active = 1 ORDER BY m.position",
        )
        .map_err(db::map_sqlite)?;
    let rows = stmt
        .query_map(params![pane, batch_id], |row| row.get::<_, String>(0))
        .map_err(db::map_sqlite)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
}

/// The tasks whose leases a heartbeat on `id` extends: every member of its active batch that
/// `pane` holds, or `id` alone when it is not in a batch (t3582: one deadline per batch).
pub(super) fn heartbeat_targets(
    conn: &Connection,
    id: &str,
    pane: &str,
) -> Result<Vec<String>, DunkError> {
    match active_batch_of(conn, id)? {
        Some(batch) => batch_members_held_by(conn, batch, pane),
        None => Ok(vec![id.to_string()]),
    }
}

/// End a batch. Its rows stay as the record; its members are free to be claimed again.
pub(super) fn end_batch(conn: &Connection, batch_id: i64) -> Result<(), DunkError> {
    conn.execute(
        "UPDATE task_batches SET state = 'ended' WHERE batch_id = ?",
        params![batch_id],
    )
    .map_err(db::map_sqlite)?;
    conn.execute(
        "UPDATE task_batch_members SET active = 0 WHERE batch_id = ?",
        params![batch_id],
    )
    .map_err(db::map_sqlite)?;
    Ok(())
}

/// The first earlier member of `task_id`'s batch that is not done, if any. An internal
/// prerequisite is satisfied only by its completion: cancelling it does not satisfy it.
pub(super) fn unfinished_earlier_member(
    conn: &Connection,
    batch_id: i64,
    task_id: &str,
) -> Result<Option<String>, DunkError> {
    conn.query_row(
        "SELECT m.task_id FROM task_batch_members AS m JOIN tasks AS t ON t.id = m.task_id \
         WHERE m.batch_id = ?1 AND m.active = 1 AND t.status != 'done' \
         AND m.position < (SELECT position FROM task_batch_members WHERE batch_id = ?1 AND task_id = ?2) \
         ORDER BY m.position LIMIT 1",
        params![batch_id, task_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(db::map_sqlite)
}

/// Whether every active member of the batch is done, so the batch has finished.
pub(super) fn batch_settled(conn: &Connection, batch_id: i64) -> Result<bool, DunkError> {
    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_batch_members AS m JOIN tasks AS t ON t.id = m.task_id \
             WHERE m.batch_id = ? AND m.active = 1 AND t.status != 'done'",
            params![batch_id],
            |row| row.get(0),
        )
        .map_err(db::map_sqlite)?;
    Ok(open == 0)
}

#[cfg(test)]
#[path = "claim_batch_tests.rs"]
mod claim_batch_tests;

#[cfg(test)]
#[path = "claim_batch_lifecycle_tests.rs"]
mod claim_batch_lifecycle_tests;
