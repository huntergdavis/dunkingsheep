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
use crate::tasks::{AddOptions, Decorated, FieldEdit, Task, Tasks};

/// `TASK_GLOBAL`.
pub const TASK_GLOBAL: &str = "global";

/// What `task_cancel` returns: the cancelled row plus the dependents it strands (t878).
#[derive(Debug, Clone, PartialEq)]
pub struct Cancelled {
    pub task: Task,
    pub blocked_dependents: Vec<String>,
}

/// Roles that file their own work and may not hand it to another lane.
const LANE_ROLES: [&str; 4] = ["desk", "planner", "worker", "reviewer"];

/// `task_add`'s optional arguments.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskAddArgs<'a> {
    pub status: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub assignees: &'a [&'a str],
    pub parent_id: Option<&'a str>,
    pub depends_on: &'a [&'a str],
    pub multi_assignee: bool,
}

/// `task_update`'s optional arguments; `None` leaves a field alone (the Python's Ellipsis
/// for `parent_id` and `block_reason` is `Some(None)` to clear).
#[derive(Debug, Default, Clone)]
pub struct TaskUpdateArgs<'a> {
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub status: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub parent_id: Option<Option<&'a str>>,
    pub assignees: Option<&'a [&'a str]>,
    pub depends_on: Option<&'a [&'a str]>,
    pub block_reason: Option<Option<&'a str>>,
    pub multi_assignee: Option<bool>,
}

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
    /// t278 no-pane/private-queue rule, THEN the expansion of `self`/`me` in the assignees
    /// (`_task_assignees`), the owner-only assignment rule, the kind rules and the open-status
    /// rule. `assignees` are the RAW ones: resolving them first would answer "self-assign
    /// requires a known caller pane" to a no-pane caller that Python refuses with the t278 text
    /// (review t1365). Returns the resolved owners for the store write. `target` is the resolved
    /// scope and `role` the caller's role (`caller_role`, not ported).
    pub fn check_task_add(
        target: &str,
        assignees: &[&str],
        role: &str,
        kind: Option<&str>,
        status: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Vec<String>, DunkError> {
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
        let owners = Self::task_assignees(assignees, self_pane_id)?;
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
        Ok(owners)
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

    /// `task_add`: resolve scope and owners, apply the pre-write refusals, then one store
    /// write. Unassigned work stays unowned (`default_task_owner`); a Flock-level priority
    /// of `medium` is the Python's default (the store's own default is `low`). `role` is the
    /// caller's role (`caller_role`, not ported); `kind` is checked here but the store has
    /// no `kind` argument in the Python either, so it is not written.
    #[allow(clippy::too_many_arguments)]
    pub async fn task_add(
        &self,
        title: &str,
        scope: Option<&str>,
        body: Option<&str>,
        opts: TaskAddArgs<'_>,
        role: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        let target = Self::task_scope(scope, self_pane_id, TASK_GLOBAL)?;
        let mut owners = Self::task_assignees(opts.assignees, self_pane_id)?;
        Self::check_task_add(&target, &owners, role, opts.kind, opts.status, self_pane_id)?;
        if owners.is_empty() {
            owners = Self::default_task_owner(self_pane_id);
        }
        let non_empty = |v: Option<&str>, d: &str| match v {
            Some(v) if !v.is_empty() => v.to_string(),
            _ => d.to_string(),
        };
        tasks
            .add_with(
                &target,
                title,
                body.unwrap_or(""),
                AddOptions {
                    status: Some(non_empty(opts.status, "open")),
                    priority: Some(non_empty(opts.priority, "medium")),
                    assignees: owners,
                    parent_id: opts.parent_id.map(str::to_string),
                    depends_on: opts.depends_on.iter().map(|d| d.to_string()).collect(),
                    created_by: self_pane_id.filter(|p| !p.is_empty()).map(str::to_string),
                    multi_assignee: opts.multi_assignee,
                },
            )
            .await
    }

    /// `task_list`: `(explicit scope or None for "visible", tasks)`. A scope other than
    /// blank/`all` goes through `task_scope` (privacy), then the store filters. `limit` of 0
    /// means the Python's default of 200.
    pub async fn task_list(
        &self,
        scope: Option<&str>,
        status: Option<&str>,
        assignee: Option<&str>,
        include_done: bool,
        limit: usize,
        self_pane_id: Option<&str>,
    ) -> Result<(Option<String>, Vec<Decorated>), DunkError> {
        let tasks = self.task_store()?;
        let explicit = match scope.map(str::trim) {
            None | Some("") | Some("all") => None,
            Some(_) => Some(Self::task_scope(scope, self_pane_id, TASK_GLOBAL)?),
        };
        let limit = if limit == 0 { 200 } else { limit };
        let rows = tasks
            .list_visible(
                explicit.as_deref(),
                status,
                assignee,
                include_done,
                limit,
                self_pane_id,
            )
            .await?;
        Ok((explicit, rows))
    }

    /// `task_read`: the task, if the caller may see it.
    pub async fn task_read(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await
    }

    /// `task_update`: field edits through `Tasks::update`, then a status change through
    /// `set_status` (which carries the terminal-status guards). The Python does both in one
    /// transaction; here a refused status leaves the field edits applied.
    pub async fn task_update(
        &self,
        task_id: &str,
        args: TaskUpdateArgs<'_>,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        let resolved = match args.assignees {
            Some(list) => Some(Self::task_assignees(list, self_pane_id)?),
            None => None,
        };
        let own = |v: &str| v.to_string();
        let edit = FieldEdit {
            title: args.title.map(own),
            body: args.body.map(own),
            priority: args.priority.map(own),
            depends_on: args.depends_on.map(|l| l.iter().map(|d| own(d)).collect()),
            parent_id: args.parent_id.map(|p| p.map(own)),
            assignees: resolved,
            multi_assignee: args.multi_assignee,
            block_reason: args.block_reason.map(|r| r.map(own)),
        };
        let mut task = tasks.update(task_id, edit, self_pane_id).await?;
        if let Some(status) = args.status {
            let reason = match args.block_reason {
                Some(Some(r)) => Some(r),
                _ => None,
            };
            task = tasks
                .set_status(task_id, status, reason, self_pane_id)
                .await?;
        }
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

    /// `task_cancel`: the row WITH `blocked_dependents` (t878), the ids of the open, in-progress
    /// or blocked tasks that depend on it and are now unclaimable by work. Silence there is the
    /// defect t878 removed.
    pub async fn task_cancel(
        &self,
        task_id: &str,
        reason: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Cancelled, DunkError> {
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        // As `task_block`: None stays NULL (t1314), not ''.
        let task = tasks.set_status(task_id, "cancelled", reason, None).await?;
        let blocked_dependents = tasks.blocked_dependents(&task.id).await?;
        Ok(Cancelled {
            task,
            blocked_dependents,
        })
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

#[cfg(test)]
mod list_tests {
    use super::*;
    use crate::flock::panes::Stores;

    #[tokio::test]
    async fn task_list_reports_visible_or_the_explicit_scope_and_enforces_privacy() {
        let dir = std::env::temp_dir().join(format!("ds-ct-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let _ = std::fs::remove_file(&path);
        let tasks = Arc::new(Tasks::open(&path).await.unwrap());
        let flock = Flock::new(dir.join("l.json"), dir.join("n.json"), false);
        flock.attach_stores(Stores {
            tasks: Some(tasks.clone()),
            ..Default::default()
        });
        tasks.add("global", "g", "").await.unwrap();
        tasks.add("w1:p1", "p", "").await.unwrap();
        let (scope, rows) = flock
            .task_list(None, None, None, true, 0, Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!((None, 2), (scope, rows.len()));
        let (scope, rows) = flock
            .task_list(Some("global"), None, None, true, 0, Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!((Some("global".to_string()), 1), (scope, rows.len()));
        assert!(flock
            .task_list(Some("w2:p2"), None, None, true, 0, Some("w1:p1"))
            .await
            .is_err());
    }
}
