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
    last_tick: Option<Instant>,
    last_result: Value,
    active: usize,
    stopped: bool,
}

/// One cache and lifecycle tracker for the process-owned allocator.
pub(crate) struct Allocator {
    state: Mutex<State>,
    idle: Condvar,
}

impl Allocator {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                last_tick: None,
                last_result: json!({}),
                active: 0,
                stopped: false,
            }),
            idle: Condvar::new(),
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
            let now = Instant::now();
            if state
                .last_tick
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(30))
            {
                return Ok(state.last_result.clone());
            }
            state.last_tick = Some(now);
            state.active += 1;
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
                let turn = flock
                    .turn_states
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(pane)
                    .cloned();
                let pending = flock
                    .outbox
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(pane)
                    .is_some_and(|q| q.iter().any(|m| m.status == "queued"));
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
                    reason = if turn.ended_at.is_none() {
                        Some("not_idle".into())
                    } else if pending {
                        Some("pending_delivery".into())
                    } else {
                        caller_pane_auth_failure(
                            Some(&identity),
                            info.get("pane_pid").and_then(Value::as_i64),
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
    let mut busy_waited = Duration::ZERO;
    let handoffs = loop {
        if services.allocator.stopped() {
            return Err(DunkError::Unavailable("allocator stopped".into()));
        }
        match tasks
            .allocation_try_guarded(Arc::clone(flock), candidates.clone())
            .await?
        {
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
