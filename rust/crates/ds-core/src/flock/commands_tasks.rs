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
//! stats. Caller-sensitive additions resolve role through `PaneInfoEnv` and `caller_role`.

use std::sync::Arc;

use super::Flock;
use crate::error::DunkError;
use crate::flock::panes::PaneInfoEnv;
use crate::tasks::metrics::MetricsOptions;
use crate::tasks::{AddOptions, Decorated, FieldEdit, RecentNote, Task, Tasks};
use crate::tasks::{ClaimOptions, ClaimOutcome};

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
    pub author: Option<&'a str>,
    pub repo: Option<&'a str>,
    /// Text, as the Python takes it (`pr must be a number` is the store's refusal).
    pub pr: Option<&'a str>,
    pub head: Option<&'a str>,
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

    /// `task_add`: resolve scope and owners, read the caller role from the request context,
    /// apply the pre-write refusals, then one store write. Unassigned work stays unowned
    /// (`default_task_owner`); the default priority is `low` (`dunk_core.task_add`:
    /// `priority or "low"`). Kind, author, repo, pr and head go to the store.
    #[allow(clippy::too_many_arguments)]
    pub async fn task_add(
        &self,
        title: &str,
        scope: Option<&str>,
        body: Option<&str>,
        opts: TaskAddArgs<'_>,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Task, DunkError> {
        let tasks = self.task_store()?;
        let target = Self::task_scope(scope, self_pane_id, TASK_GLOBAL)?;
        let mut owners = Self::task_assignees(opts.assignees, self_pane_id)?;
        let owner_refs: Vec<&str> = owners.iter().map(String::as_str).collect();
        let role = self.caller_role(self_pane_id, env)?;
        Self::check_task_add(
            &target,
            &owner_refs,
            &role,
            opts.kind,
            opts.status,
            self_pane_id,
        )?;
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
                    priority: Some(non_empty(opts.priority, "low")),
                    assignees: owners,
                    parent_id: opts.parent_id.map(str::to_string),
                    depends_on: opts.depends_on.iter().map(|d| d.to_string()).collect(),
                    created_by: self_pane_id.filter(|p| !p.is_empty()).map(str::to_string),
                    multi_assignee: opts.multi_assignee,
                    kind: Some(non_empty(opts.kind, "work")),
                    author: opts.author.map(str::to_string),
                    repo: opts.repo.map(str::to_string),
                    pr: opts.pr.map(str::to_string),
                    head: opts.head.map(str::to_string),
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
        kind: Option<&str>,
        needs_owner: bool,
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
                kind,
                needs_owner,
            )
            .await?;
        Ok((explicit, rows))
    }

    /// `task_stats`: counts by status for the visible scopes (or the explicit one).
    pub async fn task_stats(
        &self,
        scope: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<serde_json::Value, DunkError> {
        let tasks = self.task_store()?;
        let explicit = match scope.map(str::trim) {
            None | Some("") | Some("all") => None,
            Some(_) => Some(Self::task_scope(scope, self_pane_id, TASK_GLOBAL)?),
        };
        tasks.stats(explicit.as_deref(), self_pane_id).await
    }

    /// `task_heartbeat`: extend the caller's lease. `caller` is `current_caller()` (pid, start,
    /// name), which the per-request caller context supplies (t1347), so it is a parameter.
    /// Returns `(expires_at, lease_seconds)`.
    pub async fn task_heartbeat(
        &self,
        task_id: &str,
        lease_seconds: Option<f64>,
        caller: Option<(i64, Option<String>, Option<String>)>,
        self_pane_id: Option<&str>,
    ) -> Result<(f64, f64), DunkError> {
        let pane = self_pane_id.filter(|p| !p.is_empty()).ok_or_else(|| {
            DunkError::Refused("task heartbeat requires a known caller pane".into())
        })?;
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        tasks.heartbeat(task_id, pane, lease_seconds, caller).await
    }

    /// `task_release`: give the caller's claim back.
    pub async fn task_release(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Task, DunkError> {
        let pane = self_pane_id.filter(|p| !p.is_empty()).ok_or_else(|| {
            DunkError::Refused("task release requires a known caller pane".into())
        })?;
        let tasks = self.task_store()?;
        self.visible_task(&tasks, task_id, self_pane_id).await?;
        tasks.release(task_id, pane).await
    }

    /// `_claim_filters`: `(kinds, exclude_author)` for a claim, from the caller's role. A
    /// reviewer pulls only `review` tasks it did not author; the owner is unrestricted;
    /// everyone else pulls everything but reviews.
    pub fn claim_filters(
        &self,
        env: &dyn PaneInfoEnv,
        self_pane_id: Option<&str>,
    ) -> Result<(Option<Vec<String>>, Option<String>), DunkError> {
        let pane = self_pane_id.filter(|p| !p.is_empty());
        let role = match pane {
            Some(p) => self.caller_role(Some(p), env)?,
            None => "anonymous".to_string(),
        };
        Ok(Self::filters_for_role(&role, pane))
    }

    /// The pure half of `_claim_filters`: `(kinds, exclude_author)` for a ROLE. It reads no
    /// request context, so `task_next` can compute it once, before its first await, and reuse it
    /// for every claim it makes (t1513).
    pub fn filters_for_role(
        role: &str,
        pane: Option<&str>,
    ) -> (Option<Vec<String>>, Option<String>) {
        match role {
            "reviewer" => (Some(vec!["review".to_string()]), pane.map(str::to_string)),
            "owner" => (None, None),
            _ => (
                Some(["work", "verify", "report"].map(String::from).to_vec()),
                None,
            ),
        }
    }

    /// `current_caller()` as the `(pid, start, name)` the store records on a lease. It reads the
    /// per-request thread-local context, so call it on the request's thread BEFORE the first
    /// await of an async command and pass the result in (`task_heartbeat`, `task_next`).
    pub fn caller_identity(&self) -> Option<(i64, Option<String>, Option<String>)> {
        let caller = self.current_caller()?;
        let pid = caller.get("pid").and_then(serde_json::Value::as_i64)?;
        let text = |key: &str| {
            caller
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        };
        Some((pid, text("start"), text("name")))
    }

    /// `task_next`: atomically claim the next task; an empty queue (or a refusal) subscribes
    /// the pane for a later push (`following`). Returns the outcome and whether it follows.
    ///
    /// THREAD CONTRACT (t1513): the per-request caller context (service token, cached role) is a
    /// thread-local, and this is an async fn: after an await the future may resume on another
    /// thread and read ANOTHER request's context. So everything that reads it happens here, before
    /// the first await: the role is read once, the claim filters are derived from it once and
    /// reused for the `auto` pane-queue claim and the global claim, and the lease `caller` is
    /// passed in (like `task_heartbeat`'s) instead of being read from the thread-local.
    pub async fn task_next(
        &self,
        env: &dyn PaneInfoEnv,
        scope: Option<&str>,
        lease_seconds: Option<f64>,
        allow_busy: bool,
        caller: Option<(i64, Option<String>, Option<String>)>,
        self_pane_id: Option<&str>,
    ) -> Result<(ClaimOutcome, bool), DunkError> {
        let tasks = self.task_store()?;
        let pane = self_pane_id.filter(|p| !p.is_empty());
        let role = match pane {
            Some(p) => self.caller_role(Some(p), env)?,
            None => "anonymous".to_string(),
        };
        let (kinds, exclude_author) = Self::filters_for_role(&role, pane);
        if role == "reviewer" {
            let pane = pane.unwrap_or_default();
            let (may, provider, allowed) = self.with_roles(|roles, _| {
                Ok((
                    roles.may_review(pane, Some(&role))?,
                    roles.provider_of(pane)?,
                    roles.reviewer_providers.join(", "),
                ))
            })?;
            if !may {
                return Err(DunkError::Refused(format!(
                    "{pane} is a reviewer lane on a provider that may not review ({}; allowed: \
                     {allowed}). Free models are the worker tier.",
                    provider.unwrap_or_else(|| "unknown".into())
                )));
            }
        }
        let value = scope.map(str::trim).unwrap_or("");
        let mut follow_scope = TASK_GLOBAL.to_string();
        let target = match value {
            "" | "global" => TASK_GLOBAL.to_string(),
            "self" => {
                let pane = pane.ok_or_else(|| {
                    DunkError::Refused("scope 'self' requires a known caller pane".into())
                })?;
                follow_scope = pane.to_string();
                pane.to_string()
            }
            "auto" => {
                if let Some(pane) = pane {
                    let local = tasks
                        .claim_next(
                            pane,
                            pane,
                            ClaimOptions {
                                lease_seconds,
                                caller: caller.clone(),
                                allow_busy,
                                kinds: kinds.clone(),
                                exclude_author: exclude_author.clone(),
                            },
                        )
                        .await?;
                    if matches!(local, ClaimOutcome::Claimed { .. }) {
                        return Ok((local, false));
                    }
                }
                TASK_GLOBAL.to_string()
            }
            "mine" => {
                if pane.is_none() {
                    return Err(DunkError::Refused(
                        "scope 'mine' requires a known caller pane".into(),
                    ));
                }
                follow_scope = "mine".to_string();
                "mine".to_string()
            }
            other => {
                let resolved = Self::task_scope(Some(other), self_pane_id, TASK_GLOBAL)?;
                follow_scope = resolved.clone();
                resolved
            }
        };
        let outcome = tasks
            .claim_next(
                &target,
                pane.unwrap_or(""),
                ClaimOptions {
                    lease_seconds,
                    caller,
                    allow_busy,
                    kinds,
                    exclude_author,
                },
            )
            .await?;
        let mut following = false;
        if !matches!(outcome, ClaimOutcome::Claimed { .. }) {
            if let Some(pane) = pane {
                self.task_waiters
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(pane.to_string(), follow_scope);
                following = true;
            }
        }
        Ok((outcome, following))
    }

    /// `task_follow`: ask the daemon to push the next task when this pane is idle. Returns
    /// `(scope, all followers)`.
    pub fn task_follow(
        &self,
        scope: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<(String, Vec<String>), DunkError> {
        let pane = self_pane_id
            .filter(|p| !p.is_empty())
            .ok_or_else(|| DunkError::Refused("task follow requires a known caller pane".into()))?;
        let target = if scope.map(str::trim) == Some("mine") {
            "mine".to_string()
        } else {
            Self::task_scope(scope, self_pane_id, TASK_GLOBAL)?
        };
        let mut waiters = self
            .task_waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        waiters.insert(pane.to_string(), target.clone());
        Ok((target, waiters.keys().cloned().collect()))
    }

    /// `task_unfollow`: `(was_following, remaining followers)`.
    pub fn task_unfollow(
        &self,
        self_pane_id: Option<&str>,
    ) -> Result<(bool, Vec<String>), DunkError> {
        let pane = self_pane_id.filter(|p| !p.is_empty()).ok_or_else(|| {
            DunkError::Refused("task unfollow requires a known caller pane".into())
        })?;
        let mut waiters = self
            .task_waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existed = waiters.remove(pane).is_some();
        Ok((existed, waiters.keys().cloned().collect()))
    }

    /// `task_followers`: `(pane, scope)` sorted by pane.
    pub fn task_followers(&self) -> Vec<(String, String)> {
        self.task_waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(pane, scope)| (pane.clone(), scope.clone()))
            .collect()
    }

    /// `task_read` with the last three note reasons attached (t605); a read view never fails
    /// on its evidence, so a notes error is an empty list.
    pub async fn task_read_with_notes(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<(Task, Vec<RecentNote>), DunkError> {
        let tasks = self.task_store()?;
        let task = self.visible_task(&tasks, task_id, self_pane_id).await?;
        let notes = tasks.recent_notes(&task.id, 3).await.unwrap_or_default();
        Ok((task, notes))
    }

    /// `task_metrics`: scopes by caller (a pane sees global plus its own, a caller with no pane
    /// sees all), then the store's metrics over the window.
    pub async fn task_metrics(
        &self,
        window_s: Option<f64>,
        since: Option<f64>,
        until: Option<f64>,
        scope: Option<&str>,
        bucket: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<serde_json::Value, DunkError> {
        let tasks = self.task_store()?;
        let pane = self_pane_id.filter(|p| !p.is_empty());
        let (scopes, label) = match scope.map(str::trim).unwrap_or("") {
            "" | "all" => match pane {
                Some(p) => (
                    Some(vec![TASK_GLOBAL.to_string(), p.to_string()]),
                    "visible".to_string(),
                ),
                None => (None, "all".to_string()),
            },
            "global" => (Some(vec![TASK_GLOBAL.to_string()]), "global".to_string()),
            other => {
                let target = Self::task_scope(Some(other), self_pane_id, TASK_GLOBAL)?;
                (Some(vec![target.clone()]), target)
            }
        };
        tasks
            .metrics(MetricsOptions {
                window_s: window_s.unwrap_or(3600.0),
                since,
                until,
                scopes,
                bucket: bucket.unwrap_or("hour").to_string(),
                scope_label: label,
            })
            .await
    }

    /// `task_reap`: requeue claims whose lease expired or whose recorded caller died.
    ///
    /// `alive_panes` is supplied only from a live terminal snapshot. The store refuses a
    /// fixture-tagged list, so test data can never make a real lease look abandoned.
    pub async fn task_reap(
        &self,
        alive_panes: Option<Vec<String>>,
        alive_source: Option<&str>,
    ) -> Result<serde_json::Value, DunkError> {
        let tasks = self.task_store()?;
        let caller_alive: crate::tasks::CallerAlive = Box::new(crate::proc::process_alive);
        let requeued = tasks
            .reap(alive_panes, Some(caller_alive), None, alive_source)
            .await?;
        Ok(serde_json::json!({
            "requeued": requeued.into_iter().map(|item| serde_json::json!({
                "task_id": item.task_id,
                "pane_id": item.pane_id,
                "reason": item.reason,
            })).collect::<Vec<_>>()
        }))
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
    use crate::flock::panes::PaneInfoEnv;
    use crate::flock::panes::Stores;

    async fn flock_with(tag: &str) -> (crate::testsupport::Scratch<Flock>, Arc<Tasks>) {
        let scratch = crate::testsupport::dir(&format!("ct-{tag}"));
        let dir = scratch.path().to_path_buf();
        let tasks = Arc::new(Tasks::open(dir.join("tasks.db")).await.unwrap());
        let flock = Flock::new(dir.join("l.json"), dir.join("n.json"), false);
        flock.attach_stores(Stores {
            tasks: Some(tasks.clone()),
            ..Default::default()
        });
        (crate::testsupport::Scratch::new(flock, scratch), tasks)
    }

    #[tokio::test]
    async fn task_stats_counts_by_status_for_visible_scopes() {
        let (flock, tasks) = flock_with("stats").await;
        let a = tasks.add("global", "a", "").await.unwrap();
        tasks.add("global", "b", "").await.unwrap();
        tasks.add("w1:p1", "c", "").await.unwrap();
        tasks.add("w2:p2", "hidden", "").await.unwrap();
        tasks.complete(&a.id).await.unwrap();
        let got = flock.task_stats(None, Some("w1:p1")).await.unwrap();
        assert_eq!(serde_json::json!(["global", "w1:p1"]), got["scopes"]);
        assert_eq!(serde_json::json!(1), got["by_scope"]["global"]["done"]);
        assert_eq!(serde_json::json!(1), got["by_scope"]["global"]["open"]);
        assert_eq!(serde_json::json!(2), got["totals"]["open"]);
        assert_eq!(serde_json::json!(0), got["totals"]["blocked"]);
        assert!(got["by_scope"].get("w2:p2").is_none());
    }

    struct NoPanes;
    impl PaneInfoEnv for NoPanes {
        fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }
        fn get_pane(&self, _: &str) -> Option<serde_json::Value> {
            None
        }
    }

    async fn staffed(tag: &str) -> (crate::testsupport::Scratch<Flock>, Arc<Tasks>) {
        let (flock, tasks) = flock_with(tag).await;
        let dir = flock.dir().to_path_buf();
        let mut roles = crate::roles::Roles::new(
            &crate::config::Env::default(),
            Some(dir.join("roles.json")),
            Some(dir.join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w1:p1", "worker", None, None, Some("pi"), None, None, None)
            .unwrap();
        roles
            .register(
                "w2:p2",
                "reviewer",
                None,
                None,
                Some("pi"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register(
                "w3:p3",
                "reviewer",
                None,
                None,
                Some("claude"),
                None,
                None,
                None,
            )
            .unwrap();
        flock.attach_roles(roles, None);
        (flock, tasks)
    }

    #[tokio::test]
    async fn task_next_filters_by_role_and_follows_on_an_empty_queue() {
        let (flock, tasks) = staffed("next").await;
        let review = tasks
            .add_with(
                "global",
                "review",
                "",
                AddOptions {
                    kind: Some("review".into()),
                    repo: Some("o/r".into()),
                    pr: Some("1".into()),
                    head: Some("abc".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // a worker never pulls the review, so the queue is empty for it and it follows
        let (outcome, following) = flock
            .task_next(&NoPanes, None, None, false, None, Some("w1:p1"))
            .await
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Refused { .. }));
        assert!(following);
        assert_eq!(
            vec![("w1:p1".to_string(), "global".to_string())],
            flock.task_followers()
        );
        let (was, left) = flock.task_unfollow(Some("w1:p1")).unwrap();
        assert!(was && left.is_empty());
        assert!(!flock.task_unfollow(Some("w1:p1")).unwrap().0);
        // a reviewer on a provider that may review pulls it
        let (outcome, following) = flock
            .task_next(&NoPanes, None, None, false, None, Some("w3:p3"))
            .await
            .unwrap();
        assert!(!following);
        match outcome {
            ClaimOutcome::Claimed { task, .. } => assert_eq!(review.id, task.id),
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn task_next_refuses_a_reviewer_on_a_provider_that_may_not_review() {
        let (flock, _tasks) = staffed("next-free").await;
        let err = flock
            .task_next(&NoPanes, None, None, false, None, Some("w2:p2"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("w2:p2 is a reviewer lane on a provider that may not review (pi;"),
            "{err}"
        );
        let err = flock
            .task_next(&NoPanes, Some("self"), None, false, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scope 'self' requires a known caller pane"),
            "{err}"
        );
    }

    fn review_options() -> AddOptions {
        AddOptions {
            kind: Some("review".into()),
            repo: Some("o/r".into()),
            pr: Some("1".into()),
            head: Some("abc".into()),
            ..Default::default()
        }
    }

    fn claimed(outcome: ClaimOutcome) -> Task {
        match outcome {
            ClaimOutcome::Claimed { task, .. } => *task,
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn task_next_auto_claims_the_panes_own_queue_first() {
        let (flock, tasks) = staffed("auto-local").await;
        let local = tasks.add("w1:p1", "mine", "").await.unwrap();
        tasks.add("global", "fleet", "").await.unwrap();
        let (outcome, following) = flock
            .task_next(&NoPanes, Some("auto"), None, false, None, Some("w1:p1"))
            .await
            .unwrap();
        assert!(!following);
        assert_eq!(local.id, claimed(outcome).id, "the pane's own queue wins");
    }

    #[tokio::test]
    async fn task_next_auto_falls_through_to_global_when_the_pane_queue_is_empty() {
        let (flock, tasks) = staffed("auto-global").await;
        let fleet = tasks.add("global", "fleet", "").await.unwrap();
        let (outcome, following) = flock
            .task_next(&NoPanes, Some("auto"), None, false, None, Some("w1:p1"))
            .await
            .unwrap();
        assert!(!following);
        assert_eq!(fleet.id, claimed(outcome).id);
    }

    #[tokio::test]
    async fn task_next_auto_applies_the_role_filters_to_both_claims() {
        // t1513: the role is read ONCE and the same filters serve the pane-queue claim and the
        // global claim. A reviewer's pane queue holds only `work` (skipped), the global queue a
        // review (claimed); a worker never pulls that review and follows instead.
        let (flock, tasks) = staffed("auto-filters").await;
        tasks.add("w3:p3", "local work", "").await.unwrap();
        let review = tasks
            .add_with("global", "review", "", review_options())
            .await
            .unwrap();
        let (outcome, _) = flock
            .task_next(&NoPanes, Some("auto"), None, false, None, Some("w3:p3"))
            .await
            .unwrap();
        assert_eq!(review.id, claimed(outcome).id);
        let (outcome, following) = flock
            .task_next(&NoPanes, Some("auto"), None, false, None, Some("w1:p1"))
            .await
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Refused { .. }));
        assert!(following, "an empty claim subscribes the pane");
    }

    #[test]
    fn the_claim_filters_are_a_pure_function_of_the_role() {
        let (kinds, exclude) = Flock::filters_for_role("reviewer", Some("w3:p3"));
        assert_eq!(
            (Some(vec!["review".to_string()]), Some("w3:p3".to_string())),
            (kinds, exclude)
        );
        assert_eq!(
            (None, None),
            Flock::filters_for_role("owner", Some("w9:p9"))
        );
        for role in ["worker", "desk", "planner", "anonymous", "system"] {
            let (kinds, exclude) = Flock::filters_for_role(role, Some("w1:p1"));
            assert_eq!(
                Some(vec![
                    "work".to_string(),
                    "verify".to_string(),
                    "report".to_string()
                ]),
                kinds,
                "{role}"
            );
            assert_eq!(None, exclude, "{role}");
        }
    }

    #[test]
    fn task_next_reads_the_request_context_only_before_its_first_await() {
        // t1513: the per-request caller context (service token, cached role) is a thread-local,
        // and task_next is async: after an await the future may resume on another thread and read
        // ANOTHER request's context. A thread hop cannot be forced deterministically, so this pins
        // the contract at the source: nothing after the first `.await` may read that context.
        let source = include_str!("commands_tasks.rs");
        let start = source.find("pub async fn task_next(").expect("task_next");
        let body = &source[start..];
        let body = &body[..body.find("/// `task_follow`").expect("end of task_next")];
        let first_await = body.find(".await").expect("task_next awaits");
        let (before, after) = body.split_at(first_await);
        for forbidden in [
            "caller_role(",
            "claim_filters(",
            "caller_identity(",
            "current_caller(",
            "with_caller(",
            "service_token",
        ] {
            assert!(
                !after.contains(forbidden),
                "task_next reads `{forbidden}` after its first await: another request's context"
            );
        }
        assert_eq!(
            1,
            before.matches("caller_role(").count(),
            "the role is read exactly once, before the first await"
        );
    }

    #[tokio::test]
    async fn task_follow_scopes_and_needs_a_pane() {
        let (flock, _tasks) = staffed("follow").await;
        assert!(flock.task_follow(None, None).is_err());
        assert_eq!(
            ("global".to_string(), vec!["w1:p1".to_string()]),
            flock.task_follow(None, Some("w1:p1")).unwrap()
        );
        assert_eq!(
            "mine",
            flock.task_follow(Some("mine"), Some("w1:p1")).unwrap().0
        );
        assert!(flock.task_follow(Some("w9:p9"), Some("w1:p1")).is_err());
    }

    #[tokio::test]
    async fn task_read_attaches_the_last_three_note_reasons_and_metrics_pick_scopes() {
        let (flock, tasks) = flock_with("readnotes").await;
        let t = tasks.add("global", "t", "").await.unwrap();
        for n in 1..=4 {
            tasks
                .note(&t.id, "note", "w1:p1", Some(&format!("evidence {n}")))
                .await
                .unwrap();
        }
        tasks.note(&t.id, "note", "w1:p1", None).await.unwrap();
        let (task, notes) = flock
            .task_read_with_notes(&t.id, Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(t.id, task.id);
        let reasons: Vec<&str> = notes.iter().map(|n| n.reason.as_str()).collect();
        assert_eq!(vec!["evidence 2", "evidence 3", "evidence 4"], reasons);
        assert!(tasks.recent_notes(&t.id, 0).await.unwrap().is_empty());
        let visible = flock
            .task_metrics(None, None, None, None, None, Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(serde_json::json!("visible"), visible["scope"]);
        let all = flock
            .task_metrics(None, None, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(serde_json::json!("all"), all["scope"]);
        assert!(flock
            .task_metrics(None, None, None, Some("w2:p2"), None, Some("w1:p1"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn heartbeat_and_release_need_a_pane_and_a_lease() {
        let (flock, tasks) = flock_with("lease").await;
        let t = tasks.add("global", "t", "").await.unwrap();
        let err = flock
            .task_heartbeat(&t.id, None, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a known caller pane"), "{err}");
        let err = flock
            .task_release(&t.id, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires a known caller pane"), "{err}");
        let err = flock
            .task_heartbeat(&t.id, None, None, Some("w1:p1"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no active lease for w1:p1"), "{err}");
        tasks
            .claim_next("global", "w1:p1", Default::default())
            .await
            .unwrap();
        let (_, lease) = flock
            .task_heartbeat(&t.id, Some(60.0), None, Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(60.0, lease);
        let released = flock.task_release(&t.id, Some("w1:p1")).await.unwrap();
        assert_eq!("open", released.status);
    }

    #[tokio::test]
    async fn task_list_reports_visible_or_the_explicit_scope_and_enforces_privacy() {
        let scratch_dir = crate::testsupport::dir("ct-list");
        let dir = scratch_dir.path().to_path_buf();
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
            .task_list(None, None, None, true, 0, Some("w1:p1"), None, false)
            .await
            .unwrap();
        assert_eq!((None, 2), (scope, rows.len()));
        let (scope, rows) = flock
            .task_list(
                Some("global"),
                None,
                None,
                true,
                0,
                Some("w1:p1"),
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!((Some("global".to_string()), 1), (scope, rows.len()));
        assert!(flock
            .task_list(
                Some("w2:p2"),
                None,
                None,
                true,
                0,
                Some("w1:p1"),
                None,
                false
            )
            .await
            .is_err());
    }
}
