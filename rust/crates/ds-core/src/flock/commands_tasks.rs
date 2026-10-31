//! T35 (first slice): the task commands of `dunk_core.Flock` that are pure rules or map one
//! to one onto the task store: `_task_scope`, `_task_assignees`, `_default_task_owner`,
//! `_require_task_visible`, the role/kind/status rules of `task_add`, and `task_block`,
//! `task_unblock`, `task_cancel`, `task_delete`, `task_assign`, `task_note`.
//!
//! NOT in this slice: `task_add` itself (`Tasks::add` takes only scope/title/body, so the
//! assignees, kind, author, repo/pr/head and status it writes have no store call yet),
//! `task_list`/`task_read` (need the store's filtered list and `recent_notes`), `task_update`,
//! `task_complete` (verdict, `_authorize_task_complete`, `_require_completion_binding`, the
//! github hook), `task_next`/`_claim_filters`, heartbeat/release/follow/unfollow/followers/
//! stats/metrics/reap. The caller's role arrives as a parameter: `caller_role` is not ported.

use std::sync::Arc;

use super::Flock;
use crate::error::DunkError;
use crate::tasks::{Task, Tasks};

/// `TASK_GLOBAL`.
pub const TASK_GLOBAL: &str = "global";

/// Roles that file their own work and may not hand it to another lane.
const LANE_ROLES: [&str; 4] = ["desk", "planner", "worker", "reviewer"];

impl Flock {
    /// `_task_scope`: resolve a caller's scope and enforce per-pane privacy. Global is
    /// fleet-wide; a pane id is private to that pane, and an agent may only name its own
    /// pane (or `self`). A caller with no pane may name any pane for inspection.
    pub fn task_scope(
        scope: Option<&str>,
        self_pane_id: Option<&str>,
        default: &str,
    ) -> Result<String, DunkError> {
        let value = match scope.map(str::trim) {
            None | Some("") | Some("all") => return Ok(default.to_string()),
            Some(value) => value,
        };
        let self_pane = self_pane_id.filter(|p| !p.is_empty());
        if value == "self" {
            return self_pane.map(str::to_string).ok_or_else(|| {
                DunkError::Refused("scope 'self' requires a known caller pane".into())
            });
        }
        let value = crate::tasks::validate_scope(value)?;
        if let Some(pane) = self_pane {
            if value != TASK_GLOBAL && value != pane {
                return Err(DunkError::Refused(format!(
                    "per-pane tasks are private to {value}; caller runs in {pane}"
                )));
            }
        }
        Ok(value)
    }

    /// `_task_assignees`: expand `self`/`me` to the caller's pane id; blanks are dropped.
    pub fn task_assignees(
        assignees: &[&str],
        self_pane_id: Option<&str>,
    ) -> Result<Vec<String>, DunkError> {
        let mut resolved = Vec::new();
        for value in assignees {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if value == "self" || value == "me" {
                match self_pane_id.filter(|p| !p.is_empty()) {
                    Some(pane) => resolved.push(pane.to_string()),
                    None => {
                        return Err(DunkError::Refused(
                            "self-assign requires a known caller pane".into(),
                        ))
                    }
                }
            } else {
                resolved.push(value.to_string());
            }
        }
        Ok(resolved)
    }

    /// `_default_task_owner`: FLAT_FLEET.md 5 - unassigned means claimable by any lane whose
    /// role matches the kind. The queue owns the work; nobody is handed it.
    pub fn default_task_owner(_self_pane_id: Option<&str>) -> Vec<String> {
        Vec::new()
    }

    /// `_require_task_visible`: a pane caller sees global tasks and its own private queue.
    /// A caller with no pane sees everything.
    pub fn require_task_visible(task: &Task, self_pane_id: Option<&str>) -> Result<(), DunkError> {
        let Some(pane) = self_pane_id.filter(|p| !p.is_empty()) else {
            return Ok(());
        };
        if task.scope != TASK_GLOBAL && task.scope != pane {
            return Err(DunkError::Refused(format!(
                "task {} belongs to another pane's private queue ({})",
                task.id, task.scope
            )));
        }
        Ok(())
    }

    /// The refusals of `task_add` that precede the store write, in the Python's order: the
    /// t278 no-pane/private-queue rule, the owner-only assignment rule, the kind rules and the
    /// open-status rule. `target` is the resolved scope, `owners` the resolved assignees and
    /// `role` the caller's role (`caller_role`, not ported).
    pub fn check_task_add(
        target: &str,
        owners: &[String],
        role: &str,
        kind: Option<&str>,
        status: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<(), DunkError> {
        let self_pane = self_pane_id.filter(|p| !p.is_empty());
        // t278: anonymous creation in another pane's PRIVATE queue is refused, naming the
        // workaround. (A pane caller is already refused by `task_scope`.)
        if target != TASK_GLOBAL && self_pane.is_none() {
            return Err(DunkError::Refused(format!(
                "refusing to create a task in {target}'s private queue from a caller with no pane \
                 id: per-pane queues belong to that pane. Pass --scope global to create a \
                 fleet-visible task (add --assign to assign a lane), or run the add from that \
                 pane itself."
            )));
        }
        let kind = kind.filter(|k| !k.is_empty()).unwrap_or("work");
        let lane_role = LANE_ROLES.contains(&role);
        if !owners.is_empty() && lane_role && owners.iter().any(|o| Some(o.as_str()) != self_pane) {
            return Err(DunkError::Refused(format!(
                "task add may assign only to yourself for role {role}; handing work to another \
                 lane is the owner's (lanes pull with task next)"
            )));
        }
        if (role == "desk" || role == "planner") && kind != "work" {
            return Err(DunkError::Refused(format!(
                "role {role} files kind=work only"
            )));
        }
        if kind == "review" && !matches!(role, "owner" | "system" | "web") {
            return Err(DunkError::Refused(
                "review tasks are filed by the review timer (system) or the owner".into(),
            ));
        }
        if lane_role && !matches!(status, None | Some("") | Some("open")) {
            return Err(DunkError::Refused(format!(
                "role {role} files open tasks; status moves through next/complete/block"
            )));
        }
        Ok(())
    }

    fn task_store(&self) -> Result<Arc<Tasks>, DunkError> {
        self.stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("the task store is not attached".into()))
    }

    /// Fetch a task and require it visible to the caller (the first line of most commands).
    async fn visible_task(
        &self,
        tasks: &Tasks,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let task = tasks.get(task_id).await?;
        Self::require_task_visible(&task, self_pane_id)?;
        Ok(task)
    }

    /// `task_block`.
    pub async fn task_block(
        &self,
        task_id: &str,
        reason: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        // Python passes reason=None through, which stores NULL; `Tasks::block` takes a &str and
        // would write '' (t1314). `set_status` takes the Option, as `Tasks.update` does.
        tasks.set_status(task_id, "blocked", reason, None).await
    }

    /// `task_unblock`.
    pub async fn task_unblock(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        tasks.unblock(task_id).await
    }

    /// `task_cancel`.
    pub async fn task_cancel(
        &self,
        task_id: &str,
        reason: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        // As `task_block`: None stays NULL (t1314), not ''.
        tasks.set_status(task_id, "cancelled", reason, None).await
    }

    /// `task_delete`: `{"deleted": id}` in the Python, the id here.
    pub async fn task_delete(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<String, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        tasks.delete(task_id).await
    }

    /// `task_assign`: `self`/`me` expand to the caller's pane.
    pub async fn task_assign(
        &self,
        task_id: &str,
        assignees: &[&str],
        mode: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        let resolved = Self::task_assignees(assignees, self_pane_id)?;
        let refs: Vec<&str> = resolved.iter().map(String::as_str).collect();
        tasks.assign(task_id, &refs, mode).await
    }

    /// `task_note`: append an explanatory event. Unlike the others it does NOT require the
    /// task visible (the Python does not), and a missing actor is recorded as empty. The event
    /// defaults to `note` only when omitted: Python's `event="note"` keeps an explicit empty one.
    pub async fn task_note(
        &self,
        task_id: &str,
        event: Option<&str>,
        reason: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<(), DunkError> {
        let tasks = self.task_store()?;
        let event = event.unwrap_or("note");
        tasks
            .note(
                task_id,
                event,
                self_pane_id.unwrap_or(""),
                reason.filter(|r| !r.is_empty()),
            )
            .await
    }
}

#[cfg(test)]
#[path = "commands_tasks_tests.rs"]
mod commands_tasks_tests;
