//! t3583 (child C of t1982): the guarded batch claim, `task_batch_claim`.
//!
//! The route claims an explicit, ordered list of tasks for the caller, all or nothing, through
//! `Tasks::claim_batch` (child A), with the batch's recording and lease lifecycle (child B). It is
//! gated to the owner and to workers: any other caller is refused before a single write. The
//! kind and author filters are the ones `task_next` applies to the caller's role, on every
//! member. `task_next` and its `--allow-busy` behaviour are unchanged.

use serde_json::{json, Value};

use super::panes::PaneInfoEnv;
use super::Flock;
use crate::error::DunkError;
use crate::tasks::{ClaimBatchOutcome, ClaimOptions};

impl Flock {
    /// `task_batch_claim`: claim every task in `ids` for the caller, or none of them.
    ///
    /// The role is read once, before the first await (the t1513 thread contract), and gates the
    /// route. A refusal is returned as data, like `task_next`'s: `{batch_id: null, tasks: [],
    /// refused: {reason, task_ids}}`, so a caller can tell "nothing claimable" from a fault.
    pub async fn task_batch_claim_wire(
        &self,
        env: &dyn PaneInfoEnv,
        ids: &[String],
        lease_seconds: Option<f64>,
        allow_busy: bool,
        caller: Option<(i64, Option<String>, Option<String>)>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        let pane = self_pane_id
            .filter(|p| !p.is_empty())
            .ok_or_else(|| DunkError::Refused("task_batch_claim requires a caller pane".into()))?;
        let role = self.caller_role(Some(pane), env)?;
        if !matches!(role.as_str(), "owner" | "worker") {
            return Err(DunkError::Refused(format!(
                "task_batch_claim is for workers and the owner; {pane} is a {role}"
            )));
        }
        let (kinds, exclude_author) = Self::filters_for_role(&role, Some(pane));
        let store = self.task_store()?;
        let outcome = store
            .claim_batch(
                ids,
                pane,
                ClaimOptions {
                    lease_seconds,
                    caller: caller.clone(),
                    allow_busy,
                    kinds,
                    exclude_author,
                },
            )
            .await?;
        match outcome {
            ClaimBatchOutcome::Claimed {
                batch_id,
                tasks,
                lease_seconds,
                expires_at,
            } => {
                let mut wires = Vec::with_capacity(tasks.len());
                for task in &tasks {
                    wires.push(store.get_full_wire(&task.id, Some(pane)).await?.wire);
                }
                let (pid, start, name) = caller
                    .map(|(pid, start, name)| (Some(pid), start, name))
                    .unwrap_or((None, None, None));
                Ok(json!({
                    "batch_id": batch_id,
                    "tasks": wires,
                    "pane_id": pane,
                    "lease_seconds": crate::github::python_float_json_value(lease_seconds)
                        .expect("claimed lease cannot be NaN"),
                    "expires_at": crate::github::python_float_json_value(expires_at)
                        .expect("claimed lease expiry cannot be NaN"),
                    "caller": {"pid": pid, "start": start, "name": name},
                }))
            }
            ClaimBatchOutcome::Refused { refusal } => {
                let mut refused = json!({
                    "reason": refusal.reason,
                    "task_ids": refusal.task_ids,
                });
                if let Some(owned_by) = refusal.owned_by {
                    refused["owned_by"] = json!(owned_by);
                }
                Ok(json!({
                    "batch_id": Value::Null,
                    "tasks": [],
                    "pane_id": pane,
                    "refused": refused,
                }))
            }
        }
    }
}
