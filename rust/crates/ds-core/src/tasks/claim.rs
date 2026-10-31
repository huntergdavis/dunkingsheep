//! Claiming (T12), ported from `dunk_tasks.py`: leases, the claimability rules and the
//! refusal reasons. Slice 1 of T12: `leases_for`, `leases_for_pane`, `_busy_task_ids`,
//! `heartbeat`, and the two pure decisions `_claimable` and `_refusal`.
//!
//! NOT PORTED yet (next slices): `claim_next` / `_claim_locked`, `release`, `reap`, and
//! the per-viewer `claimable` / `eligible` fields of `_decorate`.

use rusqlite::params;

use super::{default_lease_s, Tasks};
use crate::db;
use crate::error::DunkError;

/// One live lease row.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub task_id: String,
    pub pane_id: String,
    pub claimed_at: f64,
    pub heartbeat_at: f64,
    pub expires_at: f64,
}

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
    /// Live leases for a task: expired rows are ignored.
    pub async fn leases_for(&self, id: &str, now: Option<f64>) -> Result<Vec<Lease>, DunkError> {
        let id = id.to_string();
        let now = now.unwrap_or_else(crate::clock::now_s);
        self.writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare(&format!("SELECT {LEASE_COLUMNS} FROM task_leases WHERE task_id = ? AND expires_at > ?"))
                    .map_err(db::map_sqlite)?;
                let rows = stmt.query_map(params![id, now], lease_from_row).map_err(db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
            })
            .await
    }

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

#[cfg(test)]
#[path = "claim_tests.rs"]
mod claim_tests;
