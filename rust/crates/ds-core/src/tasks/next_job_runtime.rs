//! Runtime state and per-pane serialization for next-job operations.
//!
//! The lock map mirrors `dunk_next_job.NextJobs._pane_lock`: a pane always maps to
//! the same async mutex, while different panes can progress independently. The
//! standard mutex protects only the map and is never held across an await.

use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tokio::sync::OwnedMutexGuard;

use super::advance::{Advance, AdvanceOptions, OperatorProof, PlanInput, Planner};
use super::next_job::Handoff;
use crate::config::Env;
use crate::dispatch::NextJob;
use crate::error::DunkError;
use crate::flock::commands_turns::TurnState;
use crate::flock::loop_env::{configured_loop_terminal_factory, LoopTerminalFactory};
use crate::flock::sends::{self, Message, SendGuardOptions};
use crate::flock::Flock;
use crate::metrics::Event;
use crate::proc::Caller;
use crate::server_runtime::LoopRuntimeAdapter;

/// A paused dispatch must remain distinguishable from a materialisation failure.
#[allow(dead_code)] // The next-job worker consumes this typed result from delivery.
#[derive(Debug)]
pub(crate) enum MaterialiseError {
    DispatchPaused(String),
    Other(DunkError),
}

impl From<DunkError> for MaterialiseError {
    fn from(error: DunkError) -> Self {
        Self::Other(error)
    }
}

pub struct NextJobs {
    // Later runtime leaves use the shared Flock for dispatch, tasks, and inbox work.
    flock: Arc<Flock>,
    env_provider: GateEnvProvider,
    terminal_factory: Option<LoopTerminalFactory>,
    #[allow(dead_code)] // The next-job worker leaves call delivery and turn binding.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct AdvanceRequest<'a> {
    pane: &'a str,
    trigger: &'a str,
    from_task: Option<&'a str>,
    caller: Option<Caller>,
    operator_proof: Option<OperatorProof>,
    dispatch_reason: Option<String>,
    unread: Option<i64>,
    pending_delivery: bool,
}

#[allow(dead_code)] // The next-job worker consumes these outcomes after this leaf.
enum TransportResult {
    Held(String),
    Sent {
        transport_id: String,
        delivered_at: f64,
    },
    Undeliverable {
        transport_id: String,
        reason: String,
    },
}

type GateEnvProvider = Arc<dyn Fn() -> Result<Env, ()> + Send + Sync + 'static>;

fn caller_map(caller: Option<Caller>) -> Map<String, Value> {
    let mut values = Map::new();
    if let Some(caller) = caller {
        values.insert("pane_id".into(), json!(caller.pane_id));
        values.insert("pid".into(), json!(caller.pid));
        values.insert("start".into(), json!(caller.start));
        values.insert("name".into(), json!(caller.name));
        values.insert("source".into(), json!(caller.source));
    }
    values
}

fn caller_from_map(value: Option<&Value>) -> Option<Caller> {
    let caller = value?.as_object()?;
    if caller.is_empty() {
        return None;
    }
    let source = match caller.get("source").and_then(Value::as_str) {
        Some("agent") => "agent",
        Some("session") => "session",
        Some("peer") => "peer",
        _ => "none",
    };
    Some(Caller::new(
        caller.get("pid").and_then(Value::as_i64),
        caller
            .get("start")
            .and_then(Value::as_str)
            .map(str::to_owned),
        caller
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        caller
            .get("pane_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        source,
    ))
}

#[allow(dead_code)] // Delivery and turn binding use the blocking receipt bridge.
async fn record_next_job_notify(
    inbox: Arc<crate::inbox_store::Store>,
    message_id: String,
    state: &'static str,
    reason: Option<String>,
    transport_id: Option<String>,
    attempt: Option<i64>,
) -> Result<(), DunkError> {
    tokio::task::spawn_blocking(move || {
        inbox.record_notify(
            &message_id,
            state,
            reason.as_deref(),
            transport_id.as_deref(),
            attempt,
        )
    })
    .await
    .map_err(|error| DunkError::Refused(format!("inbox worker failed: {error}")))?
}

impl NextJobs {
    pub(crate) fn owns_flock(&self, flock: &Flock) -> bool {
        std::ptr::eq(self.flock.as_ref(), flock)
    }

    /// Reconcile each durable handoff in journal order. A bad handoff is isolated so it
    /// cannot keep later work from reaching the existing delivery state machine.
    #[allow(dead_code)] // Wired by the process-owned next-job recovery worker.
    pub(crate) async fn tick(&self, restart: bool) -> Result<(), DunkError> {
        let tasks = self
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        for handoff in tasks.next_job_handoffs(None).await? {
            let state = handoff.get("state").and_then(Value::as_str).unwrap_or("");
            if matches!(state, "started" | "undeliverable" | "cancelled") {
                continue;
            }
            let pane = handoff
                .get("pane_id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            let message_id = handoff
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or("<missing>")
                .to_owned();
            if let Err(error) = self.deliver(handoff, restart).await {
                eprintln!(
                    "next-job tick delivery failed pane={pane} message={message_id}: {error}"
                );
            }
        }
        Ok(())
    }

    /// Replay only temporary/refusal decisions after process restart, then reconcile
    /// durable handoffs. Explicit no-work decisions stay terminal so startup never
    /// polls the unassigned fleet backlog.
    #[allow(dead_code)] // Wired by the process-owned next-job recovery worker.
    pub(crate) async fn recover(&self) -> Result<(), DunkError> {
        let tasks = self
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        for (pane, plan) in tasks.next_job_decisions().await? {
            let retry_reason = matches!(
                plan.get("reason").and_then(Value::as_str),
                Some(
                    "unread_unknown"
                        | "unread_mail"
                        | "pending_delivery"
                        | "disabled"
                        | "not_allowed"
                        | "unavailable_lane"
                        | "readiness_unknown"
                )
            );
            if retry_reason {
                let from_task = plan.get("from_task").and_then(Value::as_str);
                let caller = caller_from_map(plan.get("caller"));
                self.advance(&pane, "restart", from_task, caller, None)
                    .await?;
            }
        }
        self.tick(true).await
    }

    pub fn new(flock: Arc<Flock>) -> Self {
        Self::with_providers(flock, Arc::new(process_gate_env), None)
    }

    /// Construct one shared manager with its gate environment and terminal providers.
    pub(crate) fn with_providers(
        flock: Arc<Flock>,
        env_provider: GateEnvProvider,
        terminal_factory: Option<LoopTerminalFactory>,
    ) -> Self {
        Self {
            flock,
            env_provider,
            terminal_factory,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Advance one lane through the existing atomic task decision, then materialise its
    /// committed handoff. The caller and operator proof are supplied by the request/loop
    /// boundary; this async manager never reads request-thread identity.
    #[allow(dead_code)] // The production turn and completion callers are assembled in t1605.
    pub(crate) async fn advance(
        &self,
        pane: &str,
        trigger: &str,
        from_task: Option<&str>,
        caller: Option<Caller>,
        operator_proof: Option<OperatorProof>,
    ) -> Result<Advance, DunkError> {
        let _pane_guard = self.lock_pane(pane).await;
        let dispatch_reason = self.gate(pane, None).await;
        let unread = if dispatch_reason.is_some() {
            None
        } else {
            self.unread_count(pane).await
        };
        let pending_delivery = self
            .flock
            .outbox_snapshot()
            .get(pane)
            .is_some_and(|messages| messages.iter().any(|message| message.status == "queued"));

        self.advance_with_materialiser(
            AdvanceRequest {
                pane,
                trigger,
                from_task,
                caller,
                operator_proof,
                dispatch_reason,
                unread,
                pending_delivery,
            },
            |handoff| async move { self.materialise(&handoff).await },
        )
        .await
    }

    async fn advance_with_materialiser<M, Fut>(
        &self,
        request: AdvanceRequest<'_>,
        materialise: M,
    ) -> Result<Advance, DunkError>
    where
        M: FnOnce(Handoff) -> Fut,
        Fut: Future<Output = Result<crate::inbox::Status, MaterialiseError>>,
    {
        let AdvanceRequest {
            pane,
            trigger,
            from_task,
            caller,
            operator_proof,
            dispatch_reason,
            unread,
            pending_delivery,
        } = request;
        let tasks = self
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let caller = caller_map(caller);
        let planner: Arc<Planner> = Arc::new(|input: &PlanInput<'_>| {
            let tasks: Vec<Value> = input
                .tasks
                .iter()
                .map(|task| {
                    json!({
                        "id": task.task.id,
                        "status": task.task.status,
                        "priority": task.task.priority,
                        "assignees": task.assignees,
                        "created_at": task.task.created_at,
                        "blocked_by": task.deps.blocked_by,
                        "block_reason": task.task.block_reason,
                        "allocation_waiting": task.allocation_waiting,
                    })
                })
                .collect();
            // AdvanceOptions currently carries an integer. Keep -1 as the private
            // unknown sentinel so dispatch::next_job_plan still receives None.
            let unread = (input.unread >= 0).then_some(input.unread);
            let next_job = NextJob {
                trigger: input.trigger,
                pane_id: input.pane_id,
                tasks: &tasks,
                unread,
                pending_delivery: input.pending_delivery,
                dispatch_reason: input.dispatch_reason,
                now: Some(input.now),
                ..NextJob::default()
            };
            crate::dispatch::next_job_plan(&next_job)
                .as_object()
                .cloned()
                .unwrap_or_default()
        });
        let options = AdvanceOptions {
            from_task: from_task.map(str::to_owned),
            caller,
            unread: unread.unwrap_or(-1),
            pending_delivery,
            dispatch_reason,
            operator_proof,
        };
        let advance = tasks
            .advance_next_job(pane, trigger, options, planner)
            .await?;
        self.record_advance_decision(pane, trigger, &advance.plan)
            .await;

        let Some(handoff) = advance
            .plan
            .get("handoff")
            .and_then(Value::as_object)
            .cloned()
        else {
            return Ok(advance);
        };
        match materialise(handoff.clone()).await {
            Ok(_) => {}
            Err(MaterialiseError::DispatchPaused(reason)) => {
                let changes = Map::from_iter([("reason".into(), Value::String(reason))]);
                if tasks
                    .update_next_job(&handoff, None, changes)
                    .await?
                    .is_none()
                {
                    return Err(DunkError::Refused(
                        "committed next-job handoff disappeared while recording dispatch pause"
                            .into(),
                    ));
                }
            }
            Err(MaterialiseError::Other(error)) => {
                eprintln!(
                    "next-job inbox materialisation deferred pane={pane} message={:?}: {error}",
                    handoff.get("message_id").and_then(Value::as_str)
                );
            }
        }
        // The claim and handoff are already committed. Return that original plan even if
        // the best-effort post-commit inbox materialisation recorded a pause or failed.
        Ok(advance)
    }

    async fn unread_count(&self, pane: &str) -> Option<i64> {
        let factory = match self
            .terminal_factory
            .clone()
            .map(Ok)
            .unwrap_or_else(configured_loop_terminal_factory)
        {
            Ok(factory) => factory,
            Err(error) => {
                eprintln!("next-job unread count unavailable pane={pane}: {error}");
                return None;
            }
        };
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), factory);
        let pane = pane.to_owned();
        let worker_pane = pane.clone();
        match adapter
            .with_loop_terminal(move |flock, terminal| {
                let store = flock.stores().inbox?;
                let status = flock.status_state().ok()?;
                let env = &status.env;
                let mailbox = flock
                    .own_mailbox_from_sources(
                        terminal,
                        || store.read_pool(),
                        &worker_pane,
                        || {
                            env.get("DUNKINGSHEEP_MAILBOX")
                                .map(|value| {
                                    value.trim_matches(|c: char| {
                                        c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
                                    })
                                })
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned)
                        },
                        || {
                            let backend = env
                                .get("DUNKINGSHEEP_TERMINAL_BACKEND")
                                .unwrap_or("tmux")
                                .trim()
                                .to_lowercase();
                            Ok(if backend.is_empty() {
                                "tmux".to_owned()
                            } else {
                                backend
                            })
                        },
                    )
                    .ok()?;
                let mut lane_panes: Vec<String> = crate::pm::load_routing(None, None, env)
                    .unwrap_or_default()
                    .into_iter()
                    .flat_map(|(_, route)| route.members)
                    .collect();
                lane_panes.sort();
                lane_panes.dedup();
                crate::inbox::count(
                    store.read_pool(),
                    &mailbox,
                    &lane_panes,
                    crate::clock::now_s(),
                )
                .ok()
                .map(|counts| counts.unread)
            })
            .await
        {
            Ok(count) => count,
            Err(error) => {
                eprintln!("next-job unread count worker failed pane={pane}: {error}");
                None
            }
        }
    }

    async fn record_advance_decision(&self, pane: &str, trigger: &str, plan: &Map<String, Value>) {
        let outcome = plan
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let reason = plan
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        eprintln!(
            "next-job {pane} trigger={trigger} action={outcome} task={:?} reason={reason:?}",
            plan.get("task_id").and_then(Value::as_str)
        );
        let Some(metrics) = self.flock.stores().metrics else {
            return;
        };
        let event = Event {
            kind: "next_job.decision".into(),
            pane_id: Some(pane.to_owned()),
            source: Some(trigger.to_owned()),
            outcome: Some(outcome),
            reason,
            ..Event::default()
        };
        if let Err(error) = tokio::task::spawn_blocking(move || metrics.record(&event)).await {
            eprintln!("next-job decision metric worker failed pane={pane}: {error}");
        }
    }

    #[allow(dead_code)] // Used by the dependent next-job transition leaves.
    pub(crate) async fn materialise(
        &self,
        handoff: &Handoff,
    ) -> Result<crate::inbox::Status, MaterialiseError> {
        let env = (self.env_provider)()
            .map_err(|_| MaterialiseError::DispatchPaused("readiness_unknown".into()))?;
        self.materialise_with(handoff, env, self.terminal_factory.clone())
            .await
    }

    async fn materialise_with(
        &self,
        handoff: &Handoff,
        env: Env,
        factory: Option<LoopTerminalFactory>,
    ) -> Result<crate::inbox::Status, MaterialiseError> {
        let field = |name: &str| {
            handoff
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| DunkError::Refused(format!("missing next-job {name}")))
        };
        let pane = field("pane_id")?;
        if let Some(reason) = self.gate_with(&pane, None, env, factory.clone()).await {
            return Err(MaterialiseError::DispatchPaused(reason));
        }
        let factory = match factory {
            Some(factory) => factory,
            None => configured_loop_terminal_factory()?,
        };
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), factory);
        let requested = pane.clone();
        let (mailbox, _, actual_pane) = adapter
            .with_loop_terminal(move |flock, terminal| {
                flock.resolve_mailbox(terminal, &requested, None, true, "")
            })
            .await
            .map_err(|error| DunkError::Refused(format!("terminal worker failed: {error}")))??;
        if actual_pane.as_deref() != Some(pane.as_str()) {
            return Err(DunkError::Refused("next-job pane changed".into()).into());
        }
        let task_id = field("task_id")?;
        let message_id = field("message_id")?;
        let stores = self.flock.stores();
        let tasks = stores
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let task = tasks.get(&task_id).await?;
        let source = handoff
            .get("from_task")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| handoff.get("trigger").and_then(Value::as_str))
            .unwrap_or("None");
        let body = format!("TASK {} [{}] {}\n\n{}\n\nAlready atomically claimed for {}; start this job now.\nheartbeat: dunkingsheep task heartbeat {}\ncomplete: dunkingsheep task complete {}\nhandoff: {} from {}", task.id, task.priority, task.title, task.body, pane, task.id, task.id, message_id, source);
        let subject = format!("TASK {}: {}", task.id, task.title);
        let inbox = stores
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store unavailable".into()))?;
        tokio::task::spawn_blocking(move || {
            inbox.ensure_next_job(&message_id, &mailbox, &pane, &body, &subject)
        })
        .await
        .map_err(|error| DunkError::Refused(format!("inbox worker failed: {error}")))?
        .map_err(Into::into)
    }

    /// Advance one durable handoff through its bounded send/recovery state machine.
    #[allow(dead_code)] // Wired by the following next-job tick/recovery leaf.
    pub(crate) async fn deliver(
        &self,
        handoff: Handoff,
        restart: bool,
    ) -> Result<Option<Handoff>, DunkError> {
        self.deliver_with_provider(
            handoff,
            restart,
            Arc::clone(&self.env_provider),
            self.terminal_factory.clone(),
        )
        .await
    }

    #[allow(dead_code)] // Wired by the following next-job tick/recovery leaf.
    async fn deliver_with(
        &self,
        handoff: Handoff,
        restart: bool,
        env: Result<Env, ()>,
        factory: Option<LoopTerminalFactory>,
    ) -> Result<Option<Handoff>, DunkError> {
        self.deliver_with_provider(handoff, restart, Arc::new(move || env.clone()), factory)
            .await
    }

    async fn deliver_with_provider(
        &self,
        handoff: Handoff,
        restart: bool,
        env_provider: GateEnvProvider,
        factory: Option<LoopTerminalFactory>,
    ) -> Result<Option<Handoff>, DunkError> {
        let field = |row: &Handoff, name: &str| {
            row.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| DunkError::Refused(format!("missing next-job {name}")))
        };
        let pane = field(&handoff, "pane_id")?;
        let message_id = field(&handoff, "message_id")?;
        let _pane_guard = self.lock_pane(&pane).await;
        let stores = self.flock.stores();
        let tasks = stores
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let inbox = stores
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store unavailable".into()))?;
        let Some(mut handoff) = tasks
            .next_job_handoffs(Some(&pane))
            .await?
            .into_iter()
            .find(|row| row.get("message_id").and_then(Value::as_str) == Some(&message_id))
        else {
            return Ok(None);
        };
        let state = handoff.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "started" | "undeliverable" | "cancelled") {
            return Ok(Some(handoff));
        }
        let task_id = field(&handoff, "task_id")?;
        let task = tasks.decorate(&task_id, Some(&pane)).await?;
        if matches!(task.task.status.as_str(), "done" | "cancelled") {
            let changes = serde_json::Map::from_iter([
                ("state".into(), Value::String("cancelled".into())),
                ("reason".into(), Value::String("task_finished".into())),
            ]);
            return tasks.update_next_job(&handoff, None, changes).await;
        }

        if state == "sending" {
            if !restart {
                return Ok(Some(handoff));
            }
            let owner = handoff.get("transport_owner");
            let live = owner
                .and_then(|owner| owner.get("pid"))
                .and_then(Value::as_i64)
                .is_some_and(|pid| {
                    crate::proc::process_alive(
                        pid,
                        owner
                            .and_then(|owner| owner.get("start"))
                            .and_then(Value::as_str),
                    )
                });
            if live {
                return Ok(Some(handoff));
            }
            let receipt =
                crate::inbox::status(inbox.read_pool(), &message_id, crate::clock::now_s());
            let receipt = match receipt {
                Ok(receipt) => receipt,
                Err(_) => {
                    let changes = serde_json::Map::from_iter([
                        ("state".into(), Value::String("pending".into())),
                        (
                            "reason".into(),
                            Value::String("interrupted_before_inbox".into()),
                        ),
                    ]);
                    return tasks.update_next_job(&handoff, None, changes).await;
                }
            };
            let attempt = handoff.get("attempt").and_then(Value::as_i64).unwrap_or(0);
            if receipt.notify_attempt != Some(attempt) {
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("pending".into())),
                    (
                        "reason".into(),
                        Value::String("interrupted_before_transport".into()),
                    ),
                ]);
                return tasks.update_next_job(&handoff, None, changes).await;
            }
            if receipt.notify_state.as_deref() == Some("sent") {
                let mut changes = serde_json::Map::new();
                changes.insert("state".into(), Value::String("sent".into()));
                changes.insert("reason".into(), Value::Null);
                changes.insert(
                    "transport_id".into(),
                    receipt
                        .notify_transport_id
                        .clone()
                        .map_or(Value::Null, Value::String),
                );
                changes.insert(
                    "delivered_at".into(),
                    receipt.notify_at.map_or(Value::Null, Value::from),
                );
                handoff = match tasks.update_next_job(&handoff, None, changes).await? {
                    Some(handoff) => handoff,
                    None => return Ok(None),
                };
                self.bind_turn_locked(&pane).await?;
                return Ok(Some(handoff));
            }
            record_next_job_notify(
                Arc::clone(&inbox),
                message_id.clone(),
                "undeliverable",
                Some("delivery_interrupted".into()),
                None,
                Some(attempt),
            )
            .await?;
            let changes = serde_json::Map::from_iter([
                ("state".into(), Value::String("undeliverable".into())),
                (
                    "reason".into(),
                    Value::String("delivery_interrupted".into()),
                ),
            ]);
            return tasks.update_next_job(&handoff, None, changes).await;
        }
        if state == "sent" {
            self.bind_turn_locked(&pane).await?;
            return Ok(Some(handoff));
        }

        let dispatch_reason = match env_provider() {
            Ok(env) => self.gate_with(&pane, None, env, factory.clone()).await,
            Err(()) => Some("readiness_unknown".into()),
        };
        if let Some(reason) = dispatch_reason {
            if handoff.get("reason").and_then(Value::as_str) == Some(reason.as_str()) {
                return Ok(Some(handoff));
            }
            record_next_job_notify(
                Arc::clone(&inbox),
                message_id,
                "queued",
                Some(reason.clone()),
                None,
                handoff.get("attempt").and_then(Value::as_i64),
            )
            .await?;
            let changes = serde_json::Map::from_iter([("reason".into(), Value::String(reason))]);
            return tasks.update_next_job(&handoff, None, changes).await;
        }

        if self.flock.turn_active(&pane) {
            return Ok(Some(handoff));
        }
        let attempt = handoff
            .get("attempt")
            .and_then(Value::as_i64)
            .ok_or_else(|| DunkError::Refused("missing next-job attempt".into()))?;
        let attempt_at = handoff.get("attempt_at").and_then(Value::as_f64);
        let retry = crate::dispatch::delivery_retry_plan(
            attempt,
            attempt_at,
            crate::clock::now_s(),
            sends::SUBMIT_ATTEMPTS as i64,
            None,
        );
        match retry.get("action").and_then(Value::as_str) {
            Some("wait") => return Ok(Some(handoff)),
            Some("stop") => {
                let reason = retry
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("retry_exhausted")
                    .to_owned();
                record_next_job_notify(
                    Arc::clone(&inbox),
                    message_id,
                    "undeliverable",
                    Some(reason.clone()),
                    None,
                    Some(attempt),
                )
                .await?;
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("undeliverable".into())),
                    ("reason".into(), Value::String(reason)),
                ]);
                return tasks.update_next_job(&handoff, None, changes).await;
            }
            _ => {}
        }
        if !task.assignees.contains(&pane)
            || task.task.block_reason.is_some()
            || !task.deps.blocked_by.is_empty()
            || !task.claimed_by.contains(&pane)
        {
            let reason = "claim_no_longer_valid".to_owned();
            record_next_job_notify(
                Arc::clone(&inbox),
                message_id,
                "undeliverable",
                Some(reason.clone()),
                None,
                Some(attempt),
            )
            .await?;
            let changes = serde_json::Map::from_iter([
                ("state".into(), Value::String("undeliverable".into())),
                ("reason".into(), Value::String(reason)),
            ]);
            return tasks.update_next_job(&handoff, None, changes).await;
        }

        let now = crate::clock::now_s();
        let pid = i64::from(std::process::id());
        let mut reservation = serde_json::Map::new();
        reservation.insert("state".into(), Value::String("sending".into()));
        reservation.insert("attempt".into(), Value::from(attempt.saturating_add(1)));
        reservation.insert("attempt_at".into(), Value::from(now));
        reservation.insert(
            "transport_owner".into(),
            serde_json::json!({"pid":pid,"start":crate::proc::process_start(pid)}),
        );
        let expected = serde_json::Map::from_iter([
            (
                "state".into(),
                handoff.get("state").cloned().unwrap_or(Value::Null),
            ),
            ("attempt".into(), Value::from(attempt)),
        ]);
        let Some(reserved) = tasks
            .update_next_job(&handoff, Some(expected), reservation)
            .await?
        else {
            return Ok(None);
        };
        let materialised = match env_provider() {
            Ok(env) => self.materialise_with(&reserved, env, factory.clone()).await,
            Err(()) => Err(MaterialiseError::DispatchPaused("readiness_unknown".into())),
        };
        let mail = match materialised {
            Ok(mail) => mail,
            Err(MaterialiseError::DispatchPaused(reason)) => {
                record_next_job_notify(
                    Arc::clone(&inbox),
                    message_id,
                    "queued",
                    Some(reason.clone()),
                    None,
                    Some(attempt.saturating_add(1)),
                )
                .await?;
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("pending".into())),
                    ("reason".into(), Value::String(reason)),
                ]);
                return tasks.update_next_job(&reserved, None, changes).await;
            }
            Err(MaterialiseError::Other(error)) => {
                let reason = format!("delivery_error: {error}");
                record_next_job_notify(
                    Arc::clone(&inbox),
                    message_id,
                    "undeliverable",
                    Some(reason.clone()),
                    None,
                    Some(attempt.saturating_add(1)),
                )
                .await?;
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("undeliverable".into())),
                    ("reason".into(), Value::String(reason)),
                ]);
                return tasks.update_next_job(&reserved, None, changes).await;
            }
        };

        let final_reason = match env_provider() {
            Ok(env) => self.gate_with(&pane, None, env, factory.clone()).await,
            Err(()) => Some("readiness_unknown".into()),
        };
        if let Some(reason) = final_reason {
            record_next_job_notify(
                Arc::clone(&inbox),
                message_id,
                "queued",
                Some(reason.clone()),
                None,
                Some(attempt.saturating_add(1)),
            )
            .await?;
            let changes = serde_json::Map::from_iter([
                ("state".into(), Value::String("pending".into())),
                ("reason".into(), Value::String(reason)),
            ]);
            return tasks.update_next_job(&reserved, None, changes).await;
        }

        let attempt = attempt.saturating_add(1);
        let transport_id = format!("{message_id}:{attempt}");
        let text = format!(
            "[inbox] TASK {task_id} is already claimed for you. Read dunkingsheep inbox read and execute it now. Handoff {message_id}."
        );
        let mailbox = mail.mailbox;
        let pane_for_send = pane.clone();
        let inbox_for_send = Arc::clone(&inbox);
        let receipt_id = message_id.clone();
        let notify_attempt = attempt;
        let transport_id_for_send = transport_id.clone();
        record_next_job_notify(
            Arc::clone(&inbox),
            message_id.clone(),
            "queued",
            None,
            Some(transport_id.clone()),
            Some(attempt),
        )
        .await?;
        let terminal_factory = match factory {
            Some(factory) => factory,
            None => configured_loop_terminal_factory()?,
        };
        let permission_factory = Arc::clone(&terminal_factory);
        let permission_jobs = NextJobs::new(Arc::clone(&self.flock));
        let permission_env_provider = Arc::clone(&env_provider);
        let permission_runtime = tokio::runtime::Handle::current();
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), terminal_factory);
        let sent = adapter
            .with_loop_terminal(
                move |flock, terminal| -> Result<TransportResult, DunkError> {
                    let mut message = Message::new(
                        &transport_id_for_send,
                        &pane_for_send,
                        &mailbox,
                        &text,
                        "next_job",
                    );
                    message.notify_for.push(receipt_id.clone());
                    let options = SendGuardOptions {
                        hold_while_typing: true,
                        allow_modal: false,
                        preserve_draft: true,
                        inconclusive_fallback: false,
                    };
                    let pane_for_permission = pane_for_send.clone();
                    let permission = move || {
                        let env = match permission_env_provider() {
                            Ok(env) => env,
                            Err(()) => return Some("readiness_unknown".into()),
                        };
                        permission_runtime.block_on(permission_jobs.gate_with(
                            &pane_for_permission,
                            None,
                            env,
                            Some(Arc::clone(&permission_factory)),
                        ))
                    };
                    let (delivered, ok, detail) = sends::send_guarded(
                        flock,
                        terminal,
                        &pane_for_send,
                        options,
                        permission,
                        |env, guard| {
                            Ok(sends::submit_text(
                                flock,
                                env,
                                &pane_for_send,
                                &text,
                                true,
                                false,
                                Some(guard),
                            ))
                        },
                    );
                    if !delivered {
                        inbox_for_send.record_notify(
                            &receipt_id,
                            "queued",
                            Some(&detail),
                            Some(&transport_id_for_send),
                            Some(notify_attempt),
                        )?;
                        return Ok(TransportResult::Held(detail));
                    }
                    message.detail = Some(detail.clone());
                    message.finished_at = Some(crate::clock::now_s());
                    if ok == Some(true) {
                        inbox_for_send.record_notify(
                            &receipt_id,
                            "sent",
                            None,
                            Some(&transport_id_for_send),
                            Some(notify_attempt),
                        )?;
                        message.notify_for.clear();
                        message.status = "sent".into();
                        flock.remember(&message);
                        flock.record_delivery_outcome(
                            &message,
                            true,
                            None,
                            "ok",
                            Some("message.sent"),
                        );
                        Ok(TransportResult::Sent {
                            transport_id: message.id,
                            delivered_at: message.finished_at.unwrap_or_else(crate::clock::now_s),
                        })
                    } else {
                        inbox_for_send.record_notify(
                            &receipt_id,
                            "undeliverable",
                            Some(&detail),
                            Some(&transport_id_for_send),
                            Some(notify_attempt),
                        )?;
                        message.status = "failed".into();
                        message.notify_for.clear();
                        flock.remember(&message);
                        flock.record_delivery_outcome(
                            &message,
                            false,
                            Some(&detail),
                            "undeliverable",
                            Some("message.sent"),
                        );
                        Ok(TransportResult::Undeliverable {
                            transport_id: message.id,
                            reason: detail,
                        })
                    }
                },
            )
            .await;
        let result = match sent {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                let reason = format!("delivery_error: {error}");
                record_next_job_notify(
                    Arc::clone(&inbox),
                    message_id,
                    "undeliverable",
                    Some(reason.clone()),
                    Some(transport_id),
                    Some(attempt),
                )
                .await?;
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("undeliverable".into())),
                    ("reason".into(), Value::String(reason)),
                ]);
                return tasks.update_next_job(&reserved, None, changes).await;
            }
            Err(error) => {
                let reason = format!("delivery_error: terminal worker failed: {error}");
                record_next_job_notify(
                    Arc::clone(&inbox),
                    message_id,
                    "undeliverable",
                    Some(reason.clone()),
                    Some(transport_id),
                    Some(attempt),
                )
                .await?;
                let changes = serde_json::Map::from_iter([
                    ("state".into(), Value::String("undeliverable".into())),
                    ("reason".into(), Value::String(reason)),
                ]);
                return tasks.update_next_job(&reserved, None, changes).await;
            }
        };
        let (state, reason, transport_id, delivered_at) = match result {
            TransportResult::Held(reason) => ("pending", reason, None, None),
            TransportResult::Sent {
                transport_id,
                delivered_at,
            } => (
                "sent",
                String::new(),
                Some(transport_id),
                Some(delivered_at),
            ),
            TransportResult::Undeliverable {
                transport_id,
                reason,
            } => ("undeliverable", reason, Some(transport_id), None),
        };
        let mut changes = serde_json::Map::new();
        changes.insert("state".into(), Value::String(state.into()));
        changes.insert(
            "reason".into(),
            if reason.is_empty() {
                Value::Null
            } else {
                Value::String(reason.clone())
            },
        );
        changes.insert(
            "transport_id".into(),
            transport_id.map_or(Value::Null, Value::String),
        );
        changes.insert(
            "delivered_at".into(),
            delivered_at.map_or(Value::Null, Value::from),
        );
        let updated = tasks.update_next_job(&reserved, None, changes).await?;
        if state == "sent" {
            self.bind_turn_locked(&pane).await?;
        }
        Ok(updated)
    }

    #[allow(dead_code)] // Delivery and turn observation use this lock after worker wiring.
    fn pane_lock(&self, pane: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(pane.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    #[allow(dead_code)] // Used by delivery while the next-job worker leaf is being wired.
    async fn lock_pane(&self, pane: &str) -> OwnedMutexGuard<()> {
        self.pane_lock(pane).lock_owned().await
    }

    /// Bind a sent handoff to the producer generation observed for its pane.
    ///
    /// The sender owns this lock while it writes the delivery receipt, so an
    /// observation arriving during that window is bound by the sender's final
    /// `bind_turn_locked` call instead of waiting here.
    #[allow(dead_code)] // Wired by the following next-job tick/recovery leaf.
    pub(crate) async fn bind_turn(&self, pane: String) -> Result<(), DunkError> {
        let lock = self.pane_lock(&pane);
        let Ok(_guard) = lock.try_lock_owned() else {
            return Ok(());
        };
        self.bind_turn_locked(&pane).await
    }

    /// Bind while the caller already owns this pane's async lock.
    #[allow(dead_code)] // Delivery uses this while the next-job worker leaf is being wired.
    async fn bind_turn_locked(&self, pane: &str) -> Result<(), DunkError> {
        let stores = self.flock.stores();
        let tasks = stores
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let inbox = stores
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store unavailable".into()))?;

        for handoff in tasks.next_job_handoffs(Some(pane)).await? {
            if handoff.get("state").and_then(Value::as_str) != Some("sent")
                || handoff
                    .get("delivered_at")
                    .and_then(Value::as_f64)
                    .is_none_or(|at| at == 0.0)
            {
                continue;
            }
            let Some(message_id) = handoff.get("message_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(turn) = handoff.get("observed_turn").and_then(Value::as_object) else {
                continue;
            };
            let Some(turn_id) = turn
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|turn_id| !turn_id.is_empty())
            else {
                continue;
            };
            if turn.get("handoff_id").and_then(Value::as_str) != Some(message_id) {
                continue;
            }
            let Some(started_at) = turn.get("started_at").and_then(Value::as_f64) else {
                continue;
            };
            let Some(attempt_at) = handoff.get("attempt_at").and_then(Value::as_f64) else {
                continue;
            };
            if started_at < attempt_at {
                continue;
            }

            let receipt =
                crate::inbox::status(inbox.read_pool(), message_id, crate::clock::now_s())?;
            if receipt.notify_state.as_deref() != Some("sent") {
                continue;
            }

            let mut changes = serde_json::Map::new();
            changes.insert("state".into(), Value::String("started".into()));
            changes.insert("turn_id".into(), Value::String(turn_id.to_owned()));
            changes.insert("turn_started_at".into(), Value::from(started_at));
            changes.insert(
                "turn_source".into(),
                turn.get("source").cloned().unwrap_or(Value::Null),
            );
            changes.insert(
                "turn_caller".into(),
                turn.get("caller").cloned().unwrap_or(Value::Null),
            );
            tasks.update_next_job(&handoff, None, changes).await?;
        }
        Ok(())
    }

    /// Persist a matching producer generation before trying the pane lock. A sender may
    /// currently hold that lock while waiting for this producer callback to return, so the
    /// observation itself must remain nonblocking with respect to pane serialization.
    #[allow(dead_code)] // Wired by the following producer-turn callback leaf.
    pub(crate) async fn observe_turn(
        &self,
        pane: String,
        turn: TurnState,
    ) -> Result<(), DunkError> {
        let Some(handoff_id) = turn.handoff_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        let tasks = self
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        for handoff in tasks.next_job_handoffs(Some(&pane)).await? {
            if handoff.get("message_id").and_then(Value::as_str) != Some(handoff_id)
                || !matches!(
                    handoff.get("state").and_then(Value::as_str),
                    Some("sending" | "sent")
                )
                || handoff
                    .get("attempt_at")
                    .and_then(Value::as_f64)
                    .is_none_or(|attempt_at| turn.started_at < attempt_at)
            {
                continue;
            }
            let Some(attempt) = handoff.get("attempt").and_then(Value::as_i64) else {
                continue;
            };
            let mut observed = serde_json::Map::new();
            observed.insert("turn_id".into(), Value::String(turn.turn_id.clone()));
            observed.insert("started_at".into(), Value::from(turn.started_at));
            observed.insert(
                "source".into(),
                turn.source.clone().map_or(Value::Null, Value::String),
            );
            observed.insert("caller".into(), turn.caller.clone().unwrap_or(Value::Null));
            observed.insert("handoff_id".into(), Value::String(handoff_id.to_owned()));
            let expected = serde_json::Map::from_iter([
                ("attempt".into(), Value::from(attempt)),
                ("observed_turn".into(), Value::Null),
            ]);
            let changes =
                serde_json::Map::from_iter([("observed_turn".into(), Value::Object(observed))]);
            if tasks
                .update_next_job(&handoff, Some(expected), changes)
                .await?
                .is_some()
            {
                self.flock.wake_push();
            }
        }
        self.bind_turn(pane).await
    }

    /// Dispatch permission is distinct from task allocation: registration never bypasses
    /// the global switch and an unreadable dependency cannot become permission.
    #[allow(dead_code)] // Consumed by the next-job transition leaves.
    pub(crate) async fn gate(&self, pane: &str, lanes: Option<Vec<Value>>) -> Option<String> {
        let env = match (self.env_provider)() {
            Ok(env) => env,
            Err(()) => return Some("readiness_unknown".into()),
        };
        self.gate_with(pane, lanes, env, self.terminal_factory.clone())
            .await
    }

    async fn gate_with(
        &self,
        pane: &str,
        lanes: Option<Vec<Value>>,
        env: Env,
        factory: Option<LoopTerminalFactory>,
    ) -> Option<String> {
        if matches!(
            env.get("DUNKINGSHEEP_TURN_END_DISPATCH")
                .unwrap_or("1")
                .trim()
                .to_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off"
        ) {
            return Some("disabled".into());
        }
        let lanes = match lanes {
            Some(lanes) => lanes,
            None => {
                let factory = match factory.or_else(|| configured_loop_terminal_factory().ok()) {
                    Some(factory) => factory,
                    None => return Some("readiness_unknown".into()),
                };
                let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), factory);
                match adapter
                    .with_loop_terminal(|flock, terminal| {
                        flock.resolve_lanes(terminal, crate::clock::now_s())
                    })
                    .await
                {
                    Ok(Ok(lanes)) => lanes,
                    _ => return Some("readiness_unknown".into()),
                }
            }
        };
        let Some(tasks) = self.flock.stores().tasks else {
            return Some("readiness_unknown".into());
        };
        let status = match tasks.allocation_status().await {
            Ok(status) => status,
            Err(_) => return Some("readiness_unknown".into()),
        };
        let Some(workers) = status.get("workers").and_then(Value::as_array) else {
            return Some("readiness_unknown".into());
        };
        let registered = workers.iter().any(|row| {
            row.get("pane_id").and_then(Value::as_str) == Some(pane)
                && row.get("enabled").is_some_and(|value| {
                    value
                        .as_bool()
                        .unwrap_or_else(|| value.as_i64().is_some_and(|n| n != 0))
                })
        });
        let env_for_config = env.clone();
        let wanted =
            match tokio::task::spawn_blocking(move || dispatch_entries(&env_for_config)).await {
                Ok(Ok(wanted)) => wanted,
                _ => return Some("readiness_unknown".into()),
            };
        let allowed = dispatch_allowlist(&wanted, &lanes);
        if !allowed.contains(pane) && !registered {
            // t4697: neither listed by hand nor an allocation worker. A lane with a live
            // worker or reviewer registration and a project or global membership is served
            // too, unless the owner excluded it. The file list above stays an override.
            let flock = Arc::clone(&self.flock);
            let env_for_exclusions = env.clone();
            let owned_pane = pane.to_owned();
            let snapshot = lanes.clone();
            let eligible = tokio::task::spawn_blocking(move || {
                let exclude = dispatch_exclusions(&env_for_exclusions)?;
                Ok::<bool, ()>(flock.handoff_role_eligible(&owned_pane, &snapshot, &exclude))
            })
            .await;
            match eligible {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => return Some("not_allowed".into()),
                _ => return Some("readiness_unknown".into()),
            }
        }
        let mut present = false;
        for lane in &lanes {
            let Some(row) = lane.as_object() else {
                return Some("readiness_unknown".into());
            };
            let Some(id) = row.get("pane_id") else {
                return Some("readiness_unknown".into());
            };
            if id.as_str() == Some(pane) {
                present = true;
                break;
            }
        }
        if !present {
            return Some("unavailable_lane".into());
        }
        let path = match crate::panes::default_path(&env) {
            Ok(path) => path,
            Err(_) => return Some("readiness_unknown".into()),
        };
        let owned_pane = pane.to_owned();
        match tokio::task::spawn_blocking(move || retirement_status(&owned_pane, &path)).await {
            Ok(Ok(true)) => Some("unavailable_lane".into()),
            Ok(Ok(false)) => None,
            _ => Some("readiness_unknown".into()),
        }
    }
}

/// Read one snapshot so an unreadable or corrupt retirement registry cannot become
/// permission. A missing file has the shared store's accepted empty semantics.
pub(crate) fn retirement_status(pane: &str, path: &std::path::Path) -> Result<bool, ()> {
    let records: BTreeMap<String, Value> =
        crate::atomic_json::read_or_default(path).map_err(|_| ())?;
    let Some(record) = records.get(pane.trim()) else {
        return Ok(false);
    };
    serde_json::from_value::<crate::panes::Retired>(record.clone()).map_err(|_| ())?;
    Ok(true)
}

pub(crate) fn process_gate_env() -> Result<Env, ()> {
    let mut values = Vec::new();
    for key in [
        "DUNKINGSHEEP_TURN_END_DISPATCH",
        "DUNKINGSHEEP_DIR",
        "HOME",
        "USERPROFILE",
        "DUNKINGSHEEP_DISPATCH_LANES",
    ] {
        if let Some(value) = std::env::var_os(key) {
            let value = value.into_string().map_err(|_| ())?;
            if key == "DUNKINGSHEEP_TURN_END_DISPATCH"
                && matches!(
                    value.trim().to_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            {
                return Ok(Env::from_pairs(&[(key, value.as_str())]));
            }
            values.push((key, value));
        }
    }
    let refs: Vec<_> = values
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    Ok(Env::from_pairs(&refs))
}

/// Config file precedes the environment; an empty/missing list falls back to the environment.
pub(crate) fn dispatch_entries(env: &Env) -> Result<Vec<String>, ()> {
    let path = crate::config::config_dir(env)
        .map_err(|_| ())?
        .join("dispatch_lanes.json");
    let mut entries = Vec::new();
    match std::fs::read_to_string(path) {
        Ok(data) => {
            let loaded: Value = serde_json::from_str(&data).map_err(|_| ())?;
            let listed = if let Some(object) = loaded.as_object() {
                object.get("lanes").unwrap_or(&Value::Null)
            } else {
                &loaded
            };
            match listed {
                Value::Null => {}
                Value::Bool(false) => {}
                Value::Number(number) if number.as_f64() == Some(0.0) => {}
                Value::Object(object) if object.is_empty() => {}
                Value::String(text) => entries.extend(
                    text.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                ),
                Value::Array(values) => {
                    for value in values {
                        let text = match value {
                            Value::String(text) => text.clone(),
                            Value::Number(_) => value.to_string(),
                            Value::Bool(value) => if *value { "True" } else { "False" }.into(),
                            Value::Null => "None".into(),
                            _ => return Err(()),
                        };
                        entries.extend(
                            text.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned),
                        );
                    }
                }
                _ => return Err(()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    if entries.is_empty() {
        entries.extend(
            env.get("DUNKINGSHEEP_DISPATCH_LANES")
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(entries)
}

/// t4697: the owner's exclusions from role-based handoff, the `exclude` key of
/// `dispatch_lanes.json`: role names, pane ids or agent names, as a list or a comma-separated
/// string. A missing file or key excludes nothing; anything else unreadable is an error, so a
/// broken exclusion list cannot widen who is served.
pub(crate) fn dispatch_exclusions(env: &Env) -> Result<Vec<String>, ()> {
    let path = crate::config::config_dir(env)
        .map_err(|_| ())?
        .join("dispatch_lanes.json");
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(()),
    };
    let loaded: Value = serde_json::from_str(&data).map_err(|_| ())?;
    let texts: Vec<String> = match loaded.get("exclude") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or(()))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(()),
    };
    Ok(texts
        .iter()
        .flat_map(|text| text.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(crate) fn dispatch_allowlist(entries: &[String], lanes: &[Value]) -> BTreeSet<String> {
    let mut resolved = BTreeSet::new();
    for entry in entries {
        let wanted = entry.to_lowercase();
        let matches: Vec<_> = lanes
            .iter()
            .filter_map(|lane| {
                let id = lane
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())?;
                let name = lane.get("agent_name").and_then(Value::as_str).unwrap_or("");
                (id.to_lowercase() == wanted || name.to_lowercase() == wanted)
                    .then(|| id.to_string())
            })
            .collect();
        if matches.is_empty() {
            resolved.insert(entry.clone());
        } else {
            resolved.extend(matches);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    async fn materialise_fixture() -> (
        tempfile::TempDir,
        NextJobs,
        Arc<crate::tasks::Tasks>,
        Arc<crate::inbox_store::Store>,
    ) {
        let (dir, jobs, tasks) = configured_jobs().await;
        let db = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = Arc::new(
            tokio::task::spawn_blocking(move || crate::inbox_store::Store::open(db, blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        jobs.flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(Arc::clone(&inbox)),
            ..Default::default()
        });
        (dir, jobs, tasks, inbox)
    }

    fn fake_factory() -> LoopTerminalFactory {
        Arc::new(|| Box::new(crate::terminal::fakes::FakeTerminal::new()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_tick_skips_terminal_and_continues_after_one_failure() {
        async fn record(
            tasks: &crate::tasks::Tasks,
            task_id: &str,
            message_id: &str,
            state: &str,
            attempt: i64,
            attempt_at: f64,
            mut details: Map<String, Value>,
        ) {
            details.insert("state".into(), json!(state));
            details.insert("attempt".into(), json!(attempt));
            details.insert("attempt_at".into(), json!(attempt_at));
            tasks
                .record_next_job_handoff(
                    task_id,
                    "w1:p1",
                    None,
                    Some(message_id),
                    attempt,
                    None,
                    Some(attempt_at),
                    Some(details),
                )
                .await
                .unwrap();
        }

        async fn seed_receipt(
            inbox: Arc<crate::inbox_store::Store>,
            message_id: &'static str,
            state: &'static str,
        ) {
            tokio::task::spawn_blocking(move || {
                inbox.ensure_next_job(message_id, "w1:p1", "w1:p1", "body", "subject")?;
                inbox.record_notify(message_id, state, None, Some("transport-1"), Some(1))
            })
            .await
            .unwrap()
            .unwrap();
        }

        let (_dir, jobs, tasks, inbox) = materialise_fixture().await;
        let failed = tasks
            .add("global", "Deleted before delivery", "body")
            .await
            .unwrap();
        record(
            &tasks,
            &failed.id,
            "nj-tick-failure",
            "pending",
            0,
            0.0,
            Map::new(),
        )
        .await;
        tasks.delete(&failed.id).await.unwrap();

        let terminal = tasks
            .add("global", "Terminal handoffs", "body")
            .await
            .unwrap();
        for (message_id, state) in [
            ("nj-tick-started", "started"),
            ("nj-tick-undeliverable", "undeliverable"),
            ("nj-tick-cancelled", "cancelled"),
        ] {
            record(&tasks, &terminal.id, message_id, state, 1, 10.0, Map::new()).await;
        }

        let interrupted = tasks
            .add("global", "Interrupted handoff", "body")
            .await
            .unwrap();
        let attempt_at = crate::clock::now_s() - 10.0;
        record(
            &tasks,
            &interrupted.id,
            "nj-tick-interrupted",
            "sending",
            1,
            attempt_at,
            json!({"transport_owner":{"pid":-1,"start":null}})
                .as_object()
                .unwrap()
                .clone(),
        )
        .await;
        seed_receipt(Arc::clone(&inbox), "nj-tick-interrupted", "queued").await;

        let sent = tasks.add("global", "Sent handoff", "body").await.unwrap();
        let attempt_at = crate::clock::now_s() - 2.0;
        let message_id = "nj-tick-sent";
        record(
            &tasks,
            &sent.id,
            message_id,
            "sent",
            1,
            attempt_at,
            json!({
                "delivered_at": attempt_at + 1.0,
                "transport_id": "transport-1",
                "observed_turn": {
                    "turn_id": "turn-after-restart",
                    "started_at": attempt_at + 0.5,
                    "source": "test",
                    "caller": null,
                    "handoff_id": message_id
                }
            })
            .as_object()
            .unwrap()
            .clone(),
        )
        .await;
        seed_receipt(Arc::clone(&inbox), message_id, "sent").await;
        let unassigned = tasks
            .add("global", "Not a durable handoff", "body")
            .await
            .unwrap();

        jobs.tick(true).await.unwrap();

        let handoffs = tasks.next_job_handoffs(None).await.unwrap();
        let by_message: std::collections::HashMap<_, _> = handoffs
            .iter()
            .map(|handoff| {
                (
                    handoff["message_id"].as_str().unwrap(),
                    handoff["state"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(by_message["nj-tick-started"], "started");
        assert_eq!(by_message["nj-tick-undeliverable"], "undeliverable");
        assert_eq!(by_message["nj-tick-cancelled"], "cancelled");
        assert_eq!(by_message["nj-tick-interrupted"], "undeliverable");
        assert_eq!(by_message[message_id], "started");
        assert_eq!(tasks.get(&unassigned.id).await.unwrap().status, "open");
        let receipt = crate::inbox::status(
            inbox.read_pool(),
            "nj-tick-interrupted",
            crate::clock::now_s(),
        )
        .unwrap();
        assert_eq!(receipt.notify_state.as_deref(), Some("undeliverable"));

        tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_recover_restores_durable_pending_work() {
        let (_dir, original_jobs, tasks, inbox) = materialise_fixture().await;
        let jobs = NextJobs::with_providers(
            Arc::clone(&original_jobs.flock),
            Arc::new(|| {
                Ok(Env::from_pairs(&[(
                    "DUNKINGSHEEP_TURN_END_DISPATCH",
                    "off",
                )]))
            }),
            None,
        );
        let caller = Caller::new(
            Some(123),
            Some("process-start".into()),
            Some("codex".into()),
            Some("w1:p1".into()),
            "agent",
        );
        let source_task = tasks
            .add("global", "Previous completion context", "body")
            .await
            .unwrap();
        jobs.advance_with_materialiser(
            AdvanceRequest {
                pane: "w1:p1",
                trigger: "turn.end",
                from_task: Some(&source_task.id),
                caller: Some(caller),
                operator_proof: None,
                dispatch_reason: Some("disabled".into()),
                unread: None,
                pending_delivery: false,
            },
            |_| async { unreachable!("a refused decision cannot create a handoff") },
        )
        .await
        .unwrap();
        jobs.advance_with_materialiser(
            AdvanceRequest {
                pane: "w2:p2",
                trigger: "turn.start",
                from_task: None,
                caller: None,
                operator_proof: None,
                dispatch_reason: None,
                unread: None,
                pending_delivery: false,
            },
            |_| async { unreachable!("a refused decision cannot create a handoff") },
        )
        .await
        .unwrap();
        jobs.advance_with_materialiser(
            AdvanceRequest {
                pane: "w2:p2",
                trigger: "turn.end",
                from_task: None,
                caller: None,
                operator_proof: None,
                dispatch_reason: None,
                unread: Some(0),
                pending_delivery: false,
            },
            |_| async { unreachable!("explicit no-work cannot create a handoff") },
        )
        .await
        .unwrap();

        let unassigned = tasks
            .add("global", "Never poll unassigned backlog", "body")
            .await
            .unwrap();
        let interrupted = tasks
            .add("global", "Uncertain send is never replayed", "body")
            .await
            .unwrap();
        let message_id = "nj-recover-uncertain";
        let attempt_at = crate::clock::now_s() - 10.0;
        tasks
            .record_next_job_handoff(
                &interrupted.id,
                "w3:p3",
                None,
                Some(message_id),
                1,
                None,
                Some(attempt_at),
                Some(
                    json!({
                        "state":"sending",
                        "attempt":1,
                        "attempt_at":attempt_at,
                        "transport_owner":{"pid":-1,"start":null}
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )
            .await
            .unwrap();
        let receipt_store = Arc::clone(&inbox);
        tokio::task::spawn_blocking(move || {
            receipt_store.ensure_next_job(message_id, "w3:p3", "w3:p3", "body", "subject")?;
            receipt_store.record_notify(message_id, "queued", None, Some("transport-1"), Some(1))
        })
        .await
        .unwrap()
        .unwrap();

        jobs.recover().await.unwrap();

        let decisions = tasks.next_job_decisions().await.unwrap();
        let recovered = decisions
            .iter()
            .find(|(pane, _)| pane == "w1:p1")
            .map(|(_, plan)| plan)
            .expect("eligible decision remains journaled");
        assert_eq!(recovered["trigger"], "restart");
        assert_eq!(recovered["reason"], "disabled");
        assert_eq!(recovered["caller"]["pane_id"], "w1:p1");
        assert_eq!(recovered["caller"]["pid"], 123);
        assert_eq!(recovered["caller"]["name"], "codex");
        assert_eq!(recovered["caller"]["source"], "agent");
        assert_eq!(recovered["from_task"], source_task.id);
        let terminal = decisions
            .iter()
            .find(|(pane, _)| pane == "w2:p2")
            .map(|(_, plan)| plan)
            .expect("no-work decision remains journaled");
        assert_eq!(terminal["trigger"], "turn.end");
        assert_eq!(terminal["reason"], "no_assigned_work");

        let handoff = tasks
            .next_job_handoffs(Some("w3:p3"))
            .await
            .unwrap()
            .into_iter()
            .find(|handoff| handoff["message_id"] == message_id)
            .expect("durable handoff was reconciled");
        assert_eq!(handoff["state"], "undeliverable");
        assert_eq!(handoff["reason"], "delivery_interrupted");
        assert_eq!(handoff["attempt"], 1, "recovery must not resend");
        let receipt =
            crate::inbox::status(inbox.read_pool(), message_id, crate::clock::now_s()).unwrap();
        assert_eq!(receipt.notify_state.as_deref(), Some("undeliverable"));
        assert_eq!(tasks.get(&unassigned.id).await.unwrap().status, "open");

        tokio::task::spawn_blocking(move || drop((jobs, original_jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_materialise_is_idempotent_and_preserves_body() {
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let task = tasks
            .add("global", "Fix the gate", "First line\nSecond line")
            .await
            .unwrap();
        let h = json!({"pane_id":"w1:p1", "task_id":task.id, "message_id":"nj-test", "from_task":"t-old", "trigger":"completion"}).as_object().unwrap().clone();
        let env = || test_env(dir.path(), "1", "w1:p1");
        let first = jobs
            .materialise_with(&h, env(), Some(fake_factory()))
            .await
            .unwrap();
        assert_eq!(first.message_id, "nj-test");
        assert_eq!(first.pane_id.as_deref(), Some("w1:p1"));
        let receipt_store = Arc::clone(&inbox);
        tokio::task::spawn_blocking(move || {
            receipt_store.record_notify(
                "nj-test",
                "sent",
                Some("accepted"),
                Some("receipt-1"),
                Some(1),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let second = jobs
            .materialise_with(&h, env(), Some(fake_factory()))
            .await
            .unwrap();
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(first.mailbox, second.mailbox);
        assert_eq!(second.notify_state.as_deref(), Some("sent"));
        assert_eq!(second.notify_transport_id.as_deref(), Some("receipt-1"));
        let pool = crate::db::ReadPool::open(dir.path().join("inbox.db"), 1).unwrap();
        let (summary, body) = crate::inbox::get(&pool, "nj-test", crate::clock::now_s()).unwrap();
        assert_eq!(
            summary.subject.as_deref(),
            Some(format!("TASK {}: Fix the gate", task.id).as_str())
        );
        assert_eq!(body, format!("TASK {} [low] Fix the gate\n\nFirst line\nSecond line\n\nAlready atomically claimed for w1:p1; start this job now.\nheartbeat: dunkingsheep task heartbeat {}\ncomplete: dunkingsheep task complete {}\nhandoff: nj-test from t-old", task.id, task.id, task.id));
        let count: i64 = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_materialise_refuses_changed_or_unavailable_pane() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let task = tasks.add("global", "Example", "body").await.unwrap();
        let h = json!({"pane_id":"w1:p1", "task_id":task.id, "message_id":"nj-unavailable", "trigger":"restart"}).as_object().unwrap().clone();
        let disabled = jobs
            .materialise_with(
                &h,
                test_env(dir.path(), "off", "w1:p1"),
                Some(fake_factory()),
            )
            .await;
        assert!(
            matches!(disabled, Err(MaterialiseError::DispatchPaused(ref reason)) if reason == "disabled")
        );
        let missing_task = json!({"pane_id":"w1:p1", "task_id":"t999999999", "message_id":"nj-missing", "trigger":"restart"}).as_object().unwrap().clone();
        let result = jobs
            .materialise_with(
                &missing_task,
                test_env(dir.path(), "1", "w1:p1"),
                Some(fake_factory()),
            )
            .await;
        assert!(
            matches!(result, Err(MaterialiseError::Other(DunkError::Refused(ref reason))) if reason == "no task with id \"t999999999\"")
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let unavailable: LoopTerminalFactory = Arc::new(move || {
            let mut fake = crate::terminal::fakes::FakeTerminal::new();
            if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                fake.panes.clear();
            }
            Box::new(fake)
        });
        let result = jobs
            .materialise_with(&h, test_env(dir.path(), "1", "w1:p1"), Some(unavailable))
            .await;
        assert!(matches!(result, Err(MaterialiseError::Other(_))));
        let h = json!({"pane_id":"worker", "task_id":task.id, "message_id":"nj-changed", "trigger":"restart"}).as_object().unwrap().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let changed: LoopTerminalFactory = Arc::new(move || {
            let mut fake = crate::terminal::fakes::FakeTerminal::new();
            fake.panes.truncate(1);
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                fake.panes[0].pane_id = "worker".into();
            } else {
                fake.panes[0].label = "worker".into();
            }
            Box::new(fake)
        });
        let result = jobs
            .materialise_with(&h, test_env(dir.path(), "1", "worker"), Some(changed))
            .await;
        assert!(
            matches!(result, Err(MaterialiseError::Other(DunkError::Refused(ref reason))) if reason == "next-job pane changed")
        );
        let count: i64 = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
    }

    fn next_jobs() -> NextJobs {
        NextJobs::new(Arc::new(Flock::new(PathBuf::new(), PathBuf::new(), false)))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_reuses_one_lock_per_pane() {
        let jobs = Arc::new(next_jobs());
        let first = jobs.pane_lock("%12");
        let repeated = jobs.pane_lock("%12");
        assert!(Arc::ptr_eq(&first, &repeated));

        let held = first
            .clone()
            .try_lock_owned()
            .expect("first lock available");
        assert!(repeated.try_lock_owned().is_err());
        drop(held);

        let owned_guard = jobs.lock_pane("%12").await;
        drop(owned_guard);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_locks_are_independent_across_panes() {
        let jobs = next_jobs();
        let first = jobs.pane_lock("%12");
        let second = jobs.pane_lock("%13");
        assert!(!Arc::ptr_eq(&first, &second));

        let first_guard = first.try_lock_owned().expect("first pane lock available");
        let second_guard = second.try_lock_owned().expect("second pane lock available");
        drop((first_guard, second_guard));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_bind_turn_requires_sent_receipt_and_matching_generation() {
        let (_dir, jobs, tasks, inbox) = materialise_fixture().await;

        #[derive(Clone, Copy)]
        struct HandoffSeed<'a> {
            pane: &'a str,
            message_id: &'a str,
            notify_state: &'a str,
            turn_id: &'a str,
            handoff_id: &'a str,
            started_at: f64,
        }

        async fn seed_handoff(
            tasks: &crate::tasks::Tasks,
            inbox: &Arc<crate::inbox_store::Store>,
            seed: HandoffSeed<'_>,
        ) {
            let HandoffSeed {
                pane,
                message_id,
                notify_state,
                turn_id,
                handoff_id,
                started_at,
            } = seed;
            let task = tasks.add("global", "Bind turn", "body").await.unwrap();
            let inbox = Arc::clone(inbox);
            let message_id = message_id.to_owned();
            let pane = pane.to_owned();
            let notify_state = notify_state.to_owned();
            let receipt_message_id = message_id.clone();
            let receipt_pane = pane.clone();
            let receipt_notify_id = message_id.clone();
            tokio::task::spawn_blocking(move || {
                inbox.ensure_next_job(
                    &receipt_message_id,
                    &receipt_pane,
                    &receipt_pane,
                    "body",
                    "subject",
                )?;
                inbox.record_notify(
                    &receipt_notify_id,
                    &notify_state,
                    None,
                    Some("transport"),
                    Some(1),
                )
            })
            .await
            .unwrap()
            .unwrap();
            let details = json!({
                "state": "sent",
                "attempt_at": 100.0,
                "delivered_at": 200.0,
                "observed_turn": {
                    "turn_id": turn_id,
                    "started_at": started_at,
                    "source": "producer",
                    "caller": "worker",
                    "handoff_id": handoff_id
                }
            })
            .as_object()
            .unwrap()
            .clone();
            tasks
                .record_next_job_handoff(
                    &task.id,
                    &pane,
                    None,
                    Some(&message_id),
                    1,
                    None,
                    Some(201.0),
                    Some(details),
                )
                .await
                .unwrap();
        }

        seed_handoff(
            &tasks,
            &inbox,
            HandoffSeed {
                pane: "%valid",
                message_id: "nj-bind-valid",
                notify_state: "sent",
                turn_id: "turn-1",
                handoff_id: "nj-bind-valid",
                started_at: 101.0,
            },
        )
        .await;
        let lock = jobs.pane_lock("%valid");
        let held = lock.clone().try_lock_owned().unwrap();
        jobs.bind_turn("%valid".into()).await.unwrap();
        let busy_result = tasks.next_job_handoffs(Some("%valid")).await.unwrap();
        assert_eq!(
            busy_result[0].get("state").and_then(Value::as_str),
            Some("sent")
        );
        assert!(busy_result[0].get("turn_id").is_none());
        drop(held);

        jobs.bind_turn("%valid".into()).await.unwrap();
        let valid = tasks.next_job_handoffs(Some("%valid")).await.unwrap();
        assert_eq!(
            valid[0].get("state").and_then(Value::as_str),
            Some("started")
        );
        assert_eq!(
            valid[0].get("turn_id").and_then(Value::as_str),
            Some("turn-1")
        );
        assert_eq!(
            valid[0].get("turn_started_at").and_then(Value::as_f64),
            Some(101.0)
        );
        assert_eq!(
            valid[0].get("turn_source").and_then(Value::as_str),
            Some("producer")
        );
        assert_eq!(
            valid[0].get("turn_caller").and_then(Value::as_str),
            Some("worker")
        );

        for (pane, message_id, notify, turn_id, observed_id, started_at) in [
            (
                "%pending",
                "nj-bind-pending",
                "queued",
                "turn-2",
                "nj-bind-pending",
                101.0,
            ),
            (
                "%wrong",
                "nj-bind-wrong",
                "sent",
                "turn-3",
                "nj-other",
                101.0,
            ),
            (
                "%early",
                "nj-bind-early",
                "sent",
                "turn-4",
                "nj-bind-early",
                99.0,
            ),
            (
                "%empty",
                "nj-bind-empty",
                "sent",
                "",
                "nj-bind-empty",
                101.0,
            ),
        ] {
            seed_handoff(
                &tasks,
                &inbox,
                HandoffSeed {
                    pane,
                    message_id,
                    notify_state: notify,
                    turn_id,
                    handoff_id: observed_id,
                    started_at,
                },
            )
            .await;
            jobs.bind_turn(pane.to_owned()).await.unwrap();
            let rows = tasks.next_job_handoffs(Some(pane)).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get("state").and_then(Value::as_str), Some("sent"));
            assert!(rows[0].get("turn_id").is_none());
        }

        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_observe_turn_records_state_and_wakes_after_busy_bind() {
        let (_dir, jobs, tasks, inbox) = materialise_fixture().await;
        let pane = "%observed".to_owned();
        let message_id = "nj-observed";
        let task = tasks.add("global", "Observe turn", "body").await.unwrap();
        let inbox_store = Arc::clone(&inbox);
        let receipt_id = message_id.to_owned();
        let receipt_pane = pane.clone();
        tokio::task::spawn_blocking(move || {
            inbox_store.ensure_next_job(
                &receipt_id,
                &receipt_pane,
                &receipt_pane,
                "body",
                "subject",
            )?;
            inbox_store.record_notify(&receipt_id, "queued", None, Some("attempt"), Some(1))
        })
        .await
        .unwrap()
        .unwrap();
        let details = json!({"state":"sending", "attempt_at":100.0, "observed_turn":null})
            .as_object()
            .unwrap()
            .clone();
        tasks
            .record_next_job_handoff(
                &task.id,
                &pane,
                None,
                Some(message_id),
                1,
                None,
                Some(101.0),
                Some(details),
            )
            .await
            .unwrap();

        let wake = jobs.flock.push_wake();
        let held = jobs.pane_lock(&pane).try_lock_owned().unwrap();
        let turn = TurnState {
            started_at: 101.0,
            expires_at: 200.0,
            ended_at: None,
            ttl_s: 99.0,
            turn_id: "turn-observed".into(),
            source: Some("producer".into()),
            caller: Some(json!("worker")),
            handoff_id: Some(message_id.into()),
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            jobs.observe_turn(pane.clone(), turn),
        )
        .await
        .expect("observation must not wait for a busy pane lock")
        .unwrap();

        let rows = tasks.next_job_handoffs(Some(&pane)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("state").and_then(Value::as_str),
            Some("sending")
        );
        assert_eq!(
            rows[0]["observed_turn"],
            json!({
                "turn_id":"turn-observed",
                "started_at":101.0,
                "source":"producer",
                "caller":"worker",
                "handoff_id":message_id
            })
        );
        tokio::time::timeout(std::time::Duration::from_millis(100), wake.notified())
            .await
            .expect("a persisted observation must wake the worker");
        drop(held);

        tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_delivery_reserves_before_transport_and_reconciles_receipts() {
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let pane = "w1:p1";
        let task = tasks
            .add("global", "Deliver next job", "body")
            .await
            .unwrap();
        tasks.assign(&task.id, &[pane], "set").await.unwrap();
        tasks.claim_for_test(&task.id, pane).await;
        let details = json!({
            "state":"pending",
            "attempt":0,
            "attempt_at":0.0,
            "observed_turn":null,
            "trigger":"completion"
        })
        .as_object()
        .unwrap()
        .clone();
        tasks
            .record_next_job_handoff(
                &task.id,
                pane,
                Some("t-source"),
                Some("nj-deliver"),
                0,
                None,
                Some(1.0),
                Some(details),
            )
            .await
            .unwrap();
        let handoff = tasks.next_job_handoffs(Some(pane)).await.unwrap().remove(0);
        let delivered = jobs
            .deliver_with(
                handoff,
                false,
                Ok(test_env(dir.path(), "1", pane)),
                Some(fake_factory()),
            )
            .await
            .unwrap()
            .expect("reserved handoff remains journalled");
        assert_eq!(delivered.get("state").and_then(Value::as_str), Some("sent"));
        assert_eq!(delivered.get("attempt").and_then(Value::as_i64), Some(1));
        assert_eq!(
            delivered.get("transport_id").and_then(Value::as_str),
            Some("nj-deliver:1")
        );
        let receipt =
            crate::inbox::status(inbox.read_pool(), "nj-deliver", crate::clock::now_s()).unwrap();
        assert_eq!(receipt.notify_state.as_deref(), Some("sent"));
        assert_eq!(receipt.notify_attempt, Some(1));
        assert_eq!(receipt.notify_transport_id.as_deref(), Some("nj-deliver:1"));
        assert!(jobs.flock.outbox_snapshot().is_empty());

        tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_delivery_distinguishes_materialise_pause_from_error() {
        for (message_id, fail_factory_call, expected_state, expected_notify) in [
            ("nj-materialise-pause", 2usize, "pending", "queued"),
            (
                "nj-materialise-error",
                3usize,
                "undeliverable",
                "undeliverable",
            ),
        ] {
            let (dir, jobs, tasks, inbox) = materialise_fixture().await;
            let pane = "w1:p1";
            let task = tasks
                .add("global", "Materialise next job", "body")
                .await
                .unwrap();
            tasks.assign(&task.id, &[pane], "set").await.unwrap();
            tasks.claim_for_test(&task.id, pane).await;
            let details = json!({
                "state":"pending",
                "attempt":0,
                "attempt_at":0.0,
                "observed_turn":null,
                "trigger":"completion"
            })
            .as_object()
            .unwrap()
            .clone();
            tasks
                .record_next_job_handoff(
                    &task.id,
                    pane,
                    Some("t-source"),
                    Some(message_id),
                    0,
                    None,
                    Some(1.0),
                    Some(details),
                )
                .await
                .unwrap();
            let handoff = tasks.next_job_handoffs(Some(pane)).await.unwrap().remove(0);
            let inbox_store = Arc::clone(&inbox);
            let receipt_id = message_id.to_owned();
            let receipt_pane = pane.to_owned();
            tokio::task::spawn_blocking(move || {
                inbox_store.ensure_next_job(
                    &receipt_id,
                    &receipt_pane,
                    &receipt_pane,
                    "body",
                    "subject",
                )
            })
            .await
            .unwrap()
            .unwrap();

            let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls = Arc::clone(&factory_calls);
            let factory: LoopTerminalFactory = Arc::new(move || {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let mut terminal = crate::terminal::fakes::FakeTerminal::new();
                if call == fail_factory_call {
                    terminal.panes.clear();
                }
                Box::new(terminal)
            });
            let delivered = jobs
                .deliver_with(
                    handoff,
                    false,
                    Ok(test_env(dir.path(), "1", pane)),
                    Some(factory),
                )
                .await
                .unwrap()
                .expect("reserved handoff remains journalled");

            assert_eq!(
                delivered.get("state").and_then(Value::as_str),
                Some(expected_state)
            );
            assert_eq!(delivered.get("attempt").and_then(Value::as_i64), Some(1));
            assert_eq!(
                factory_calls.load(std::sync::atomic::Ordering::SeqCst),
                fail_factory_call,
                "transport must not be attempted after materialisation fails"
            );
            if expected_state == "undeliverable" {
                assert!(delivered
                    .get("reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason.starts_with("delivery_error:")));
            } else {
                assert_eq!(
                    delivered.get("reason").and_then(Value::as_str),
                    Some("unavailable_lane")
                );
            }
            let receipt =
                crate::inbox::status(inbox.read_pool(), message_id, crate::clock::now_s()).unwrap();
            assert_eq!(receipt.notify_state.as_deref(), Some(expected_notify));
            assert_eq!(receipt.notify_attempt, Some(1));
            assert!(jobs.flock.outbox_snapshot().is_empty());

            tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_delivery_rejects_dependency_added_after_handoff() {
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let pane = "w1:p1";
        let dependency = tasks
            .add("global", "Open prerequisite", "body")
            .await
            .unwrap();
        let task = tasks
            .add("global", "Deliver dependent task", "body")
            .await
            .unwrap();
        tasks.assign(&task.id, &[pane], "set").await.unwrap();
        tasks.claim_for_test(&task.id, pane).await;
        let details = json!({
            "state":"pending",
            "attempt":0,
            "attempt_at":0.0,
            "observed_turn":null,
            "trigger":"completion"
        })
        .as_object()
        .unwrap()
        .clone();
        tasks
            .record_next_job_handoff(
                &task.id,
                pane,
                Some("t-source"),
                Some("nj-new-dependency"),
                0,
                None,
                Some(1.0),
                Some(details),
            )
            .await
            .unwrap();
        tasks
            .update(
                &task.id,
                crate::tasks::FieldEdit {
                    depends_on: Some(vec![dependency.id.clone()]),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        let decorated = tasks.decorate(&task.id, Some(pane)).await.unwrap();
        assert_eq!(decorated.deps.blocked_by, vec![dependency.id]);

        let inbox_for_seed = Arc::clone(&inbox);
        tokio::task::spawn_blocking(move || {
            inbox_for_seed.ensure_next_job("nj-new-dependency", pane, pane, "body", "subject")
        })
        .await
        .unwrap()
        .unwrap();

        let handoff = tasks.next_job_handoffs(Some(pane)).await.unwrap().remove(0);
        let delivered = jobs
            .deliver_with(
                handoff,
                false,
                Ok(test_env(dir.path(), "1", pane)),
                Some(fake_factory()),
            )
            .await
            .unwrap()
            .expect("invalid handoff remains journalled");

        assert_eq!(
            delivered.get("state").and_then(Value::as_str),
            Some("undeliverable")
        );
        assert_eq!(
            delivered.get("reason").and_then(Value::as_str),
            Some("claim_no_longer_valid")
        );
        assert_eq!(delivered.get("attempt").and_then(Value::as_i64), Some(0));
        let receipt = crate::inbox::status(
            inbox.read_pool(),
            "nj-new-dependency",
            crate::clock::now_s(),
        )
        .unwrap();
        assert_eq!(receipt.notify_state.as_deref(), Some("undeliverable"));
        assert_eq!(receipt.notify_attempt, Some(0));
        assert!(jobs.flock.outbox_snapshot().is_empty());

        tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_delivery_rechecks_permission_inside_send_guard() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let pane = "w1:p1";
        let task = tasks
            .add("global", "Guarded next job", "body")
            .await
            .unwrap();
        tasks.assign(&task.id, &[pane], "set").await.unwrap();
        tasks.claim_for_test(&task.id, pane).await;
        let details = json!({
            "state":"pending",
            "attempt":0,
            "attempt_at":0.0,
            "observed_turn":null,
            "trigger":"completion"
        })
        .as_object()
        .unwrap()
        .clone();
        tasks
            .record_next_job_handoff(
                &task.id,
                pane,
                Some("t-source"),
                Some("nj-final-permission"),
                0,
                None,
                Some(1.0),
                Some(details),
            )
            .await
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::clone(&calls);
        let env_dir = dir.path().to_owned();
        let provider: GateEnvProvider = Arc::new(move || {
            let call = provider_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let switch = if call == 4 { "off" } else { "1" };
            Ok(test_env(&env_dir, switch, pane))
        });
        let handoff = tasks.next_job_handoffs(Some(pane)).await.unwrap().remove(0);
        let delivered = jobs
            .deliver_with_provider(handoff, false, provider, Some(fake_factory()))
            .await
            .unwrap()
            .expect("held handoff remains journalled");

        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            delivered.get("state").and_then(Value::as_str),
            Some("pending")
        );
        assert_eq!(
            delivered.get("reason").and_then(Value::as_str),
            Some("disabled")
        );
        assert_eq!(delivered.get("attempt").and_then(Value::as_i64), Some(1));
        let receipt = crate::inbox::status(
            inbox.read_pool(),
            "nj-final-permission",
            crate::clock::now_s(),
        )
        .unwrap();
        assert_eq!(receipt.notify_state.as_deref(), Some("queued"));
        assert_eq!(receipt.notify_attempt, Some(1));
        assert!(jobs.flock.outbox_snapshot().is_empty());

        tokio::task::spawn_blocking(move || drop((jobs, inbox, tasks)))
            .await
            .unwrap();
    }

    fn materialised_status(message_id: &str) -> crate::inbox::Status {
        crate::inbox::Status {
            message_id: message_id.to_owned(),
            state: "queued".into(),
            mailbox: "worker".into(),
            destination: "worker".into(),
            pane_id: Some("%1".into()),
            sender: Some("system".into()),
            created_at: crate::clock::now_s(),
            read_at: None,
            acked_at: None,
            archived_at: None,
            cancelled_at: None,
            waiting_s: Some(0),
            body_bytes: 0,
            attachments: 0,
            attachment_bytes: 0,
            reason: None,
            notify_state: None,
            notify_reason: None,
            notify_at: None,
            notify_transport_id: None,
            notify_attempt: None,
        }
    }

    async fn add_assigned_task(tasks: &crate::tasks::Tasks, pane: &str) {
        let task = tasks
            .add("global", "Advance to assigned work", "body")
            .await
            .unwrap();
        tasks.assign(&task.id, &[pane], "set").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_review_does_not_block_the_author_next_job() {
        // test_pending_review_does_not_block_author_next_job: the review is older and has the same
        // priority, but its author must never be handed it. The advance completes the source and
        // chooses through the runtime's real planner (dispatch::next_job_plan).
        let (_dir, jobs, tasks) = configured_jobs().await;
        let source = tasks
            .add("global", "source", "concrete acceptance")
            .await
            .unwrap();
        tasks.assign(&source.id, &["%leader"], "set").await.unwrap();
        let revision = "a".repeat(40);
        let review = tasks
            .allocation_review(
                &source.id,
                &revision,
                "review",
                "code",
                &json!([]),
                "%leader",
            )
            .await
            .unwrap();
        let successor = tasks
            .add("global", "next", "concrete acceptance")
            .await
            .unwrap();
        tasks
            .assign(&successor.id, &["%leader"], "set")
            .await
            .unwrap();
        let advance = jobs
            .advance_with_materialiser(
                AdvanceRequest {
                    pane: "%leader",
                    trigger: "completion",
                    from_task: Some(&source.id),
                    caller: None,
                    operator_proof: None,
                    dispatch_reason: None,
                    unread: Some(0),
                    pending_delivery: false,
                },
                |_| async {
                    Err(MaterialiseError::DispatchPaused(
                        "materialise is not what this case checks".to_owned(),
                    ))
                },
            )
            .await
            .unwrap();
        assert_eq!(advance.plan["action"], "claim");
        assert_eq!(
            advance.plan["task_id"].as_str(),
            Some(successor.id.as_str())
        );
        assert_eq!(tasks.get(&review.id).await.unwrap().status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_advance_records_handoff_and_progress() {
        let (_dir, jobs, tasks) = configured_jobs().await;
        let pane = "%1";
        add_assigned_task(&tasks, pane).await;
        let metrics = Arc::new(
            std::thread::spawn(|| crate::metrics::MetricsStore::open(":memory:"))
                .join()
                .expect("metrics store thread")
                .expect("metrics store"),
        );
        jobs.flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            metrics: Some(Arc::clone(&metrics)),
            ..Default::default()
        });

        let advance = jobs
            .advance_with_materialiser(
                AdvanceRequest {
                    pane,
                    trigger: "turn.end",
                    from_task: None,
                    caller: None,
                    operator_proof: None,
                    dispatch_reason: None,
                    unread: Some(0),
                    pending_delivery: false,
                },
                |handoff| async move {
                    let id = handoff
                        .get("message_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Ok(materialised_status(id))
                },
            )
            .await
            .unwrap();

        assert_eq!(advance.plan["action"], "claim");
        assert_eq!(advance.plan["handoff"]["state"], "pending");
        let claimed = tasks
            .get(advance.plan["task_id"].as_str().unwrap())
            .await
            .unwrap();
        assert_eq!(claimed.status, "in_progress");
        let handoffs = tasks.next_job_handoffs(Some(pane)).await.unwrap();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0]["state"], "pending");
        let events = std::thread::spawn({
            let metrics = Arc::clone(&metrics);
            move || metrics.events_between(0.0, f64::MAX)
        })
        .join()
        .expect("metrics events thread")
        .unwrap();
        assert!(events.iter().any(|event| {
            event.kind == "next_job.decision"
                && event.pane_id.as_deref() == Some(pane)
                && event.outcome.as_deref() == Some("claim")
        }));
        jobs.flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Default::default()
        });
        let metrics = Arc::try_unwrap(metrics).expect("only local metrics owner remains");
        std::thread::spawn(move || drop(metrics))
            .join()
            .expect("metrics store drop thread");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_unknown_unread_is_not_permission() {
        let (_dir, jobs, tasks) = configured_jobs().await;
        let pane = "%1";
        add_assigned_task(&tasks, pane).await;

        let advance = jobs
            .advance_with_materialiser(
                AdvanceRequest {
                    pane,
                    trigger: "turn.end",
                    from_task: None,
                    caller: None,
                    operator_proof: None,
                    dispatch_reason: None,
                    unread: None,
                    pending_delivery: false,
                },
                |_| async {
                    Err(MaterialiseError::Other(DunkError::Refused(
                        "unexpected materialisation".into(),
                    )))
                },
            )
            .await
            .unwrap();

        assert_eq!(advance.plan["action"], "skip");
        assert_eq!(advance.plan["reason"], "unread_unknown");
        assert!(advance.plan.get("handoff").is_none());
        assert!(tasks
            .next_job_handoffs(Some(pane))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_advance_matches_python_materialise_error_handling() {
        for (pause, reason) in [(true, "dispatch paused"), (false, "materialise failed")] {
            let (_dir, jobs, tasks) = configured_jobs().await;
            let pane = "%1";
            add_assigned_task(&tasks, pane).await;
            let materialise_error = if pause {
                MaterialiseError::DispatchPaused(reason.to_owned())
            } else {
                MaterialiseError::Other(DunkError::Refused(reason.to_owned()))
            };
            let advance = jobs
                .advance_with_materialiser(
                    AdvanceRequest {
                        pane,
                        trigger: "turn.end",
                        from_task: None,
                        caller: None,
                        operator_proof: None,
                        dispatch_reason: None,
                        unread: Some(0),
                        pending_delivery: false,
                    },
                    move |_| async move { Err(materialise_error) },
                )
                .await
                .unwrap();
            let committed = advance.plan["handoff"].as_object().unwrap();
            assert_eq!(advance.plan["action"], "claim");
            assert_eq!(committed["state"], "pending");
            assert_eq!(committed["attempt"], 0);
            assert!(committed["reason"].is_null());

            let handoffs = tasks.next_job_handoffs(Some(pane)).await.unwrap();
            let stored = handoffs.first().expect("committed handoff");
            assert_eq!(stored["state"], committed["state"]);
            assert_eq!(stored["attempt"], committed["attempt"]);
            if pause {
                assert_eq!(stored["reason"], reason);
                for (key, value) in committed {
                    if key != "reason" && key != "at" {
                        assert_eq!(stored.get(key), Some(value), "changed field {key}");
                    }
                }
            } else {
                for (key, value) in committed {
                    if key != "at" {
                        assert_eq!(stored.get(key), Some(value), "changed field {key}");
                    }
                }
                assert!(stored["reason"].is_null());
            }
        }
    }

    async fn configured_jobs() -> (tempfile::TempDir, NextJobs, Arc<crate::tasks::Tasks>) {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Arc::new(
            crate::tasks::Tasks::open(dir.path().join("tasks.db"))
                .await
                .unwrap(),
        );
        tasks.allocation_init().await.unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Default::default()
        });
        (dir, NextJobs::new(flock), tasks)
    }

    fn test_env(dir: &std::path::Path, switch: &str, lanes: &str) -> Env {
        Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", dir.to_str().unwrap()),
            ("DUNKINGSHEEP_TURN_END_DISPATCH", switch),
            ("DUNKINGSHEEP_DISPATCH_LANES", lanes),
        ])
    }

    /// t4697: a pane on no hand list is served on its live worker or reviewer registration
    /// plus a membership, and the owner's `exclude` key takes it out again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_serves_registered_roles_and_honours_the_owner_exclusion() {
        let (dir, jobs, _tasks) = configured_jobs().await;
        let pid = i64::from(std::process::id());
        let start = crate::proc::process_start(pid).unwrap();
        let lanes: Vec<Value> = ["%1", "%2", "%3", "%4"]
            .iter()
            .map(|pane| json!({"pane_id": pane, "pane_pid": pid, "agent_name": ""}))
            .collect();
        let mut roles = crate::roles::Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        for (pane, role) in [("%1", "worker"), ("%2", "reviewer"), ("%3", "planner")] {
            roles
                .register(pane, role, Some(pid), Some(&start), None, None, None, None)
                .unwrap();
        }
        jobs.flock.attach_roles(roles, None);
        struct Panes(Vec<Value>);
        impl crate::flock::panes::PaneInfoEnv for Panes {
            fn list_panes_grouped(&self) -> Vec<Value> {
                self.0.clone()
            }
            fn get_pane(&self, id: &str) -> Option<Value> {
                self.0.iter().find(|lane| lane["pane_id"] == id).cloned()
            }
        }
        for pane in ["%1", "%2", "%3"] {
            jobs.flock
                .lane_project_set(
                    &Panes(lanes.clone()),
                    pane,
                    crate::tasks::lane_project::ProjectScope::Project("dunkingsheep".into()),
                )
                .unwrap();
        }
        let gate = |pane: &'static str| {
            let lanes = lanes.clone();
            let env = test_env(dir.path(), "1", "");
            let jobs = &jobs;
            async move { jobs.gate_with(pane, Some(lanes), env, None).await }
        };
        assert_eq!(gate("%1").await, None, "live worker");
        assert_eq!(gate("%2").await, None, "live reviewer");
        assert_eq!(gate("%3").await.as_deref(), Some("not_allowed"), "planner");
        assert_eq!(
            gate("%4").await.as_deref(),
            Some("not_allowed"),
            "unregistered"
        );
        // The pane id now belongs to another process: the old registration grants nothing.
        let reused = vec![json!({"pane_id": "%1", "pane_pid": 1})];
        assert_eq!(
            jobs.gate_with("%1", Some(reused), test_env(dir.path(), "1", ""), None)
                .await
                .as_deref(),
            Some("not_allowed")
        );
        let config = dir.path().join("dispatch_lanes.json");
        std::fs::write(&config, r#"{"exclude": ["reviewer", "%9"]}"#).unwrap();
        assert_eq!(gate("%1").await, None);
        assert_eq!(gate("%2").await.as_deref(), Some("not_allowed"));
        std::fs::write(&config, r#"{"exclude": "%1, planner"}"#).unwrap();
        assert_eq!(gate("%1").await.as_deref(), Some("not_allowed"));
        assert_eq!(gate("%2").await, None);
        // The hand list is an explicit override: a listed pane is served although excluded.
        std::fs::write(&config, r#"{"lanes": ["%1"], "exclude": ["%1"]}"#).unwrap();
        assert_eq!(gate("%1").await, None);
        // An exclusion list that cannot be understood never widens who is served.
        std::fs::write(&config, r#"{"exclude": {"role": "reviewer"}}"#).unwrap();
        assert_eq!(gate("%1").await.as_deref(), Some("readiness_unknown"));
        std::fs::write(&config, r#"{"exclude": ["reviewer", 7]}"#).unwrap();
        assert_eq!(gate("%2").await.as_deref(), Some("readiness_unknown"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_preserves_dispatch_refusals() {
        let (dir, jobs, tasks) = configured_jobs().await;
        let lanes = vec![
            json!({"pane_id":"%1", "agent_name":"Writer"}),
            json!({"pane_id":"%2", "agent_name":"writer"}),
        ];
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes.clone()),
                test_env(dir.path(), "off", "writer"),
                None
            )
            .await
            .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes.clone()),
                test_env(dir.path(), "1", ""),
                None
            )
            .await,
            None
        );
        assert_eq!(
            jobs.gate_with(
                "%2",
                Some(lanes.clone()),
                test_env(dir.path(), "1", "WRITER"),
                None
            )
            .await,
            None
        );
        assert_eq!(
            jobs.gate_with(
                "%2",
                Some(lanes.clone()),
                test_env(dir.path(), "1", "%other"),
                None
            )
            .await
            .as_deref(),
            Some("not_allowed")
        );
        assert_eq!(
            jobs.gate_with("%1", Some(vec![]), test_env(dir.path(), "1", ""), None)
                .await
                .as_deref(),
            Some("unavailable_lane")
        );
        crate::panes::retire(
            "%1",
            "fixture",
            Some("%owner"),
            &[],
            &[],
            &dir.path().join("retired-panes.json"),
            None,
        )
        .unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes), test_env(dir.path(), "1", ""), None)
                .await
                .as_deref(),
            Some("unavailable_lane")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_fails_closed_on_lane_or_config_error() {
        let (dir, jobs, tasks) = configured_jobs().await;
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        let failing: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = crate::terminal::fakes::FakeTerminal::new();
            terminal.fail_grouped_panes =
                Some(crate::terminal::TerminalError::refused("listing failed"));
            Box::new(terminal)
        });
        assert_eq!(
            jobs.gate_with("%1", None, test_env(dir.path(), "1", "%1"), Some(failing))
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
        std::fs::write(dir.path().join("dispatch_lanes.json"), "{").unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
        std::fs::remove_file(dir.path().join("dispatch_lanes.json")).unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"other":1})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
        let no_tasks = next_jobs();
        assert_eq!(
            no_tasks
                .gate_with(
                    "%1",
                    Some(vec![json!({"pane_id":"%1"})]),
                    test_env(dir.path(), "1", "%1"),
                    None
                )
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
        let conn = rusqlite::Connection::open(dir.path().join("tasks.db")).unwrap();
        conn.execute("DROP TABLE allocation_workers", []).unwrap();
        drop(conn);
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_fails_closed_on_retirement_read_error() {
        let (dir, jobs, _) = configured_jobs().await;
        let path = dir.path().join("retired-panes.json");
        let lanes = vec![json!({"pane_id":"%1"})];
        let env = || test_env(dir.path(), "1", "%1");
        // The existing store treats a missing file as empty.
        assert_eq!(
            jobs.gate_with("%1", Some(lanes.clone()), env(), None).await,
            None
        );
        for bad in ["{", r#"{"%1":{}}"#] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(
                jobs.gate_with("%1", Some(lanes.clone()), env(), None)
                    .await
                    .as_deref(),
                Some("readiness_unknown")
            );
        }
        std::fs::write(&path, r#"{"%2":{}}"#).unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes.clone()), env(), None).await,
            None
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes), env(), None)
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_registration_is_dispatch_opt_in_but_not_kill_switch_override() {
        let (dir, jobs, tasks) = configured_jobs().await;
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        let lane = vec![json!({"pane_id":"%1"})];
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lane.clone()),
                test_env(dir.path(), "0", ""),
                None
            )
            .await
            .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            jobs.gate_with("%1", Some(lane), test_env(dir.path(), "1", ""), None)
                .await,
            None
        );
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(false),
            )
            .await
            .unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", ""),
                None
            )
            .await
            .as_deref(),
            Some("not_allowed")
        );
        // Python (c): with the kill switch off, a registered pane whose registration is disabled
        // still reads "disabled"; the switch is checked before the registration.
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "0", ""),
                None
            )
            .await
            .as_deref(),
            Some("disabled")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_uses_given_snapshot_and_real_allowlist_file() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, _) = configured_jobs().await;
        std::fs::write(
            dir.path().join("dispatch_lanes.json"),
            r#"{"lanes":["Writer, other"]}"#,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let factory: LoopTerminalFactory = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(crate::terminal::fakes::FakeTerminal::new())
        });
        let lanes = vec![json!({"pane_id":"%1","agent_name":"writer"})];
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes),
                test_env(dir.path(), "1", "%other"),
                Some(factory)
            )
            .await,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for content in ["false", "0", r#"{"lanes":false}"#, r#"{"lanes":{}}"#] {
            std::fs::write(dir.path().join("dispatch_lanes.json"), content).unwrap();
            assert_eq!(
                jobs.gate_with(
                    "%1",
                    Some(vec![json!({"pane_id":"%1"})]),
                    test_env(dir.path(), "1", "%1"),
                    None
                )
                .await,
                None
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_resolves_real_terminal_on_blocking_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, _) = configured_jobs().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let factory: LoopTerminalFactory = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(crate::terminal::fakes::FakeTerminal::new())
        });
        assert_eq!(
            jobs.gate_with(
                "w1:p1",
                None,
                test_env(dir.path(), "1", "w1:p1"),
                Some(factory)
            )
            .await,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_production_entry_refuses_missing_store() {
        let jobs = next_jobs();
        let reason = jobs.gate("%fixture", Some(vec![])).await;
        assert!(matches!(
            reason.as_deref(),
            Some("disabled" | "readiness_unknown")
        ));
    }
}
