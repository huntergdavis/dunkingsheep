//! Full task dictionaries for API adapters; request identity uses the existing Flock path.

use super::{
    commands_tasks::{TaskAddArgs, TaskUpdateArgs},
    panes::PaneInfoEnv,
    Flock,
};
use crate::{
    error::DunkError,
    tasks::{
        wire::{DecoratedWire, TaskWire, TaskWireNotes},
        ClaimOutcome,
    },
};
use serde_json::{json, Value};

impl Flock {
    /// Return the full post-commit row just as Python's mutation methods read it afterward.
    pub async fn task_update_wire(
        &self,
        task_id: &str,
        args: TaskUpdateArgs<'_>,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        self.task_update_wire_with_raw_block_reason(task_id, args, None, self_pane_id)
            .await
    }

    pub(crate) async fn task_update_wire_with_raw_block_reason(
        &self,
        task_id: &str,
        args: TaskUpdateArgs<'_>,
        raw_block_reason: Option<Option<Value>>,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        let task = self
            .task_update_with_raw_block_reason(task_id, args, raw_block_reason, self_pane_id)
            .await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    pub async fn task_assign_wire(
        &self,
        task_id: &str,
        assignees: &[&str],
        mode: &str,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        self.task_assign_wire_with_mode_parser(
            task_id,
            assignees,
            || Ok(mode.to_string()),
            self_pane_id,
            false,
            None,
        )
        .await
    }

    pub(crate) async fn task_assign_wire_with_mode_parser<F>(
        &self,
        task_id: &str,
        assignees: &[&str],
        parse_mode: F,
        self_pane_id: Option<&str>,
        planner_author_guard: bool,
        planner: Option<crate::tasks::planner_scope::PlannerIdentity>,
    ) -> Result<TaskWire, DunkError>
    where
        F: FnOnce() -> Result<String, DunkError>,
    {
        let task = self
            .task_assign_with_mode_parser(
                task_id,
                assignees,
                parse_mode,
                self_pane_id,
                planner_author_guard,
                planner,
            )
            .await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    pub async fn task_block_wire(
        &self,
        task_id: &str,
        reason: Option<&Value>,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        // Preserve visibility and terminal-state refusals before SQLite binding errors.
        self.task_read(task_id, self_pane_id).await?;
        let task = self
            .task_store()?
            .set_status_with_raw_reason(task_id, "blocked", reason, None)
            .await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    pub async fn task_unblock_wire(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        let task = self.task_unblock(task_id, self_pane_id).await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    pub async fn task_cancel_wire(
        &self,
        task_id: &str,
        reason: Option<&Value>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        self.task_read(task_id, self_pane_id).await?;
        let tasks = self.task_store()?;
        let task = tasks
            .set_status_with_raw_reason(task_id, "cancelled", reason, None)
            .await?;
        let blocked_dependents = tasks.blocked_dependents(&task.id).await?;
        let wire = tasks.get_full_wire(&task.id, self_pane_id).await?;
        let mut response = wire.wire;
        response["blocked_dependents"] = json!(blocked_dependents);
        Ok(response)
    }

    /// Append a task event without a task lookup, preserving optional actor and raw event.
    pub async fn task_note_wire(
        &self,
        task_id: &str,
        event: &Value,
        actor: Option<&str>,
        meta: &Value,
    ) -> Result<Value, DunkError> {
        self.task_store()?
            .note_raw(task_id, event, actor, meta)
            .await?;
        Ok(json!({"noted": task_id, "event": event, "actor": actor, "meta": meta}))
    }

    /// Preserve the complete task/lease envelope using one post-commit full-row readback.
    #[allow(clippy::too_many_arguments)]
    pub async fn task_next_wire(
        &self,
        env: &dyn PaneInfoEnv,
        scope: Option<&str>,
        lease_seconds: Option<f64>,
        allow_busy: bool,
        caller: Option<(i64, Option<String>, Option<String>)>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        self.task_next_wire_with_lease_parser(
            env,
            scope,
            || Ok(lease_seconds),
            allow_busy,
            caller,
            self_pane_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn task_next_wire_with_lease_parser<F>(
        &self,
        env: &dyn PaneInfoEnv,
        scope: Option<&str>,
        parse_lease: F,
        allow_busy: bool,
        caller: Option<(i64, Option<String>, Option<String>)>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError>
    where
        F: FnOnce() -> Result<Option<f64>, DunkError>,
    {
        let (outcome, following) = self
            .task_next_with_lease_parser(
                env,
                scope,
                parse_lease,
                allow_busy,
                caller.clone(),
                self_pane_id,
            )
            .await?;
        match outcome {
            ClaimOutcome::Claimed {
                task,
                scope,
                lease_seconds,
                expires_at,
            } => {
                let wire = self
                    .task_store()?
                    .get_full_wire(&task.id, self_pane_id)
                    .await?;
                let (pid, start, name) = caller
                    .map(|(pid, start, name)| (Some(pid), start, name))
                    .unwrap_or((None, None, None));
                Ok(json!({
                    "task": wire.wire,
                    "scope": scope,
                    "pane_id": self_pane_id,
                    "lease_seconds": crate::github::python_float_json_value(lease_seconds)
                        .expect("claimed lease cannot be NaN"),
                    "expires_at": crate::github::python_float_json_value(expires_at)
                        .expect("claimed lease expiry cannot be NaN"),
                    "caller": {"pid": pid, "start": start, "name": name},
                }))
            }
            ClaimOutcome::Refused { scope, refusal } => {
                let mut refused = json!({
                    "reason": refusal.reason,
                    "task_ids": refusal.task_ids,
                });
                if let Some(owned_by) = refusal.owned_by {
                    refused["owned_by"] = json!(owned_by);
                }
                let mut response = json!({
                    "task": Value::Null,
                    "scope": scope,
                    "pane_id": self_pane_id,
                    "refused": refused,
                });
                if following {
                    response["following"] = json!(true);
                }
                Ok(response)
            }
        }
    }

    pub async fn task_release_wire(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        let task = self.task_release(task_id, self_pane_id).await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    /// Preserve `task_add`'s role/default/refusal order, then read typed+wire together.
    pub async fn task_add_wire(
        &self,
        title: &str,
        scope: Option<&str>,
        body: Option<&str>,
        opts: TaskAddArgs<'_>,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<TaskWire, DunkError> {
        let task = self
            .task_add(title, scope, body, opts, self_pane_id, env)
            .await?;
        self.task_store()?
            .get_full_wire(&task.id, self_pane_id)
            .await
    }

    /// `Flock.task_list`: None/zero default to 200; negative limits keep Python slicing.
    #[allow(clippy::too_many_arguments)]
    pub async fn task_list_wire(
        &self,
        scope: Option<&str>,
        status: Option<&str>,
        assignee: Option<&str>,
        include_done: bool,
        limit: Option<i64>,
        self_pane_id: Option<&str>,
        kind: Option<&str>,
        needs_owner: bool,
    ) -> Result<(Option<String>, Vec<DecoratedWire>), DunkError> {
        let tasks = self.task_store()?;
        let explicit = match scope.map(str::trim) {
            None | Some("") | Some("all") => None,
            Some(_) => Some(Self::task_scope(scope, self_pane_id, "global")?),
        };
        let limit = limit.filter(|n| *n != 0).unwrap_or(200);
        let rows = tasks
            .list_visible_wire(
                explicit.as_deref(),
                status,
                assignee,
                include_done,
                Some(limit),
                self_pane_id,
                kind,
                needs_owner,
            )
            .await?;
        Ok((explicit, rows))
    }

    /// Authorize and read full row/notes inside the store's single snapshot callback.
    pub async fn task_read_wire_with_notes(
        &self,
        task_id: &str,
        self_pane_id: Option<&str>,
    ) -> Result<TaskWireNotes, DunkError> {
        self.task_store()?
            .read_visible_wire_with_notes(task_id, self_pane_id)
            .await
    }
}

#[cfg(test)]
mod tests;
