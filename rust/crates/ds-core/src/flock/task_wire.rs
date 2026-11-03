//! Full task dictionaries for API adapters; request identity uses the existing Flock path.

use super::{commands_tasks::TaskAddArgs, panes::PaneInfoEnv, Flock};
use crate::{
    error::DunkError,
    tasks::wire::{DecoratedWire, TaskWire, TaskWireNotes},
};

impl Flock {
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
