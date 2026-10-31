//! Claiming (T12), ported from `dunk_tasks.py`: leases, the claimability rules and the
//! refusal reasons. Slice 1 of T12: `leases_for`, `leases_for_pane`, `_busy_task_ids`,
//! `heartbeat`, and the two pure decisions `_claimable` and `_refusal`.
//!
//! Slice 2: `claim_next` / `_claim_locked` (one immediate transaction, kinds, exclude_author,
//! the busy refusal, scope 'mine').
//!
//! Slice 3: `release`, `reap` and `_after_lease_change`.
//!
//! NOT PORTED yet: the per-viewer `claimable` / `eligible` fields of `_decorate`.

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use super::{
    decode_list, default_lease_s, dependency_state_conn, order_key, validate_scope, Lease, Task,
    Tasks,
};
use crate::db;
use crate::error::DunkError;

/// What `_claimable` and `_refusal` read from a decorated task.
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub id: String,
    pub status: String,
    pub assignees: Vec<String>,
    pub multi_assignee: bool,
    pub allocation_waiting: bool,
    /// Panes holding a live lease.
    pub leased_by: Vec<String>,
    pub blocked_by: Vec<String>,
}

/// `_claimable`: whether `pane_id` may take this task right now.
pub fn claimable(task: &Candidate, pane_id: Option<&str>) -> bool {
    if task.allocation_waiting {
        return false;
    }
    if let Some(pane) = pane_id {
        if task.leased_by.iter().any(|holder| holder == pane) {
            return false; // already holds this one
        }
    }
    if !task.leased_by.is_empty() {
        return task.multi_assignee;
    }
    let assigned_to_me = pane_id.is_some_and(|pane| task.assignees.iter().any(|a| a == pane));
    match task.status.as_str() {
        // F2: a named assignee can pull its own task.
        "open" => task.assignees.is_empty() || task.multi_assignee || assigned_to_me,
        "in_progress" => task.multi_assignee || assigned_to_me,
        _ => false,
    }
}

/// Why nothing was claimable, as a reason plus the evidence for it (`_refusal`).
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub reason: &'static str,
    pub task_ids: Vec<String>,
    pub owned_by: Option<Vec<String>>,
}

pub fn refusal(candidates: &[Candidate], pane_id: Option<&str>) -> Refusal {
    let plain = |reason, task_ids| Refusal {
        reason,
        task_ids,
        owned_by: None,
    };
    if candidates.is_empty() {
        return plain("empty", Vec::new());
    }
    let blocked: Vec<String> = candidates
        .iter()
        .filter(|t| !t.blocked_by.is_empty())
        .map(|t| t.id.clone())
        .collect();
    if blocked.len() == candidates.len() {
        return plain("blocked", blocked);
    }
    let owned: Vec<&Candidate> = candidates
        .iter()
        .filter(|t| {
            !t.assignees.is_empty()
                && !t.multi_assignee
                && !pane_id.is_some_and(|pane| t.assignees.iter().any(|a| a == pane))
        })
        .collect();
    if !owned.is_empty() {
        let mut names: Vec<String> = owned
            .iter()
            .flat_map(|t| t.assignees.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        return Refusal {
            reason: "owned_elsewhere",
            task_ids: owned.iter().map(|t| t.id.clone()).collect(),
            owned_by: Some(names),
        };
    }
    let leased: Vec<String> = candidates
        .iter()
        .filter(|t| !t.leased_by.is_empty())
        .map(|t| t.id.clone())
        .collect();
    if !leased.is_empty() {
        return plain("leased", leased);
    }
    plain(
        "not_claimable",
        candidates.iter().map(|t| t.id.clone()).collect(),
    )
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        task_id: row.get(0)?,
        pane_id: row.get(1)?,
        claimed_at: row.get(2)?,
        heartbeat_at: row.get(3)?,
        expires_at: row.get(4)?,
    })
}

const LEASE_COLUMNS: &str = "task_id, pane_id, claimed_at, heartbeat_at, expires_at";

impl Tasks {
    /// Live leases held by a pane.
    pub async fn leases_for_pane(
        &self,
        pane_id: &str,
        now: Option<f64>,
    ) -> Result<Vec<Lease>, DunkError> {
        let pane = pane_id.to_string();
        let now = now.unwrap_or_else(crate::clock::now_s);
        self.writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare(&format!("SELECT {LEASE_COLUMNS} FROM task_leases WHERE pane_id = ? AND expires_at > ?"))
                    .map_err(db::map_sqlite)?;
                let rows = stmt.query_map(params![pane, now], lease_from_row).map_err(db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
            })
            .await
    }

    /// Ids of in-progress tasks this pane holds a live lease on (`_busy_task_ids`).
    pub async fn busy_task_ids(&self, pane_id: &str, now: f64) -> Result<Vec<String>, DunkError> {
        let pane = pane_id.to_string();
        self.writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT lease.task_id FROM task_leases AS lease \
                         JOIN tasks AS task ON task.id = lease.task_id \
                         WHERE lease.pane_id = ? AND lease.expires_at > ? \
                         AND task.status = 'in_progress' ORDER BY lease.task_id",
                    )
                    .map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map(params![pane, now], |row| row.get::<_, String>(0))
                    .map_err(db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
            })
            .await
    }

    /// Extend a live lease; refused when the caller holds none. `caller` is the pid,
    /// start and name the pane's agent runs as, refreshed too: the agent may have been
    /// restarted and liveness tracks the current process.
    pub async fn heartbeat(
        &self,
        id: &str,
        pane_id: &str,
        lease_seconds: Option<f64>,
        caller: Option<(i64, Option<String>, Option<String>)>,
    ) -> Result<(f64, f64), DunkError> {
        let lease_s = lease_seconds
            .filter(|s| *s != 0.0)
            .unwrap_or_else(default_lease_s);
        let (id, pane) = (id.to_string(), pane_id.to_string());
        let now = crate::clock::now_s();
        let changed = self
            .writer
            .call({
                let (id, pane) = (id.clone(), pane.clone());
                move |conn| match caller {
                    Some((pid, start, name)) => conn.execute(
                        "UPDATE task_leases SET heartbeat_at = ?, expires_at = ?, caller_pid = ?, \
                         caller_start = ?, caller_name = ? WHERE task_id = ? AND pane_id = ?",
                        params![now, now + lease_s, pid, start, name, id, pane],
                    ),
                    None => conn.execute(
                        "UPDATE task_leases SET heartbeat_at = ?, expires_at = ? WHERE task_id = ? AND pane_id = ?",
                        params![now, now + lease_s, id, pane],
                    ),
                }
                .map_err(db::map_sqlite)
            })
            .await?;
        if changed == 0 {
            return Err(DunkError::Refused(format!(
                "no active lease for {pane} on {id}; claim it first"
            )));
        }
        Ok((now + lease_s, lease_s))
    }
}

/// What `claim_next` returned: a claimed task, or a refusal with its reason.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    Claimed {
        task: Box<Task>,
        scope: String,
        lease_seconds: f64,
        expires_at: f64,
    },
    Refused {
        scope: String,
        refusal: Refusal,
    },
}

/// `claim_next`'s options (the Python's keyword arguments).
#[derive(Debug, Clone, Default)]
pub struct ClaimOptions {
    pub lease_seconds: Option<f64>,
    pub caller: Option<(i64, Option<String>, Option<String>)>,
    pub allow_busy: bool,
    pub kinds: Option<Vec<String>>,
    pub exclude_author: Option<String>,
}

struct Row {
    candidate: Candidate,
    scope: String,
    priority: String,
    created_at: f64,
    kind: String,
    author: Option<String>,
}

impl Tasks {
    /// Atomically claim the next eligible task in `scope` (`claim_next`).
    ///
    /// The whole read-select-update runs under `BEGIN IMMEDIATE`: SQLite serialises
    /// writers, so two agents polling at once cannot both win the same exclusive task. A
    /// pane already holding in-progress work is REFUSED a second task unless
    /// `allow_busy` (t145); every refusal carries a reason, so "nothing to do" and "you
    /// were refused" never look alike.
    pub async fn claim_next(
        &self,
        scope: &str,
        pane_id: &str,
        options: ClaimOptions,
    ) -> Result<ClaimOutcome, DunkError> {
        let mine_only = scope.trim() == "mine";
        if pane_id.is_empty() {
            return Err(DunkError::Refused(if mine_only {
                "scope 'mine' requires a known caller pane".into()
            } else {
                "claiming a task requires a caller pane id".into()
            }));
        }
        let target = if mine_only {
            "mine".to_string()
        } else {
            validate_scope(scope)?
        };
        let lease_s = options
            .lease_seconds
            .filter(|s| *s != 0.0)
            .unwrap_or_else(default_lease_s);
        if lease_s <= 0.0 {
            return Err(DunkError::Refused("lease_seconds must be positive".into()));
        }
        let pane = pane_id.to_string();
        let now = crate::clock::now_s();
        let outcome_target = target.clone();
        let chosen_id = self
            .writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let (sql, bind): (&str, Vec<String>) = if mine_only {
                    ("SELECT id, scope, priority, created_at, status, kind, author, assignees, multi_assignee FROM tasks WHERE scope IN (?, ?) AND status IN ('open', 'in_progress') ORDER BY created_at, id",
                     vec!["global".into(), pane.clone()])
                } else {
                    ("SELECT id, scope, priority, created_at, status, kind, author, assignees, multi_assignee FROM tasks WHERE scope = ? AND status IN ('open', 'in_progress') ORDER BY created_at, id",
                     vec![target.clone()])
                };
                let mut rows: Vec<Row> = Vec::new();
                {
                    let mut stmt = tx.prepare(sql).map_err(db::map_sqlite)?;
                    let found = stmt
                        .query_map(rusqlite::params_from_iter(bind.iter()), |row| {
                            let assignees: Option<String> = row.get(7)?;
                            Ok(Row {
                                candidate: Candidate {
                                    id: row.get(0)?,
                                    status: row.get(4)?,
                                    assignees: decode_list(assignees.as_deref()),
                                    multi_assignee: row.get::<_, i64>(8)? != 0,
                                    ..Default::default()
                                },
                                scope: row.get(1)?,
                                priority: row.get(2)?,
                                created_at: row.get(3)?,
                                kind: row.get(5)?,
                                author: row.get(6)?,
                            })
                        })
                        .map_err(db::map_sqlite)?;
                    for row in found {
                        rows.push(row.map_err(db::map_sqlite)?);
                    }
                }
                if mine_only {
                    rows.retain(|row| row.candidate.assignees.iter().any(|a| *a == pane));
                }
                for row in rows.iter_mut() {
                    let id = row.candidate.id.clone();
                    let mut stmt = tx
                        .prepare("SELECT pane_id FROM task_leases WHERE task_id = ? AND expires_at > ?")
                        .map_err(db::map_sqlite)?;
                    row.candidate.leased_by = stmt
                        .query_map(params![id, now], |r| r.get::<_, String>(0))
                        .map_err(db::map_sqlite)?
                        .collect::<Result<_, _>>()
                        .map_err(db::map_sqlite)?;
                    row.candidate.blocked_by = dependency_state_conn(&tx, &id)?.1.blocked_by;
                    // The allocation table may not exist yet (the Python tolerates that).
                    let offer: Option<String> = match tx
                        .query_row("SELECT owners FROM allocation_offers WHERE task_id = ? AND state = 'ready'",
                                   params![id], |r| r.get(0))
                        .optional()
                    {
                        Ok(found) => found,
                        Err(error) if error.to_string().contains("no such table") => None,
                        Err(error) => return Err(db::map_sqlite(error)),
                    };
                    if let Some(owners) = offer {
                        let mut mine = row.candidate.assignees.clone();
                        mine.sort();
                        row.candidate.allocation_waiting = mine == decode_list(Some(&owners));
                    }
                }
                rows.sort_by_key(|row| order_key(&row.priority, row.created_at, &row.candidate.id));
                let refuse = |tx: rusqlite::Transaction<'_>, refusal: Refusal| -> Result<Result<String, Refusal>, DunkError> {
                    tx.commit().map_err(db::map_sqlite)?;
                    eprintln!("task claim refused: pane={pane} scope={target} reason={} tasks={}",
                              refusal.reason, if refusal.task_ids.is_empty() { "-".to_string() } else { refusal.task_ids.join(",") });
                    Ok(Err(refusal))
                };
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
                        return refuse(tx, Refusal { reason: "busy", task_ids: busy, owned_by: None });
                    }
                }
                let chosen = rows.iter().find(|row| {
                    row.candidate.blocked_by.is_empty()
                        && options
                            .kinds
                            .as_ref()
                            .is_none_or(|kinds| kinds.is_empty() || kinds.contains(&row.kind))
                        && !options.exclude_author.as_ref().is_some_and(|who| row.author.as_ref() == Some(who))
                        && claimable(&row.candidate, Some(&pane))
                });
                let Some(chosen) = chosen else {
                    let all: Vec<Candidate> = rows.iter().map(|r| r.candidate.clone()).collect();
                    return refuse(tx, refusal(&all, Some(&pane)));
                };
                // `_claim_locked`: the lease, the status, the attempt and the event.
                let mut assignees = chosen.candidate.assignees.clone();
                let added = !assignees.contains(&pane);
                if added {
                    assignees.push(pane.clone());
                }
                let (pid, start, name) = match &options.caller {
                    Some((p, s, n)) => (Some(*p), s.clone(), n.clone()),
                    None => (None, None, None),
                };
                tx.execute(
                    "INSERT OR REPLACE INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at, caller_pid, caller_start, caller_name, assignee_added) VALUES (?,?,?,?,?,?,?,?,?)",
                    params![chosen.candidate.id, pane, now, now, now + lease_s, pid, start, name, added as i64],
                ).map_err(db::map_sqlite)?;
                tx.execute(
                    "UPDATE tasks SET status = 'in_progress', assignees = ?, attempts = attempts + 1, updated_at = ?, block_reason = NULL WHERE id = ?",
                    params![json!(assignees).to_string(), now, chosen.candidate.id],
                ).map_err(db::map_sqlite)?;
                tx.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, duration_ms, meta) VALUES (?,?,?,?,?,?,?,?,?,?)",
                    params![chosen.candidate.id, "claimed", now, pane, chosen.scope, chosen.priority,
                            chosen.candidate.status, "in_progress", ((now - chosen.created_at) * 1000.0) as i64,
                            json!({ "lease_seconds": lease_s, "caller_pid": pid, "caller_name": name }).to_string()],
                ).map_err(db::map_sqlite)?;
                let id = chosen.candidate.id.clone();
                tx.commit().map_err(db::map_sqlite)?;
                Ok(Ok(id))
            })
            .await?;
        match chosen_id {
            Ok(id) => Ok(ClaimOutcome::Claimed {
                task: Box::new(self.get(&id).await?),
                scope: outcome_target,
                lease_seconds: lease_s,
                expires_at: now + lease_s,
            }),
            Err(refusal) => Ok(ClaimOutcome::Refused {
                scope: outcome_target,
                refusal,
            }),
        }
    }
}

/// One requeued claim returned by `reap`.
#[derive(Debug, Clone, PartialEq)]
pub struct Requeued {
    pub task_id: String,
    pub pane_id: String,
    pub reason: &'static str,
}

/// `caller_alive(pid, start)`: is the process behind a lease still there?
pub type CallerAlive = Box<dyn Fn(i64, Option<&str>) -> bool + Send>;

/// `_after_lease_change`: reopen a task when its last lease goes away, keeping the OWNER.
/// A lease is activity: expiry, release and a reaped claim end the claim, not the
/// assignment (t145), so the assignee list is written back as it was given. Returns the
/// resulting status (F3: a task with another live lease stays in_progress).
fn after_lease_change(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    assignees: &[String],
    now: f64,
) -> Result<String, DunkError> {
    let remaining: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM task_leases WHERE task_id = ?",
            params![id],
            |r| r.get(0),
        )
        .map_err(db::map_sqlite)?;
    let mut status: String = tx
        .query_row("SELECT status FROM tasks WHERE id = ?", params![id], |r| {
            r.get(0)
        })
        .map_err(db::map_sqlite)?;
    if status != "done" && status != "cancelled" && remaining == 0 {
        status = "open".into();
    }
    tx.execute(
        "UPDATE tasks SET assignees = ?, status = ?, attempts = attempts + 1, updated_at = ? WHERE id = ?",
        params![json!(assignees).to_string(), status, now, id],
    )
    .map_err(db::map_sqlite)?;
    Ok(status)
}

impl Tasks {
    /// Give a claim back without completing it (`release`).
    ///
    /// A claim that ADDED its pane to the assignee list takes it away again on release
    /// (t595): leaving it made a released task unclaimable by everyone. An assignment the
    /// owner made before the claim is untouched, because the lease records which it was.
    pub async fn release(&self, id: &str, pane_id: &str) -> Result<Task, DunkError> {
        let wanted = id.trim().to_string();
        let pane = pane_id.to_string();
        let now = crate::clock::now_s();
        let resolved = self
            .writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let (scope, priority, status, raw): (String, String, String, Option<String>) = tx
                    .query_row(
                        "SELECT scope, priority, status, assignees FROM tasks WHERE id = ?",
                        params![wanted],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))?;
                let added = tx
                    .query_row(
                        "SELECT assignee_added FROM task_leases WHERE task_id = ? AND pane_id = ?",
                        params![wanted, pane],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .is_some_and(|v| v != 0);
                tx.execute("DELETE FROM task_leases WHERE task_id = ? AND pane_id = ?", params![wanted, pane])
                    .map_err(db::map_sqlite)?;
                let mut assignees = decode_list(raw.as_deref());
                if added {
                    assignees.retain(|a| *a != pane);
                }
                let resulting = after_lease_change(&tx, &wanted, &assignees, now)?;
                tx.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to) VALUES (?,?,?,?,?,?,?,?)",
                    params![wanted, "released", now, pane, scope, priority, status, resulting],
                )
                .map_err(db::map_sqlite)?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(wanted)
            })
            .await?;
        self.get(&resolved).await
    }

    /// Return expired or dead-caller leases to the queue (`reap`).
    ///
    /// `alive_panes` is the set of panes the terminal backend knows, and must be tagged
    /// `alive_source == "live"`: a fixture pane list must never move real work. Only the
    /// lease deadline or a dead caller requeues a claim; a vanished pane is recorded in
    /// the event but is not evidence by itself (the pane reader can lag, and a legacy
    /// lease with no caller identity is unattributable, not dead).
    pub async fn reap(
        &self,
        alive_panes: Option<Vec<String>>,
        caller_alive: Option<CallerAlive>,
        now: Option<f64>,
        alive_source: Option<&str>,
    ) -> Result<Vec<Requeued>, DunkError> {
        if alive_panes.is_some() && alive_source != Some("live") {
            return Err(DunkError::Refused(
                "alive_panes must be tagged alive_source='live'; a fixture pane list must never move real work".into(),
            ));
        }
        let now = now.unwrap_or_else(crate::clock::now_s);
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                type LeaseRow = (String, String, f64, Option<i64>, Option<String>, Option<String>);
                let leases: Vec<LeaseRow> = {
                    let mut stmt = tx
                        .prepare("SELECT task_id, pane_id, expires_at, caller_pid, caller_start, caller_name FROM task_leases")
                        .map_err(db::map_sqlite)?;
                    let rows = stmt
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                        .map_err(db::map_sqlite)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(db::map_sqlite)?;
                    rows
                };
                let mut requeued = Vec::new();
                for (task_id, pane_id, expires_at, pid, start, name) in leases {
                    let expired = expires_at <= now;
                    let gone = alive_panes.as_ref().is_some_and(|alive| !alive.contains(&pane_id));
                    let dead_caller = match (&caller_alive, pid) {
                        (Some(alive), Some(pid)) if pid != 0 => !alive(pid, start.as_deref()),
                        _ => false,
                    };
                    // BOTH-condition rule: only the lease deadline or a dead caller are reasons.
                    if !(expired || dead_caller) {
                        continue;
                    }
                    let reason = if dead_caller { "caller_dead" } else { "expired" };
                    tx.execute("DELETE FROM task_leases WHERE task_id = ? AND pane_id = ?", params![task_id, pane_id])
                        .map_err(db::map_sqlite)?;
                    let row: Option<(String, String, String, Option<String>)> = tx
                        .query_row(
                            "SELECT scope, priority, status, assignees FROM tasks WHERE id = ?",
                            params![task_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        )
                        .optional()
                        .map_err(db::map_sqlite)?;
                    let Some((scope, priority, status, raw)) = row else { continue };
                    let owners = decode_list(raw.as_deref());
                    let resulting = after_lease_change(&tx, &task_id, &owners, now)?;
                    let event = if resulting == "open" { "requeued" } else { "released" };
                    tx.execute(
                        "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, meta) VALUES (?,?,?,?,?,?,?,?,?)",
                        params![task_id, event, now, pane_id, scope, priority, status, resulting,
                                json!({ "reason": reason, "pane_gone": gone, "caller_pid": pid, "caller_name": name }).to_string()],
                    )
                    .map_err(db::map_sqlite)?;
                    if resulting == "open" {
                        if alive_panes.as_ref().is_some_and(|alive| !owners.is_empty() && !owners.iter().any(|o| alive.contains(o))) {
                            // Advisory only: ownership is durable, so a dead owner needs an
                            // explicit reassignment, not a lease side effect.
                            eprintln!("task {task_id} reopened while owned by a pane that is not live: owners={}", owners.join(","));
                        }
                        requeued.push(Requeued { task_id, pane_id, reason });
                    }
                }
                tx.commit().map_err(db::map_sqlite)?;
                Ok(requeued)
            })
            .await
    }
}

#[cfg(test)]
#[path = "claim_tests.rs"]
mod claim_tests;
