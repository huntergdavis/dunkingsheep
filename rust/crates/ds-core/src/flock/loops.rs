//! `flock/loops.rs` (T36, slice 3): the task reaper loop, from `dunk_core._task_reaper_loop`.
//!
//! One TICK is a plain function over a [`ReaperEnv`] (the backend and the maintenance steps the
//! Python reaches through `self`), so a test drives it without a daemon or a timer; the loop is
//! a tokio task that calls the tick every `DUNKINGSHEEP_REAP_S` seconds (default 15) and stops on
//! a signal (`self._task_stop`).
//!
//! The tick, in the Python's order: list panes -> snapshot launches -> clear historical errors ->
//! inbox maintenance (undeliverable sweep + 24 h auto-archive); then, ONLY for a LIVE backend,
//! `Tasks::reap` with the live pane set and the caller-liveness check; then memory self-repair,
//! then the follower cleanup (a waiter whose pane is gone is dropped).
//!
//! A FIXTURE BACKEND CANNOT REAP, and must not try (t247): `reap` refuses a pane list that is not
//! tagged live, correctly, so the tick skips the reap instead of logging a refusal as a fault.
//!
//! NOT PORTED here (named hooks on `ReaperEnv`, default no-ops, until their owners land): the
//! inbox `mark_undeliverable` / `auto_archive_undeliverable` / `_archive_retired_inbox` sweeps,
//! `_clear_historical_errors`, and `self.last_message`. `_task_push_loop` and
//! `_stuck_observer_loop` are the next slices (they need `next_jobs`, which has no Rust owner).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::Flock;
use crate::error::DunkError;
use crate::tasks::{CallerAlive, Requeued};

/// `TASK_REAP_S` (`DUNKINGSHEEP_REAP_S`, default 15 s), read per call here.
pub fn reap_interval() -> Duration {
    let secs = std::env::var("DUNKINGSHEEP_REAP_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(15.0);
    Duration::from_secs_f64(secs)
}

/// What one reaper tick reaches outside the task store. Every maintenance step is a hook that
/// must never break the tick (the Python wraps each in its own `except`).
pub trait ReaperEnv: Send + Sync {
    /// `self.herdr.list_panes()`: the backend's panes, or an error when it cannot answer.
    fn list_panes(&self) -> Result<Vec<Value>, DunkError>;
    /// `getattr(self.herdr, "live", False)`: only a live backend may move real work.
    fn is_live(&self) -> bool;
    /// `_clear_historical_errors(panes)`.
    fn clear_historical_errors(&self, _panes: &BTreeSet<String>) {}
    /// The inbox sweeps: `mark_undeliverable(panes)`, `auto_archive_undeliverable()`,
    /// `_archive_retired_inbox(pane_list)`.
    fn inbox_maintenance(&self, _panes: &BTreeSet<String>, _pane_list: &[Value]) {}
    /// `self.memory.recover_leases()` + `requeue_partial()`: deterministic self-repair.
    fn memory_maintenance(&self) {}
    /// `self.last_message = "Requeued N task(s) (...)"`.
    fn set_last_message(&self, _message: &str) {}
}

/// What a tick did, for the log and for tests.
#[derive(Debug, Default, PartialEq)]
pub struct ReaperTick {
    /// How many panes the backend listed; `None` when the listing failed.
    pub panes: Option<usize>,
    /// The backend is not live, so nothing was reaped.
    pub skipped_not_live: bool,
    pub requeued: Vec<Requeued>,
    /// A reap error (logged, never raised).
    pub reap_error: Option<String>,
}

impl Flock {
    /// `_task_reaper_loop`, one iteration.
    pub async fn reaper_tick(&self, env: &dyn ReaperEnv) -> ReaperTick {
        let mut tick = ReaperTick::default();
        let mut panes: Option<BTreeSet<String>> = None;
        match env.list_panes() {
            Ok(list) => {
                self.snapshot_launches(&list);
                let ids: BTreeSet<String> = list
                    .iter()
                    .filter_map(|p| p.get("pane_id").and_then(Value::as_str).map(String::from))
                    .collect();
                env.clear_historical_errors(&ids);
                env.inbox_maintenance(&ids, &list);
                tick.panes = Some(ids.len());
                panes = Some(ids);
            }
            Err(error) => eprintln!("task reaper: pane listing failed: {error}"),
        }
        if !env.is_live() {
            tick.skipped_not_live = true;
            return tick;
        }
        let Some(tasks) = self.stores().tasks else {
            tick.reap_error = Some("no task store is attached".into());
            return tick;
        };
        // A lease's recorded caller is checked against /proc (`process_alive`).
        let alive: CallerAlive = Box::new(crate::proc::process_alive);
        let live_panes = panes
            .as_ref()
            .map(|p| p.iter().cloned().collect::<Vec<_>>());
        match tasks
            .reap(live_panes, Some(alive), None, Some("live"))
            .await
        {
            Ok(requeued) => {
                if !requeued.is_empty() {
                    let ids: Vec<&str> = requeued
                        .iter()
                        .take(5)
                        .map(|r| r.task_id.as_str())
                        .collect();
                    env.set_last_message(&format!(
                        "Requeued {} task(s) ({})",
                        requeued.len(),
                        ids.join(", ")
                    ));
                    eprintln!("requeued task leases: {requeued:?}");
                }
                tick.requeued = requeued;
            }
            Err(error) => {
                eprintln!("task reaper failed: {error}");
                tick.reap_error = Some(error.to_string());
                return tick;
            }
        }
        env.memory_maintenance();
        if let Some(panes) = &panes {
            self.task_waiters
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|pane, _| panes.contains(pane));
        }
        tick
    }
}

/// The loop: tick every `interval` until `stop` flips to true (`self._task_stop.wait(...)`).
pub async fn run_reaper(
    flock: Arc<Flock>,
    env: Arc<dyn ReaperEnv>,
    interval: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(interval) => {
                flock.reaper_tick(env.as_ref()).await;
            }
        }
    }
}
