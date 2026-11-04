//! Poolrad uses the canonical attached Tasks actor and startup path snapshot.
//! Exporter and git work run on the standard request thread, outside store calls.

use serde_json::Value;
use tokio::runtime::{Handle, RuntimeFlavor};

use super::RequestRuntimeAdapter;
use crate::error::{refusal, DunkError};
use crate::poolrad::{PoolradConfig, PoolradSync, PoolradTaskStore};
use crate::tasks::{AddOptions, Tasks};

impl RequestRuntimeAdapter<'_> {
    fn poolrad_config(&self) -> Result<PoolradConfig, DunkError> {
        let state = self.terminal.flock.status_state()?;
        PoolradConfig::from_env(&state.env, &state.python_home).map_err(refusal)
    }

    fn require_poolrad_thread(&self) -> Result<(), DunkError> {
        if Handle::try_current().is_ok() {
            return Err(refusal(
                "async command work requires a dedicated blocking request thread",
            ));
        }
        // Handle::block_on cannot drive a current-thread runtime's IO/timers.
        if self.runtime.runtime_flavor() != RuntimeFlavor::MultiThread {
            return Err(refusal(
                "Poolrad requires an independently driven multithread runtime",
            ));
        }
        Ok(())
    }

    pub(super) fn runtime_poolrad_sync(
        &self,
        limit: usize,
        dry_run: bool,
    ) -> Result<Value, DunkError> {
        self.require_poolrad_thread()?;
        let config = self.poolrad_config()?;
        PoolradSync::from_config(&RuntimePoolradTasks(self), &config)
            .run_with(limit, dry_run, "medium")
            .map_err(refusal)
    }

    pub(super) fn runtime_poolrad_sync_args(
        &self,
        args: &crate::api::Args,
    ) -> Result<Value, DunkError> {
        self.require_poolrad_thread()?;
        // dunk_core imports dunk_poolrad (and evaluates Path.home()) before int(limit).
        let config = self.poolrad_config()?;
        let limit = crate::api::poolrad_limit(args)?;
        let dry_run = crate::api::poolrad_dry_run(args.get("dry_run"));
        PoolradSync::from_config(&RuntimePoolradTasks(self), &config)
            .run_with(limit, dry_run, "medium")
            .map_err(refusal)
    }

    pub(super) fn runtime_poolrad_progress(&self) -> Result<Value, DunkError> {
        self.require_poolrad_thread()?;
        let config = self.poolrad_config()?;
        crate::poolrad::progress(
            Some(&config.repo),
            Some(&config.queue_path),
            Some(&config.credit_path),
        )
        .map_err(refusal)
    }
}

struct RuntimePoolradTasks<'a, 'b>(&'a RequestRuntimeAdapter<'b>);

impl RuntimePoolradTasks<'_, '_> {
    fn tasks(&self) -> Result<std::sync::Arc<Tasks>, String> {
        self.0
            .require_poolrad_thread()
            .map_err(|error| error.to_string())?;
        // Resolve only at the engine's list/add call, after export and git.
        self.0
            .terminal
            .flock
            .stores()
            .tasks
            .ok_or_else(|| "the task store is not attached".to_owned())
    }
}

impl PoolradTaskStore for RuntimePoolradTasks<'_, '_> {
    fn list(
        &self,
        scope: Option<&str>,
        include_done: bool,
        limit: usize,
        kind: &str,
    ) -> Result<Vec<Value>, String> {
        let tasks = self.tasks()?;
        let limit = i64::try_from(limit).map_err(|error| error.to_string())?;
        self.0
            .runtime
            .block_on(tasks.list_visible_wire(
                scope,
                None,
                None,
                include_done,
                Some(limit),
                None,
                Some(kind),
                false,
            ))
            .map(|rows| rows.into_iter().map(|row| row.wire).collect())
            .map_err(|error| error.to_string())
    }

    fn add(
        &self,
        scope: &str,
        title: &str,
        body: &str,
        priority: &str,
        kind: &str,
        created_by: &str,
    ) -> Result<Value, String> {
        let tasks = self.tasks()?;
        self.0.runtime.block_on(async {
            let task = tasks
                .add_with(
                    scope,
                    title,
                    body,
                    AddOptions {
                        priority: Some(priority.to_owned()),
                        kind: Some(kind.to_owned()),
                        created_by: Some(created_by.to_owned()),
                        ..AddOptions::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            // Return the real full dictionary. A readback failure does not undo
            // the committed creation; expose its id instead of claiming no write.
            tasks
                .get_full_wire(&task.id, None)
                .await
                .map(|row| row.wire)
                .map_err(|error| format!("task {} created, but readback failed: {error}", task.id))
        })
    }
}

#[cfg(test)]
mod tests;
