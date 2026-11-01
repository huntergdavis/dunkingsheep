//! `flock/loops_push.rs` (T36c, slice 1): the task push loop, from `dunk_core._task_push_loop`,
//! `_push_tasks`, `_push_targets`, `_claim_filters`, `_may_pull_everything` and the next-job
//! wake (`_next_job_wake`).
//!
//! One SWEEP ([`Flock::push_tasks`]) is a plain async fn over a [`PushEnv`] (the backend, the role
//! registry, the inbox and the process table the Python reaches through `self`), so a test drives
//! it without a daemon or a timer. The loop ([`run_push`]) is a tokio task: recover at start, then
//! wake on the push notifier (turn end, task completion: t615 "push on turn end AND on the
//! sweep") or after `DUNKINGSHEEP_PUSH_S` (default 10 s), then allocator tick, next-jobs tick,
//! sweep; it stops on a signal (`self._task_stop`).
//!
//! t615 (measured 2026-10-29, owner): four registered lanes sat idle for ~40 minutes with 13 open
//! tasks because only ONE of them had ever called `task follow`. Registration is the subscription
//! now. Each decision is returned (and logged), because "nothing to do" and "I was skipped" must
//! never look alike.
//!
//! NOT PORTED here (named hooks on [`PushEnv`], default no-ops until their owners land):
//! `next_jobs.recover()` / `next_jobs.tick()` (NextJobs has no Rust owner; `tasks/next_job.rs`
//! holds only the handoff store), `allocator.tick()` (t820, in review), and the real
//! `_unread_mail` / `_lane_agent_alive` / `_announce_task` wiring over the inbox store and the
//! process table (the env supplies them). `_stuck_observer_loop` is the next slice.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::Flock;
use crate::error::DunkError;
use crate::push::{push_targets, Via, BUSY_STATUSES};
use crate::tasks::{ClaimOptions, ClaimOutcome, Task};

/// `TASK_PUSH_S` (`DUNKINGSHEEP_PUSH_S`, default 10 s), read per call here.
pub fn push_interval() -> Duration {
    let secs = std::env::var("DUNKINGSHEEP_PUSH_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(10.0);
    Duration::from_secs_f64(secs)
}

/// What one push sweep reaches outside the task store.
pub trait PushEnv: Send + Sync {
    /// `self.herdr.list_panes()`.
    fn list_panes(&self) -> Result<Vec<Value>, DunkError>;
    /// `caller_role(pane)`: the RESOLVED role (t975), not the registry string.
    fn caller_role(&self, pane_id: &str) -> String;
    /// `roles.describe()["lanes"]` keys; `None` when the registry is unavailable (followers only).
    fn registered_lanes(&self) -> Option<BTreeSet<String>>;
    /// `roles.may_review(pane, role="reviewer")`.
    fn may_review(&self, pane_id: &str) -> bool;
    /// `roles.provider_of(pane)`.
    fn provider_of(&self, pane_id: &str) -> Option<String>;
    /// `_lane_agent_alive`: `Some(false)` is evidence of a dead lane, `None` is "cannot say".
    fn lane_agent_alive(&self, pane_id: &str) -> Option<bool>;
    /// `_unread_mail`: mail the lane must act on holds it back.
    fn unread_mail(&self, pane_id: &str) -> bool;
    /// `_announce_task`: the inbox message and `last_message`.
    fn announce_task(&self, pane_id: &str, task: &Task, lease_seconds: f64);
    /// `self.next_jobs.recover()` at loop start.
    fn recover_next_jobs(&self) {}
    /// `self.allocator.tick()`.
    fn allocator_tick(&self) {}
    /// `self.next_jobs.tick()`.
    fn next_jobs_tick(&self) {}
}

/// One decision of a sweep, the same words the Python logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushDecision {
    pub pane_id: String,
    /// `claim`, `none`, `skip` or `claim_failed`.
    pub action: &'static str,
    /// The reason (`busy_status`, `holding_work`, ...) or the claimed task id.
    pub detail: String,
}

/// `_claim_filters(pane)`: `(kinds, exclude_author)` from the caller's role.
pub fn claim_filters(role: &str, pane_id: &str) -> (Option<Vec<String>>, Option<String>) {
    let owned = |kinds: &[&str]| kinds.iter().map(|k| (*k).to_string()).collect();
    match role {
        "reviewer" => (Some(owned(&["review"])), Some(pane_id.to_string())),
        "owner" => (None, None),
        _ => (Some(owned(&["work", "verify", "report"])), None),
    }
}

impl Flock {
    /// The notifier `_next_job_wake` is: turn end and task completion call [`Flock::wake_push`].
    pub fn push_wake(&self) -> Arc<tokio::sync::Notify> {
        self.push_wake.clone()
    }

    /// `self._next_job_wake.set()`.
    pub fn wake_push(&self) {
        self.push_wake.notify_one();
    }

    /// `_push_targets`: pane id -> (scope, why): explicit followers plus registered lanes.
    pub fn push_target_map(
        &self,
        env: &dyn PushEnv,
    ) -> std::collections::BTreeMap<String, (String, Via)> {
        let followers = self
            .task_waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let lanes = env.registered_lanes();
        push_targets(&followers, lanes.as_ref(), |pane| env.caller_role(pane))
    }

    /// `_may_pull_everything`: true when this pane's role may pull any kind (the owner), or a
    /// reviewer whose provider may review.
    fn may_pull_everything(&self, env: &dyn PushEnv, pane_id: &str) -> bool {
        match env.caller_role(pane_id).as_str() {
            "owner" => true,
            "reviewer" => env.may_review(pane_id),
            _ => false,
        }
    }

    /// `_push_tasks`: feed idle lanes one task each, followers AND every registered lane.
    /// The gates are the ones the task names and nothing else: role, busy-ness, unread mail.
    pub async fn push_tasks(&self, env: &dyn PushEnv) -> Vec<PushDecision> {
        let mut decisions = Vec::new();
        let targets = self.push_target_map(env);
        if targets.is_empty() {
            return decisions;
        }
        let panes: std::collections::BTreeMap<String, Value> = match env.list_panes() {
            Ok(list) => list
                .into_iter()
                .filter_map(|p| {
                    p.get("pane_id")
                        .and_then(Value::as_str)
                        .map(|id| (id.to_string(), p.clone()))
                })
                .collect(),
            Err(_) => {
                eprintln!("push sweep skipped: pane list unavailable");
                return decisions;
            }
        };
        let Some(tasks) = self.stores().tasks else {
            eprintln!("push sweep skipped: no task store is attached");
            return decisions;
        };
        let mut note = |pane: &str, action: &'static str, detail: String| {
            eprintln!("push {action} pane={pane} {detail}");
            decisions.push(PushDecision {
                pane_id: pane.to_string(),
                action,
                detail,
            });
        };
        for (pane_id, (scope, via)) in &targets {
            let why = match via {
                Via::Follower => "follower",
                Via::Registered => "registered",
            };
            let Some(pane) = panes.get(pane_id) else {
                note(pane_id, "skip", "pane_gone".into());
                continue;
            };
            let status = pane
                .get("agent_status")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown");
            if BUSY_STATUSES.contains(&status) {
                note(pane_id, "skip", "busy_status".into());
                continue;
            }
            if tasks
                .leases_for_pane(pane_id, None)
                .await
                .is_ok_and(|l| !l.is_empty())
            {
                note(pane_id, "skip", "holding_work".into());
                continue;
            }
            if env.lane_agent_alive(pane_id) == Some(false) {
                // A registration outlives its agent: the lane wrapper ends in `exec sh`, which
                // keeps the pane pid. Feeding that shell starves a live lane (review t1010).
                note(pane_id, "skip", "no_live_agent".into());
                continue;
            }
            let role = env.caller_role(pane_id);
            if role == "reviewer" && !env.may_review(pane_id) {
                let provider = env.provider_of(pane_id).unwrap_or_else(|| "unknown".into());
                note(
                    pane_id,
                    "skip",
                    format!("provider_may_not_review provider={provider}"),
                );
                continue;
            }
            if env.unread_mail(pane_id) {
                note(pane_id, "skip", "unread_mail".into());
                continue;
            }
            // The role->kind filter belongs here as much as in task_next (t595): without it the
            // push path hands a worker a review it cannot complete.
            let (kinds, exclude_author) = claim_filters(&role, pane_id);
            if kinds.is_none() && !self.may_pull_everything(env, pane_id) {
                note(pane_id, "skip", format!("no_kind_for_role via={why}"));
                continue;
            }
            let options = ClaimOptions {
                kinds,
                exclude_author,
                ..ClaimOptions::default()
            };
            match tasks.claim_next(scope, pane_id, options).await {
                Err(error) => {
                    eprintln!("task claim failed for {pane_id}: {error}");
                    note(pane_id, "claim_failed", error.to_string());
                }
                Ok(ClaimOutcome::Refused { refusal, .. }) => {
                    note(
                        pane_id,
                        "none",
                        format!("scope={scope} reason={} via={why}", refusal.reason),
                    );
                }
                Ok(ClaimOutcome::Claimed {
                    task,
                    lease_seconds,
                    ..
                }) => {
                    note(pane_id, "claim", format!("{} via={why}", task.id));
                    env.announce_task(pane_id, &task, lease_seconds);
                }
            }
        }
        decisions
    }
}

/// The loop: recover once, then sweep on every wake or `interval`, until `stop` flips to true.
pub async fn run_push(
    flock: Arc<Flock>,
    env: Arc<dyn PushEnv>,
    interval: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    env.recover_next_jobs();
    let wake = flock.push_wake();
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
            _ = wake.notified() => {}
            _ = tokio::time::sleep(interval) => {}
        }
        if *stop.borrow() {
            return;
        }
        env.allocator_tick();
        env.next_jobs_tick();
        flock.push_tasks(env.as_ref()).await;
    }
}
