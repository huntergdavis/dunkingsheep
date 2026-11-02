//! Startup-owned status facts. Final daemon wiring must attach these explicitly (t1605).
//! Sources: `dunk_core.Flock.__init__`, `_quarantine_shared_mailboxes`, and the accepted
//! binary identity contract in `flock::status::CodeIdentity`.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::{status::CodeIdentity, Flock};
use crate::{config::Env, error::DunkError, terminal::Terminal};

/// A snapshot captured once by startup, never synthesized by a status request.
#[derive(Debug)]
pub struct StatusState {
    pub(crate) version: String,
    pub(crate) started_at: f64,
    pub(crate) pid: u32,
    pub(crate) federated_input: bool,
    pub(crate) code: CodeIdentity,
    pub(crate) state_file: PathBuf,
    pub(crate) env: Env,
}

impl StatusState {
    /// `state_file` is the configured whole-state path, distinct from launches.json.
    /// `version` comes from the daemon's release configuration, not the crate version.
    pub fn at_start(
        version: impl Into<String>,
        state_file: impl Into<PathBuf>,
        executable: impl Into<PathBuf>,
        env: Env,
    ) -> Self {
        Self {
            version: version.into(),
            started_at: crate::clock::now_s(),
            pid: std::process::id(),
            federated_input: env.get("DUNKINGSHEEP_FEDERATED_INPUT") == Some("1"),
            code: CodeIdentity::at_start(executable),
            state_file: state_file.into(),
            env,
        }
    }
}

impl Flock {
    /// Attach the real startup snapshot exactly once. `Flock::new` intentionally leaves it
    /// unconfigured: its legacy arguments contain neither release nor whole-state path.
    pub fn attach_status_state(&self, state: StatusState) -> Result<(), DunkError> {
        self.status_runtime
            .set(state)
            .map_err(|_| DunkError::Refused("status startup state is already configured".into()))
    }

    pub(crate) fn status_state(&self) -> Result<&StatusState, DunkError> {
        self.status_runtime
            .get()
            .ok_or_else(|| DunkError::Refused("status startup state is not configured".into()))
    }

    /// Live command/loop owners publish their actual completed operation here.
    pub fn set_last_message(&self, message: impl Into<String>) {
        *self
            .last_message
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = message.into();
        self.invalidate_view();
    }

    pub(crate) fn last_message(&self) -> String {
        self.last_message
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn quarantined_mailboxes(&self) -> Option<Value> {
        let moved = self
            .quarantined_mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!moved.is_empty()).then(|| json!(*moved))
    }

    /// Startup migration, using the selected backend and actual inbox writer. A backend
    /// failure leaves mail alone; a single failed rename does not block other mailboxes.
    /// Call after attaching stores and before accepting requests (t1605).
    pub fn quarantine_shared_mailboxes(&self, terminal: &dyn Terminal) -> Result<(), DunkError> {
        let store = self
            .stores()
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store is not wired".into()))?;
        let panes = match terminal.try_list_panes() {
            Ok(panes) => panes,
            Err(_) => return Ok(()),
        };
        let types: std::collections::BTreeSet<String> = panes
            .iter()
            .map(|pane| crate::terminal::parse::agent_kind(&pane.command, &pane.title))
            .filter(|agent| !agent.is_empty())
            .map(|agent| agent.to_lowercase())
            .collect();
        let mut moved = Vec::new();
        for old in types {
            let new = format!("unrouted:{old}");
            if let Ok(count) = store.rename_mailbox(&old, &new) {
                if count > 0 {
                    moved.push(json!({"was": old, "now": new, "messages": count}));
                }
            }
        }
        *self
            .quarantined_mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = moved;
        self.invalidate_view();
        Ok(())
    }
}
