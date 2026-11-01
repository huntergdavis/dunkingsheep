//! T35 slice 3 (first part): the guards of `Flock.task_complete` that run BEFORE any mutation:
//! `_authorize_task_complete`, `_require_completion_binding` and the review-completion gates
//! (role, no-self-merge, verdict, cancelled/done). NOT here: the wiring (`set_verdict`,
//! `advance_next_job`, `set_landed`), `_require_review_evidence` (needs `Tasks::notes`), the
//! `authors_of` store read, `_after_review_verdict` (the GitHub hook).

use std::collections::BTreeSet;

use super::panes::PaneInfoEnv;
use super::Flock;
use crate::error::DunkError;
use crate::proc::{caller_pane_auth_failure, Caller};
use crate::tasks::{validate_verdict, Decorated, ReviewFacts, Task, Tasks};

fn refuse<T>(text: impl Into<String>) -> Result<T, DunkError> {
    Err(DunkError::Refused(text.into()))
}

/// What the review gates read about the completing caller.
#[derive(Debug, Clone)]
pub struct ReviewCaller<'a> {
    pub pane: Option<&'a str>,
    pub role: &'a str,
    /// `roles.may_review(pane, role=role)`.
    pub may_review: bool,
    /// `tasks.authors_of(repo, pr)`: lanes that worked on this PR, or the store's error text when it
    /// could not answer. The caller passes an empty set when the task has no repo+pr (the Python
    /// never queries then). `Err` makes a reviewer's completion FAIL CLOSED (t723): an unverified
    /// reviewer is the bug the no-self-review guard exists to stop, so it must never be defaulted to
    /// "no authors" by a caller that swallowed the error.
    pub worked: Result<&'a BTreeSet<String>, &'a str>,
}

impl Flock {
    /// `_require_completion_binding`: refuse a pane claim the kernel evidence does not support.
    /// `caller` is `current_caller()` as a `Caller`; a request that arrived over the socket is
    /// never trusted on its labels, so every shape the kernel cannot check refuses, while a
    /// request with no peer (in-process code, tests) keeps label policy.
    pub fn require_completion_binding(
        &self,
        self_pane: &str,
        env: &dyn PaneInfoEnv,
        caller: Option<&Caller>,
    ) -> Result<(), DunkError> {
        let socket = self.socket_peer_ok();
        if socket == Some(false) {
            eprintln!("task completion refused reason=unverified_peer pane={self_pane:?}");
            return refuse(format!(
                "pane {self_pane} cannot be verified (socket peer credentials unavailable); a \
                 completion on an unauthenticated transport is refused. Retry when credentials \
                 resolve, or complete from a live lane pane."
            ));
        }
        let Some(caller) = caller else {
            return Ok(());
        };
        let socket_request = socket == Some(true);
        if !std::path::Path::new("/proc").is_dir() {
            if socket_request {
                eprintln!("task completion refused reason=no_proc_table pane={self_pane:?}");
                return refuse(format!(
                    "pane {self_pane} cannot be verified (no process table on this transport); \
                     a completion without kernel evidence is refused."
                ));
            }
            return Ok(());
        }
        let Some(pane) = env.get_pane(self_pane) else {
            if !socket_request {
                return Ok(());
            }
            eprintln!("task completion refused reason=unknown_pane pane={self_pane:?}");
            return refuse(format!(
                "pane {self_pane} is not visible on this backend; a completion naming an \
                 unresolvable pane is refused. Run the command from a live lane pane, or have a \
                 superuser (CoS/VP operator) reassign it first."
            ));
        };
        let pane_pid = pane.get("pane_pid").and_then(serde_json::Value::as_i64);
        if pane_pid.is_none() {
            if !socket_request {
                return Ok(());
            }
            eprintln!("task completion refused reason=pane_pid_missing pane={self_pane:?}");
            return refuse(format!(
                "pane {self_pane} reports no process id, so the calling process cannot be bound \
                 to it; the completion is refused rather than taken on the label alone. Retry \
                 when the backend reports pane processes."
            ));
        }
        if let Some(reason) = caller_pane_auth_failure(Some(caller), pane_pid) {
            eprintln!(
                "task completion refused reason=spoofed_pane pane={self_pane:?} binding={reason}"
            );
            return refuse(format!(
                "the calling process is not inside pane {self_pane} (caller binding failed: \
                 {reason}); a pane label alone does not authorize completion. Run the command \
                 from the lane that owns the task."
            ));
        }
        Ok(())
    }

    /// `_authorize_task_complete`: refuse foreign completion BEFORE any mutation (t423). The
    /// requesting pane must own the task (assignee or live lease-holder) or be an operator;
    /// unattributed callers complete nothing.
    pub fn authorize_task_complete(
        &self,
        task: &Decorated,
        self_pane_id: Option<&str>,
        operator: bool,
        env: &dyn PaneInfoEnv,
        caller: Option<&Caller>,
    ) -> Result<(), DunkError> {
        let owners: BTreeSet<&str> = task
            .assignees
            .iter()
            .chain(task.claimed_by.iter())
            .map(String::as_str)
            .collect();
        let Some(pane) = self_pane_id.filter(|p| !p.is_empty()) else {
            eprintln!(
                "task completion refused reason=anonymous task={:?}",
                task.task.id
            );
            return refuse(format!(
                "task {} cannot be completed without a pane identity, so ownership cannot be \
                 checked. Pass --self-pane <pane> from the lane that owns it, or have a \
                 superuser (CoS/VP operator) complete it.",
                task.task.id
            ));
        };
        self.require_completion_binding(pane, env, caller)?;
        if owners.contains(pane) || operator {
            return Ok(());
        }
        eprintln!(
            "task completion refused reason=foreign_owner task={:?} pane={pane:?}",
            task.task.id
        );
        let named = owners.iter().copied().collect::<Vec<_>>().join(", ");
        refuse(format!(
            "task {} is owned by {}; {pane} may not complete it. Complete your own tasks, or \
             have a superuser (CoS/VP operator) complete it.",
            task.task.id,
            if named.is_empty() {
                "no one recorded"
            } else {
                &named
            }
        ))
    }

    /// `_require_review_evidence`: refuse a verdict unless this pane noted its evidence AFTER it
    /// claimed (a note written before the review started is a plan, not a finding). An owner
    /// or system caller with no lease is exempt: it is not the lane that judged the work.
    /// `role` is the caller's already-resolved role (`anonymous` with no pane).
    pub async fn require_review_evidence(
        tasks: &Tasks,
        task: &Task,
        self_pane_id: Option<&str>,
        role: &str,
    ) -> Result<(), DunkError> {
        let pane = self_pane_id.filter(|p| !p.is_empty());
        let mut lease = None;
        if let Some(pane) = pane {
            lease = tasks
                .leases_for_pane(pane, None)
                .await?
                .into_iter()
                .find(|l| l.task_id == task.id);
        }
        if lease.is_none() && matches!(role, "owner" | "system") {
            return Ok(());
        }
        let claimed_at = lease
            .as_ref()
            .map(|l| l.claimed_at)
            .unwrap_or(task.created_at);
        let noted =
            tasks.notes(&task.id).await?.into_iter().any(|n| {
                n.actor.as_deref() == pane && n.at >= claimed_at && !n.text.trim().is_empty()
            });
        if noted {
            return Ok(());
        }
        refuse(format!(
            "a verdict needs evidence: record what you ran and what you did NOT verify first, \
             with `dunkingsheep task note {} --reason \"<commands, head sha, what is not \
             verified>\"`. A note by another lane, or one written before you claimed, does not \
             count.",
            task.id
        ))
    }

    /// The review-completion gates of `task_complete`, in the Python's order. Returns the
    /// validated verdict. `verdict` on a non-review task is a refusal (the `elif` branch).
    pub fn check_task_completion_gates(
        task: &Decorated,
        facts: &ReviewFacts,
        verdict: Option<&str>,
        who: &ReviewCaller<'_>,
    ) -> Result<Option<String>, DunkError> {
        let t = &task.task;
        if t.kind != "review" {
            return match verdict {
                Some(_) => refuse("--verdict applies to review tasks only"),
                None => Ok(None),
            };
        }
        let pane = who.pane.unwrap_or("");
        if who.role == "reviewer" && !who.may_review {
            return refuse(format!(
                "{pane} may not review (provider not on the reviewer list)"
            ));
        }
        if who.role != "reviewer" && who.role != "owner" {
            return refuse(format!(
                "a review is completed by its reviewer lane, not role {}",
                who.role
            ));
        }
        if who.role == "reviewer" && !pane.is_empty() {
            if facts
                .author
                .as_deref()
                .is_some_and(|a| !a.is_empty() && a == pane)
            {
                return refuse("a lane never accepts its own work (no-self-merge)");
            }
            let worked = match who.worked {
                Ok(worked) => worked,
                Err(error) => {
                    return refuse(format!(
                        "cannot verify that {pane} is not the author of {}#{}: {error}. \
                         no-self-review is checked before a verdict is recorded, so this review \
                         is refused until the task store answers.",
                        facts.repo.as_deref().unwrap_or("None"),
                        facts
                            .pr
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "None".into())
                    ));
                }
            };
            if worked.contains(pane) {
                return refuse(format!(
                    "a lane never accepts its own work (no-self-merge): {pane} worked on {}#{} \
                     (task author, assignee or completer), so it cannot review this head. \
                     Another reviewer lane completes {}.",
                    facts.repo.as_deref().unwrap_or("None"),
                    facts
                        .pr
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "None".into()),
                    t.id
                ));
            }
        }
        let Some(verdict) = verdict else {
            return refuse("completing a review needs --verdict accept|needs-fix");
        };
        let verdict = validate_verdict(verdict)?;
        if t.status == "cancelled" {
            return refuse(format!(
                "{} is cancelled: a cancelled review takes no verdict. If the head still needs \
                 judging, the review timer files a review for it.",
                t.id
            ));
        }
        if t.status == "done" {
            return refuse(format!(
                "{} is already done with verdict {}: a verdict is bound to the head it measured, \
                 so a second one is refused rather than overwriting the merge record",
                t.id,
                crate::context::store::py_repr(facts.verdict.as_deref().unwrap_or(""))
            ));
        }
        Ok(Some(verdict))
    }
}

#[cfg(test)]
#[path = "commands_task_complete_tests.rs"]
mod tests;
