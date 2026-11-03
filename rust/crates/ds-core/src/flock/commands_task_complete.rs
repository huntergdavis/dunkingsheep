//! T35 slice 3: source-ordered task completion guards, the committed NextJobs handoff,
//! landed state, and the contained ReviewSync verdict hook.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use super::panes::PaneInfoEnv;
use super::Flock;
use crate::error::DunkError;
use crate::proc::{caller_pane_auth_failure, Caller};
use crate::review::ConfiguredReviewPolicy;
use crate::superusers::FileAllowlist;
use crate::tasks::advance::OperatorProof;
use crate::tasks::{validate_verdict, Decorated, ReviewFacts, Task, Tasks};
use serde_json::{json, Value};

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
    /// Complete a task through the request's shared next-job manager. The request context is
    /// captured synchronously by the dispatcher; the identity-bound caller is copied before
    /// driving the async store work on that same request thread.
    pub fn task_complete(
        &self,
        context: &crate::api::HandlerContext<'_>,
        task_id: &str,
        by: Option<&str>,
        verdict: Option<&str>,
        landed: bool,
    ) -> Result<Value, DunkError> {
        if !std::ptr::eq(self, context.flock) {
            return refuse("task completion request flock mismatch");
        }
        let caller = context.request.caller.clone();
        let self_pane_id = context.request.self_pane_id.clone();
        let task_id = task_id.to_owned();
        let by = by.map(str::to_owned);
        let verdict = verdict.map(str::to_owned);
        context.block_on(self.task_complete_async(
            context,
            &task_id,
            by.as_deref(),
            verdict.as_deref(),
            landed,
            caller,
            self_pane_id,
        ))?
    }

    async fn task_complete_async(
        &self,
        context: &crate::api::HandlerContext<'_>,
        task_id: &str,
        by: Option<&str>,
        requested_verdict: Option<&str>,
        landed: bool,
        caller: Option<Caller>,
        self_pane_id: Option<String>,
    ) -> Result<Value, DunkError> {
        let tasks = self.task_store()?;
        let visible = tasks
            .get_full_wire(task_id, self_pane_id.as_deref())
            .await?;
        let original_task = visible.wire.clone();
        let task = visible.decorated;
        let is_review = task.task.kind == "review";

        let role = if is_review {
            Some(self.caller_role(self_pane_id.as_deref(), context.env)?)
        } else {
            None
        };
        let facts = review_facts_from_wire(&visible.wire);
        let empty_worked = BTreeSet::new();
        let may_review = if role.as_deref() == Some("reviewer") {
            match self_pane_id.as_deref().filter(|pane| !pane.is_empty()) {
                Some(pane) => {
                    self.with_roles(|roles, _| roles.may_review(pane, role.as_deref()))?
                }
                None => false,
            }
        } else {
            false
        };
        let role_caller = role.as_deref().map(|role| ReviewCaller {
            pane: self_pane_id.as_deref(),
            role,
            may_review,
            worked: Ok(&empty_worked),
        });
        if is_review {
            let who = role_caller
                .as_ref()
                .ok_or_else(|| DunkError::Refused("review caller role is unavailable".into()))?;
            Self::check_review_role(&task, who)?;
        } else if requested_verdict.is_some() {
            return refuse("--verdict applies to review tasks only");
        }

        let worked = if is_review
            && role.as_deref() == Some("reviewer")
            && self_pane_id.as_deref().is_some_and(|pane| !pane.is_empty())
        {
            match (facts.repo.as_deref(), facts.pr) {
                (Some(repo), Some(pr)) => tasks
                    .authors_of(repo, pr)
                    .await
                    .map(|authors| authors.into_iter().collect())
                    .map_err(|e| e.to_string()),
                _ => Ok(empty_worked.clone()),
            }
        } else {
            Ok(empty_worked.clone())
        };
        let mut validated_verdict = None;
        if is_review {
            let role = role
                .as_deref()
                .ok_or_else(|| DunkError::Refused("review caller role is unavailable".into()))?;
            let review_caller = ReviewCaller {
                pane: self_pane_id.as_deref(),
                role,
                may_review,
                worked: match &worked {
                    Ok(worked) => Ok(worked),
                    Err(error) => Err(error),
                },
            };
            // Keep the source order: role/provider and authorship/verdict gates, then evidence,
            // and only then stale/cancelled review state.
            validated_verdict = Self::check_review_caller_and_verdict(
                &task,
                &facts,
                requested_verdict,
                &review_caller,
            )?;
            Self::require_review_evidence(
                tasks.as_ref(),
                &task.task,
                self_pane_id.as_deref(),
                role,
            )
            .await?;
            Self::check_review_head_unjudged(&task, &facts)?;
        }

        // Match Python's order: pane lookup, operator proof, ownership/binding, then mutation.
        let pane_info = self_pane_id
            .as_deref()
            .filter(|pane| !pane.is_empty())
            .and_then(|pane| context.env.get_pane(pane));
        let role = match role {
            Some(role) => role,
            None => self.caller_role(self_pane_id.as_deref(), context.env)?,
        };
        let operator = matches!(role.as_str(), "owner" | "system");
        self.authorize_task_complete(
            &task,
            self_pane_id.as_deref(),
            operator,
            context.env,
            caller.as_ref(),
        )?;
        let next_jobs = context.env.next_jobs()?;
        if !next_jobs.owns_flock(self) {
            return refuse("request next-job flock mismatch");
        }
        let allowlist_path = self.with_roles(|roles, legacy| {
            Ok(legacy
                .map(|allowlist| allowlist.path.clone())
                .unwrap_or_else(|| roles.path.clone()))
        })?;
        let pane = self_pane_id
            .as_deref()
            .filter(|pane| !pane.is_empty())
            .ok_or_else(|| DunkError::Refused("task completion needs a pane identity".into()))?;
        let operator_proof = Some(OperatorProof {
            pane_id: pane.to_owned(),
            pane_info,
            allowlist: Arc::new(FileAllowlist(allowlist_path)),
        });
        let actor = self_pane_id
            .as_deref()
            .filter(|pane| !pane.is_empty())
            .or(by);

        if let Some(verdict) = validated_verdict.as_deref() {
            tasks.set_verdict(task_id, verdict, actor).await?;
        }
        let advance = next_jobs
            .advance(pane, "completion", Some(task_id), caller, operator_proof)
            .await?;
        self.wake_push();

        let mut result = tasks
            .get_full_wire(task_id, self_pane_id.as_deref())
            .await?
            .wire
            .as_object()
            .cloned()
            .ok_or_else(|| DunkError::Refused("completed task is not an object".into()))?;
        let mut next_job = advance.plan;
        next_job.remove("task");
        result.insert("next_job".into(), Value::Object(next_job));
        if !landed {
            tasks.set_landed(task_id, false, actor, None, None).await?;
            result.insert("landed".into(), json!(false));
        }
        if let Some(verdict) = validated_verdict.as_deref() {
            let github = self
                .contain_review_verdict(&original_task, async {
                    let tokens = context
                        .env
                        .github_tokens()
                        .map_err(|error| error.to_string())?;
                    let transport = context
                        .env
                        .review_transport()
                        .map_err(|error| error.to_string())?;
                    let policy = ConfiguredReviewPolicy {
                        config_dir: tokens.config_dir().to_path_buf(),
                    };
                    Ok(self
                        .after_review_verdict(
                            tokens,
                            policy,
                            tokens.api_root(),
                            super::commands_review::BorrowedGitHubTransport(transport),
                            &original_task,
                            verdict,
                            actor,
                        )
                        .await)
                })
                .await;
            result.insert("github".into(), github);
        }
        Ok(Value::Object(result))
    }

    /// Error boundary retained from the partial PR629 adapter. The actual
    /// `_after_review_verdict` hook is the ReviewSync-backed method in `commands_review`.
    pub async fn contain_review_verdict<F>(
        &self,
        task: &serde_json::Value,
        verdict: F,
    ) -> serde_json::Value
    where
        F: Future<Output = Result<serde_json::Value, String>>,
    {
        match verdict.await {
            Ok(result) => result,
            Err(error) => {
                let task_id = task
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("None");
                eprintln!("review verdict side effects failed for {task_id}: {error}");
                serde_json::json!({"errors": [error]})
            }
        }
    }

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

    /// The review-completion gates of `task_complete` in the Python's order: the first half (caller
    /// role, no-self-review, verdict required and validated), then [`Flock::require_review_evidence`]
    /// (`_require_review_evidence`, needs the task notes), then the second half
    /// ([`Flock::check_review_head_unjudged`]: cancelled, already done). This composes the two
    /// halves WITHOUT the evidence step in between, so the real `task_complete` must call the halves
    /// itself and put the evidence check between them: running evidence after this function would
    /// reorder the refusals a lane sees (t656 vs t621). Returns the validated verdict. `verdict` on a
    /// non-review task is a refusal (the `elif` branch).
    pub fn check_task_completion_gates(
        task: &Decorated,
        facts: &ReviewFacts,
        verdict: Option<&str>,
        who: &ReviewCaller<'_>,
    ) -> Result<Option<String>, DunkError> {
        let verdict = Self::check_review_caller_and_verdict(task, facts, verdict, who)?;
        if task.task.kind == "review" {
            Self::check_review_head_unjudged(task, facts)?;
        }
        Ok(verdict)
    }

    /// First half of the gates: everything before `_require_review_evidence`.
    pub fn check_review_caller_and_verdict(
        task: &Decorated,
        facts: &ReviewFacts,
        verdict: Option<&str>,
        who: &ReviewCaller<'_>,
    ) -> Result<Option<String>, DunkError> {
        let t = &task.task;
        Self::check_review_role(task, who)?;
        if t.kind != "review" {
            return match verdict {
                Some(_) => refuse("--verdict applies to review tasks only"),
                None => Ok(None),
            };
        }
        let pane = who.pane.unwrap_or("");
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
        Ok(Some(verdict))
    }

    fn check_review_role(task: &Decorated, who: &ReviewCaller<'_>) -> Result<(), DunkError> {
        if task.task.kind != "review" {
            return Ok(());
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
        Ok(())
    }

    /// Second half of the gates: one verdict per head (t656). A cancelled task is not a place to put
    /// a verdict, and a done one already carries the verdict its merge record was built on.
    pub fn check_review_head_unjudged(
        task: &Decorated,
        facts: &ReviewFacts,
    ) -> Result<(), DunkError> {
        let t = &task.task;
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
        Ok(())
    }
}

fn review_facts_from_wire(wire: &Value) -> ReviewFacts {
    ReviewFacts {
        author: wire
            .get("author")
            .and_then(Value::as_str)
            .map(str::to_owned),
        repo: wire.get("repo").and_then(Value::as_str).map(str::to_owned),
        pr: wire.get("pr").and_then(Value::as_i64),
        verdict: wire
            .get("verdict")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

#[cfg(test)]
#[path = "commands_task_complete_tests.rs"]
mod tests;
