//! Runtime allocation pass; the task store remains the claim authority.
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::error::DunkError;
use crate::proc::{caller_pane_auth_failure, Caller};
use crate::server_runtime::RuntimeServices;
use crate::tasks::allocator::{AllocationCandidate, GuardedAllocation};

struct State {
    last_tick: Duration,
    last_result: Value,
    active: usize,
    stopped: bool,
}

impl State {
    /// Python initializes last_tick to zero on the system monotonic clock.
    fn begin_tick(&mut self, now: Duration) -> bool {
        if now.saturating_sub(self.last_tick) < Duration::from_secs(30) {
            return false;
        }
        self.last_tick = now;
        true
    }
}

fn monotonic_since_boot() -> Result<Duration, DunkError> {
    let time = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .map_err(|error| DunkError::Unavailable(format!("monotonic clock unavailable: {error}")))?;
    let seconds = u64::try_from(time.tv_sec())
        .map_err(|_| DunkError::Unavailable("negative monotonic clock".into()))?;
    let nanos = u32::try_from(time.tv_nsec())
        .map_err(|_| DunkError::Unavailable("invalid monotonic clock".into()))?;
    Ok(Duration::new(seconds, nanos))
}

/// One cache and lifecycle tracker for the process-owned allocator.
pub(crate) struct Allocator {
    state: Mutex<State>,
    idle: Condvar,
    #[cfg(test)]
    before_enqueue: Mutex<
        Option<(
            std::sync::mpsc::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        )>,
    >,
    #[cfg(test)]
    commit_barrier: Mutex<Option<Arc<crate::tasks::allocator::AllocationCommitBarrier>>>,
}

impl Allocator {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                last_tick: Duration::ZERO,
                last_result: json!({}),
                active: 0,
                stopped: false,
            }),
            idle: Condvar::new(),
            #[cfg(test)]
            before_enqueue: Mutex::new(None),
            #[cfg(test)]
            commit_barrier: Mutex::new(None),
        }
    }

    /// Stop scheduling and wait until every spawned pass has resolved its actor reply.
    pub(crate) fn stop_and_join(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stopped = true;
        while state.active != 0 {
            state = self.idle.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_cache(&self, result: Value) {
        let mut state = self.state.lock().unwrap();
        state.last_result = result;
        state.last_tick = Duration::ZERO;
    }

    #[cfg(test)]
    pub(crate) fn wait_for_active(&self, count: usize) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .idle
            .wait_timeout_while(state, Duration::from_secs(3), |state| state.active < count)
            .unwrap();
        state.active >= count
    }

    #[cfg(test)]
    pub(crate) fn set_before_enqueue(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        *self.before_enqueue.lock().unwrap() = Some((entered, release));
    }

    #[cfg(test)]
    pub(crate) fn set_commit_barrier(
        &self,
        barrier: Arc<crate::tasks::allocator::AllocationCommitBarrier>,
    ) {
        *self.commit_barrier.lock().unwrap() = Some(barrier);
    }

    fn stopped(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).stopped
    }

    /// Detached execution retains ownership through caller cancellation and shutdown.
    pub(crate) async fn tick(services: Arc<RuntimeServices>) -> Result<Value, DunkError> {
        {
            let mut state = services
                .allocator
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if state.stopped {
                return Err(DunkError::Unavailable("allocator stopped".into()));
            }
            let now = monotonic_since_boot()?;
            if !state.begin_tick(now) {
                return Ok(state.last_result.clone());
            }
            state.active += 1;
            #[cfg(test)]
            services.allocator.idle.notify_all();
        }
        let handle = services.runtime_handle().clone();
        handle
            .spawn(async move {
                struct Active(Arc<RuntimeServices>);
                impl Drop for Active {
                    fn drop(&mut self) {
                        let mut state = self
                            .0
                            .allocator
                            .state
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        state.active -= 1;
                        self.0.allocator.idle.notify_all();
                    }
                }
                let _active = Active(Arc::clone(&services));
                let result = run(&services).await?;
                if let Some(result) = result {
                    services
                        .allocator
                        .state
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .last_result = result.clone();
                    Ok(result)
                } else {
                    Ok(json!({}))
                }
            })
            .await
            .map_err(|error| DunkError::Unavailable(format!("allocator worker failed: {error}")))?
    }
}

/// Observe the two related tables without blocking an executor worker or holding
/// one guard while waiting for the other. Contention is retried outside both.
fn try_idle_snapshot(
    flock: &crate::flock::Flock,
    pane: &str,
) -> Result<Option<(Option<super::commands_turns::TurnState>, bool)>, DunkError> {
    let turns = match flock.turn_states.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Err(DunkError::Refused("turn state lock poisoned".into()))
        }
    };
    let outbox = match flock.outbox.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Err(DunkError::Refused("outbox lock poisoned".into()))
        }
    };
    let turn = turns.get(pane).cloned();
    let pending = outbox
        .get(pane)
        .is_some_and(|queue| queue.iter().any(|message| message.status == "queued"));
    Ok(Some((turn, pending)))
}

async fn run(services: &Arc<RuntimeServices>) -> Result<Option<Value>, DunkError> {
    let flock = services.flock();
    let tasks = flock
        .stores()
        .tasks
        .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
    let status = tasks.allocation_status().await?;
    let workers = status
        .get("workers")
        .and_then(Value::as_array)
        .ok_or_else(|| DunkError::Refused("allocation workers unavailable".into()))?;
    if workers.is_empty() {
        return Ok(None);
    }
    let lanes = services.allocator_lanes().await?;
    let panes: BTreeMap<_, _> = lanes
        .iter()
        .filter_map(|lane| {
            lane.get("pane_id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), lane.clone()))
        })
        .collect();
    let mut candidates = BTreeMap::new();
    let mut reasons = Map::new();
    for worker in workers {
        let pane = worker
            .get("pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| DunkError::Refused("allocation worker missing pane".into()))?;
        let enabled = worker
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                worker
                    .get("enabled")
                    .and_then(Value::as_i64)
                    .is_some_and(|v| v != 0)
            });
        let mut reason = if enabled {
            services.allocator_gate(pane, lanes.clone()).await
        } else {
            Some("disabled".into())
        };
        if reason.is_none() {
            if let Some(info) = panes.get(pane) {
                let (turn, pending) = loop {
                    if let Some(snapshot) = try_idle_snapshot(flock, pane)? {
                        break snapshot;
                    }
                    if services.allocator.stopped() {
                        return Err(DunkError::Unavailable("allocator stopped".into()));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                if let Some(turn) = turn {
                    let caller = turn.caller.clone().unwrap_or_else(|| json!({}));
                    let identity = Caller::new(
                        caller.get("pid").and_then(Value::as_i64),
                        caller
                            .get("start")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        caller
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        Some(pane.to_owned()),
                        "agent",
                    );
                    reason = if turn.ended_at.is_none_or(|ended_at| ended_at == 0.0) {
                        Some("not_idle".into())
                    } else if pending {
                        Some("pending_delivery".into())
                    } else {
                        let present = caller.as_object().is_some_and(|map| !map.is_empty());
                        caller_pane_auth_failure(
                            present.then_some(&identity),
                            info.get("pane_pid")
                                .and_then(Value::as_i64)
                                .filter(|pid| *pid != 0),
                        )
                        .map(str::to_owned)
                    };
                    if reason.is_none() {
                        match services
                            .allocator_probe(pane.to_owned(), info.clone())
                            .await
                        {
                            Ok((0, delivery))
                                if delivery.get("state").and_then(Value::as_str)
                                    == Some("ready") =>
                            {
                                candidates.insert(
                                    pane.to_owned(),
                                    AllocationCandidate {
                                        turn_id: turn.turn_id,
                                        ended_at: turn.ended_at,
                                        caller,
                                    },
                                );
                            }
                            Ok((0, _)) => reason = Some("delivery_not_ready".into()),
                            _ => reason = Some("unread_or_unknown".into()),
                        }
                    }
                } else {
                    reason = Some("not_idle".into());
                }
            } else {
                reason = Some("missing".into());
            }
        }
        reasons.insert(
            pane.to_owned(),
            reason.map(Value::String).unwrap_or(Value::Null),
        );
    }
    #[cfg(test)]
    {
        let barrier = services.allocator.before_enqueue.lock().unwrap().take();
        if let Some((entered, release)) = barrier {
            entered.send(()).map_err(|_| {
                DunkError::Unavailable("allocator pre-enqueue barrier closed".into())
            })?;
            release.await.map_err(|_| {
                DunkError::Unavailable("allocator pre-enqueue barrier closed".into())
            })?;
        }
    }
    let mut busy_waited = Duration::ZERO;
    let handoffs = loop {
        if services.allocator.stopped() {
            return Err(DunkError::Unavailable("allocator stopped".into()));
        }
        #[cfg(test)]
        let barrier = services.allocator.commit_barrier.lock().unwrap().take();
        #[cfg(test)]
        let attempt = if let Some(barrier) = barrier {
            tasks
                .allocation_try_guarded_with_barrier(Arc::clone(flock), candidates.clone(), barrier)
                .await
        } else {
            tasks
                .allocation_try_guarded(Arc::clone(flock), candidates.clone())
                .await
        };
        #[cfg(not(test))]
        let attempt = tasks
            .allocation_try_guarded(Arc::clone(flock), candidates.clone())
            .await;
        match attempt? {
            GuardedAllocation::Committed(handoffs) => break handoffs,
            GuardedAllocation::RetryLock => tokio::time::sleep(Duration::from_millis(10)).await,
            GuardedAllocation::RetryBusy => {
                let budget = Duration::from_millis(crate::db::BUSY_TIMEOUT_MS);
                if busy_waited >= budget {
                    return Err(DunkError::Busy);
                }
                let wait = Duration::from_millis(10).min(budget - busy_waited);
                let started = Instant::now();
                tokio::time::sleep(wait).await;
                busy_waited += started.elapsed();
            }
        }
    };
    for handoff in &handoffs {
        if let Some(object) = handoff.as_object() {
            services.allocator_materialise(object).await;
        }
    }
    Ok(Some(
        json!({"allocated": handoffs, "workers": reasons, "at": crate::clock::now_s()}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::{panes::Stores, Flock};
    use crate::tasks::Tasks;

    #[test]
    fn initial_cache_uses_system_monotonic_origin() {
        let mut state = State {
            last_tick: Duration::ZERO,
            last_result: json!({}),
            active: 0,
            stopped: false,
        };
        assert!(!state.begin_tick(Duration::from_millis(29_999)));
        assert_eq!(state.last_tick, Duration::ZERO);
        assert!(state.begin_tick(Duration::from_secs(30)));
        assert!(!state.begin_tick(Duration::from_millis(59_999)));
        assert!(state.begin_tick(Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn preflight_outbox_contention_releases_turn_lock_and_actor_progresses() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
        tasks.allocation_init().await.unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let other = Arc::clone(&flock);
        let holder = std::thread::spawn(move || {
            let _outbox = other.outbox.lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(try_idle_snapshot(&flock, "%1").unwrap().is_none());
        assert!(
            flock.turn_states.try_lock().is_ok(),
            "failed snapshot must release turn guard"
        );
        let status = tokio::time::timeout(Duration::from_secs(2), tasks.allocation_status())
            .await
            .unwrap()
            .unwrap();
        assert!(status["workers"].is_array());
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(try_idle_snapshot(&flock, "%1").unwrap().is_some());
    }
}
