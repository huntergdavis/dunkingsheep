//! `turn_start` / `turn_end` / `require_turn_caller` (T36, slice 1), ported from
//! `dunk_core.Flock`: the turn state machine and its authentication. A turn holds sends until
//! it ends, carries a TTL so a crashed agent cannot hold mail forever, and is bound to the
//! process that started it (`turn_id` + pid/start), so a forged pane label or a replayed token
//! cannot end it or supply idle evidence.
//!
//! The edges the Python reaches through `self` are a trait here ([`TurnHooks`]): the metrics
//! recorder, `next_jobs.observe_turn`, and the turn-end dispatch. `NoHooks` is the default.
//!
//! Slice 2: `turn_active` and `turn_expire` (the crashed-turn path).
//!
//! Slice 4 (t1412): `_dispatch_on_turn_end` / `_dispatch_claimed_turn_end`, the turn-end nudge:
//! every guardrail in the Python's order, the record it returns, and the cooldown bookkeeping.
//! The planning functions of `dunk_dispatch` (`turn_end_plan`, `lane_plan`, `turn_end_text`)
//! are T54 and reach this code through [`TurnDispatchEnv`].
//!
//! NOT PORTED yet (next T36 slices): the loops (`_task_reaper_loop`,
//! `_task_push_loop`, `_stuck_observer_loop`, the next-job wake) in `flock/loops.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use serde_json::{json, Map, Value};

use super::Flock;
use crate::activity::Turn;
use crate::error::DunkError;
use crate::proc::{caller_pane_auth_failure, Caller};

/// One pane's turn. `ended_at` is kept (not popped): its end time is the only evidence of
/// IDLENESS the pi extension will ever give us.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnState {
    pub started_at: f64,
    pub expires_at: f64,
    pub ended_at: Option<f64>,
    pub ttl_s: f64,
    pub turn_id: String,
    pub source: Option<String>,
    pub caller: Option<Value>,
    pub handoff_id: Option<String>,
}

/// The turn table (`self.turns`).
pub type Turns = BTreeMap<String, TurnState>;

/// What `turn_start` / `turn_end` reach outside the turn table.
pub trait TurnHooks {
    /// `self.metrics.record("turn.start" | "turn.end", ...)`: never raises.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        kind: &str,
        pane_id: &str,
        source: Option<&str>,
        reason: Option<&str>,
        outcome: &str,
        at: f64,
        duration_ms: Option<i64>,
    );
    /// `self.next_jobs.observe_turn(pane_id, state)`.
    fn observe_turn(&self, pane_id: &str, state: &TurnState);
    /// `self._dispatch_on_turn_end(pane_id)`: the record returned under `dispatch`.
    fn dispatch_on_turn_end(&self, pane_id: &str) -> Value;
}

/// Hooks that do nothing: dispatch is a skip.
pub struct NoHooks;

impl TurnHooks for NoHooks {
    fn record(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: &str,
        _: f64,
        _: Option<i64>,
    ) {
    }
    fn observe_turn(&self, _: &str, _: &TurnState) {}
    fn dispatch_on_turn_end(&self, _: &str) -> Value {
        skip("turn.end", "not_wired")
    }
}

fn skip(trigger: &str, reason: &str) -> Value {
    json!({ "trigger": trigger, "action": "skip", "reason": reason, "delivered": false })
}

/// `DUNKINGSHEEP_TURN_TTL_S`, default 1800 (read per call here; the Python reads it at import).
pub fn turn_ttl_s() -> f64 {
    std::env::var("DUNKINGSHEEP_TURN_TTL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800.0)
}

/// `secrets.token_hex(16)`.
fn token_hex16() -> Result<String, DunkError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `nj-[0-9a-f]{32}`.
fn valid_handoff_id(id: &str) -> bool {
    id.strip_prefix("nj-").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

impl Flock {
    /// `require_turn_caller`: authenticate a socket turn event. A request's pane labels alone
    /// prove nothing: the event must target the caller's own pane, and the caller's process
    /// must descend from that pane's root (`pane_pid`, from the live pane record).
    pub fn require_turn_caller(
        &self,
        pane_id: Option<&str>,
        self_pane_id: Option<&str>,
        pane_pid: Option<i64>,
        caller: Option<&Caller>,
    ) -> Result<(), DunkError> {
        let own = self_pane_id.filter(|p| !p.is_empty());
        if own.is_none() || pane_id.is_some_and(|p| !p.is_empty() && Some(p) != own) {
            eprintln!("turn authentication refused reason=target_mismatch pane={pane_id:?} self_pane={self_pane_id:?}");
            return Err(DunkError::Refused(
                "turn event must target the caller's own pane".into(),
            ));
        }
        if let Some(reason) = caller_pane_auth_failure(caller, pane_pid) {
            eprintln!("turn authentication refused reason={reason} pane={self_pane_id:?} pane_pid={pane_pid:?}");
            return Err(DunkError::Refused(
                "turn event caller is not authenticated to this pane".into(),
            ));
        }
        Ok(())
    }

    /// `turn_start`: declare a busy turn for `pane_id` (falling back to the caller's pane).
    pub fn turn_start(
        &self,
        pane_id: Option<&str>,
        ttl_s: Option<f64>,
        source: Option<&str>,
        self_pane_id: Option<&str>,
        handoff_id: Option<&str>,
        hooks: &dyn TurnHooks,
    ) -> Result<Value, DunkError> {
        let pane_id = pane_id
            .filter(|p| !p.is_empty())
            .or(self_pane_id.filter(|p| !p.is_empty()))
            .ok_or_else(|| DunkError::Refused("turn start requires a pane".into()))?;
        if handoff_id.is_some_and(|id| !valid_handoff_id(id)) {
            return Err(DunkError::Refused("invalid next-job handoff id".into()));
        }
        let ttl = ttl_s.filter(|t| *t != 0.0).unwrap_or_else(turn_ttl_s);
        if ttl <= 0.0 {
            return Err(DunkError::Refused("ttl_s must be positive".into()));
        }
        let now = crate::clock::now_s();
        let state = TurnState {
            started_at: now,
            expires_at: now + ttl,
            ended_at: None,
            ttl_s: ttl,
            turn_id: token_hex16()?,
            source: source.map(String::from),
            caller: self.current_caller(),
            handoff_id: handoff_id.map(String::from),
        };
        self.turn_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pane_id.to_string(), state.clone());
        self.set_turn(
            pane_id,
            Turn {
                started_at: Some(now),
                expires_at: Some(now + ttl),
                ended_at: None,
            },
        );
        hooks.record("turn.start", pane_id, source, None, "ok", now, None);
        hooks.observe_turn(pane_id, &state);
        Ok(json!({
            "pane_id": pane_id, "started_at": now, "expires_at": now + ttl, "ttl_s": ttl,
            "source": source, "turn_id": state.turn_id,
        }))
    }

    /// `turn_end`. An invalid end (no turn, already ended, expired, or - when `authenticated` -
    /// a wrong `turn_id` or a foreign process) changes nothing and dispatches nothing.
    pub fn turn_end(
        &self,
        pane_id: Option<&str>,
        reason: &str,
        self_pane_id: Option<&str>,
        turn_id: Option<&str>,
        authenticated: bool,
        hooks: &dyn TurnHooks,
    ) -> Result<Value, DunkError> {
        let pane_id = pane_id
            .filter(|p| !p.is_empty())
            .or(self_pane_id.filter(|p| !p.is_empty()))
            .ok_or_else(|| DunkError::Refused("turn end requires a pane".into()))?;
        let now = crate::clock::now_s();
        let state = {
            let mut turns = self
                .turn_states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let invalid: Option<&str> = match turns.get(pane_id) {
                None => Some("no_active_turn"),
                Some(state) if state.ended_at.is_some() => Some("already_ended"),
                Some(state) if state.expires_at <= now => Some("expired_turn"),
                Some(state) if authenticated => {
                    let caller = self.current_caller().unwrap_or(Value::Null);
                    let starter = state.caller.clone().unwrap_or(Value::Null);
                    if turn_id.is_none_or(str::is_empty) || turn_id != Some(state.turn_id.as_str())
                    {
                        Some("turn_mismatch")
                    } else if caller.get("pid").is_none_or(|p| p.is_null() || p == 0)
                        || ["pid", "start"]
                            .iter()
                            .any(|k| caller.get(k) != starter.get(k))
                    {
                        Some("foreign_turn")
                    } else {
                        None
                    }
                }
                Some(_) => None,
            };
            if let Some(invalid) = invalid {
                return Ok(json!({
                    "pane_id": pane_id, "ended": false, "reason": invalid,
                    "dispatch": skip("turn.end", invalid),
                }));
            }
            let state = turns.get_mut(pane_id).expect("checked above");
            state.ended_at = Some(now);
            state.expires_at = 0.0;
            state.clone()
        };
        self.set_turn(
            pane_id,
            Turn {
                started_at: Some(state.started_at),
                expires_at: Some(0.0),
                ended_at: Some(now),
            },
        );
        let duration = ((now - state.started_at) * 1000.0) as i64;
        hooks.record(
            "turn.end",
            pane_id,
            None,
            Some(reason),
            "ok",
            now,
            Some(duration),
        );
        // THE TRIGGER: the turn just ended and the lane is idle.
        let dispatch = if authenticated {
            hooks.dispatch_on_turn_end(pane_id)
        } else {
            skip("turn.end", "unauthenticated")
        };
        Ok(json!({ "pane_id": pane_id, "ended": true, "reason": reason, "dispatch": dispatch }))
    }
    /// `turn_active`: true only for an unexpired declared turn. Expiry is the safety.
    pub fn turn_active(&self, pane_id: &str) -> bool {
        self.turn_states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(pane_id)
            .is_some_and(|state| state.expires_at > crate::clock::now_s())
    }

    /// `turn_expire`: release expired turns and record why. This is the crashed-turn path: no
    /// `turn_end` arrived, so the TTL fires, mail is released, and the event says so. The caller
    /// pid's liveness is RECORDED, never used to extend the hold: a live pid does not prove the
    /// turn is still running after the declared deadline.
    ///
    /// Like the Python, an ENDED turn (`expires_at` zeroed) is swept here too, after it has served
    /// as idleness evidence. Both turn tables lose the pane.
    pub fn turn_expire(&self, now: Option<f64>, hooks: &dyn TurnHooks) -> Vec<Value> {
        let now = now
            .filter(|n| *n != 0.0)
            .unwrap_or_else(crate::clock::now_s);
        let released: Vec<(String, TurnState)> = {
            let mut states = self
                .turn_states
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let expired: Vec<String> = states
                .iter()
                .filter(|(_, state)| state.expires_at <= now)
                .map(|(pane, _)| pane.clone())
                .collect();
            expired
                .into_iter()
                .filter_map(|pane| states.remove(&pane).map(|state| (pane, state)))
                .collect()
        };
        for (pane_id, _) in &released {
            self.turns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(pane_id);
        }
        for (pane_id, state) in &released {
            let caller = state.caller.as_ref();
            let pid = caller
                .and_then(|c| c.get("pid"))
                .and_then(Value::as_i64)
                .filter(|p| *p != 0);
            let start = caller.and_then(|c| c.get("start")).and_then(Value::as_str);
            let alive = pid.is_some_and(|pid| crate::proc::process_alive(pid, start));
            eprintln!(
                "turn expired on {pane_id} after {:.0}s (ttl {:.0}s); releasing hold (caller pid {pid:?} alive={alive})",
                now - state.started_at,
                state.ttl_s
            );
            hooks.record(
                "turn.expired",
                pane_id,
                None,
                Some("ttl"),
                "released",
                now,
                Some(((now - state.started_at) * 1000.0) as i64),
            );
        }
        released
            .iter()
            .map(|(pane_id, state)| json!({ "pane_id": pane_id, "ttl_s": state.ttl_s, "reason": "ttl" }))
            .collect()
    }
}

/// Per-flock turn-end dispatch state: when each pane (and each pane+task) was last prompted,
/// and the panes a dispatch is currently running for (`turn_end_nudges`,
/// `turn_end_task_nudges`, `turn_end_dispatching`).
#[derive(Debug, Default)]
pub struct TurnDispatchState {
    pub nudges: BTreeMap<String, f64>,
    pub task_nudges: BTreeMap<(String, String), f64>,
    pub dispatching: BTreeSet<String>,
}

/// What `_dispatch_claimed_turn_end` asks of the shared lane gate (`dunk_dispatch.lane_plan`).
#[derive(Debug, Clone, Default)]
pub struct Readiness {
    /// The pane is on the dispatch allowlist.
    pub allowed: bool,
    /// The pane is a live, non-retired lane with a known status.
    pub candidate: bool,
    pub ready: bool,
    /// First true of has_mail / blocked / dead / working, when not ready.
    pub not_ready_reason: Option<String>,
}

/// Everything the turn-end nudge reaches outside the Flock's own state. The planning functions
/// belong to `dunk_dispatch` (T54); until then they are the caller's.
pub trait TurnDispatchEnv {
    /// `TURN_END_DISPATCH`: the trigger is switched on.
    fn dispatch_enabled(&self) -> bool;
    /// `inbox_count(None, self_pane_id=pane).unread`.
    fn unread_count(&self, pane_id: &str) -> Result<Option<i64>, String>;
    /// The pane's live claims as decorated task records (`task_read` per lease).
    fn claimed_tasks(&self, pane_id: &str) -> Result<Vec<Value>, String>;
    /// `dunk_dispatch.turn_end_plan(...)`: `{action: "nudge"|"skip", reason, item}`.
    fn turn_end_plan(
        &self,
        pane_id: &str,
        tasks: &[Value],
        unread: Option<i64>,
        last_nudge_at: Option<f64>,
        task_nudges: &BTreeMap<String, f64>,
        now: f64,
    ) -> Value;
    /// Ids of this pane's messages still `queued` in the outbox.
    fn pending_outbox(&self, pane_id: &str) -> Vec<String>;
    /// The allowlist + the shared lane readiness gate; an error fails closed.
    fn readiness(&self, pane_id: &str, unread: Option<i64>) -> Result<Readiness, String>;
    /// `dunk_dispatch.turn_end_text(task)`.
    fn turn_end_text(&self, task: &Value) -> String;
    /// `send_now(pane, text, wait_s=TURN_END_WAIT_S, source="turn_end")`: `{outcome, message_id,
    /// message}`; an Err is a refusal.
    fn send_now(&self, pane_id: &str, text: &str) -> Result<Value, String>;
    /// `self.metrics.record("turn.end.dispatch", ...)`.
    fn record(&self, pane_id: &str, outcome: &str, reason: Option<&str>, bytes: Option<usize>);
    /// `self.next_jobs.advance(pane_id, "turn.end")`: the next-job plan.
    fn advance_next_job(&self, pane_id: &str) -> Value;
    /// Any next-job handoff of this pane whose task is not yet done or cancelled.
    fn open_handoffs(&self, pane_id: &str) -> bool;
}

impl Flock {
    /// `_dispatch_on_turn_end`: advance the next-job plan; a claim, or an unfinished handoff,
    /// ends here with `delivered: false`; otherwise reserve the pane (concurrent turn ends cannot
    /// enqueue twice) and run the claimed-work nudge.
    pub fn dispatch_on_turn_end(&self, pane_id: &str, env: &dyn TurnDispatchEnv) -> Value {
        let plan = env.advance_next_job(pane_id);
        self.dispatch_after_plan(pane_id, plan, env)
    }

    fn dispatch_after_plan(&self, pane_id: &str, plan: Value, env: &dyn TurnDispatchEnv) -> Value {
        let with_delivered = |mut plan: Value| {
            if let Some(object) = plan.as_object_mut() {
                object.insert("delivered".into(), json!(false));
            }
            plan
        };
        let reason = plan.get("reason").and_then(Value::as_str).unwrap_or("");
        if [
            "disabled",
            "not_allowed",
            "unavailable_lane",
            "readiness_unknown",
        ]
        .contains(&reason)
        {
            return with_delivered(plan);
        }
        if plan.get("action").and_then(Value::as_str) == Some("claim") || env.open_handoffs(pane_id)
        {
            return with_delivered(plan);
        }
        {
            let mut state = self.turn_dispatch.lock().unwrap_or_else(|p| p.into_inner());
            if state.dispatching.contains(pane_id) {
                return json!({
                    "trigger": "turn.end", "pane_id": pane_id, "action": "skip",
                    "reason": "dispatch_in_progress", "delivered": false,
                });
            }
            state.dispatching.insert(pane_id.to_string());
        }
        // The Python runs the whole claimed dispatch inside try/finally: the reservation is
        // released even when a hook raises, so one failing dispatch cannot wedge a lane. A drop
        // guard does the same here, panic included.
        let _reservation = Reservation {
            flock: self,
            pane_id,
        };
        self.dispatch_claimed_turn_end(pane_id, crate::clock::now_s(), env)
    }

    /// `_dispatch_claimed_turn_end`: prompt a lane that ended its turn still holding work.
    /// Every guardrail is a code the caller and the log can read, in the Python's order:
    /// disabled, then the plan's own skips (unread_unknown, unread_mail, no_open_work, cooldown,
    /// task_cooldown, ...), held_delivery_pending, not_allowed, unavailable_lane, the lane gate
    /// (has_mail / blocked / dead / working), readiness_unknown. A send that is HELD (the box
    /// would not clear, a dead session, an unconfirmed submit) is recorded as held and is NOT
    /// counted as delivered and does NOT start the cooldown clock.
    pub fn dispatch_claimed_turn_end(
        &self,
        pane_id: &str,
        now: f64,
        env: &dyn TurnDispatchEnv,
    ) -> Value {
        let mut record = Map::new();
        record.insert("trigger".into(), json!("turn.end"));
        record.insert("pane_id".into(), json!(pane_id));
        record.insert("action".into(), json!("skip"));
        record.insert("reason".into(), Value::Null);
        record.insert("delivered".into(), json!(false));
        if !env.dispatch_enabled() {
            record.insert("reason".into(), json!("disabled"));
            return Value::Object(record);
        }
        let last_nudge = self
            .turn_dispatch
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .nudges
            .get(pane_id)
            .copied();
        let unread = match env.unread_count(pane_id) {
            Ok(unread) => unread,
            Err(error) => {
                record.insert(
                    "error".into(),
                    json!(format!("inbox count failed: {error}")),
                );
                None
            }
        };
        let tasks = match env.claimed_tasks(pane_id) {
            Ok(tasks) => tasks,
            Err(error) => {
                record.insert("error".into(), json!(format!("task list failed: {error}")));
                Vec::new()
            }
        };
        let task_nudges: BTreeMap<String, f64> = {
            let state = self.turn_dispatch.lock().unwrap_or_else(|p| p.into_inner());
            tasks
                .iter()
                .filter_map(|task| {
                    let id = task.get("id").map(value_text)?;
                    state
                        .task_nudges
                        .get(&(pane_id.to_string(), id.clone()))
                        .map(|at| (id, *at))
                })
                .collect()
        };
        let plan = env.turn_end_plan(pane_id, &tasks, unread, last_nudge, &task_nudges, now);
        if plan.get("action").and_then(Value::as_str) != Some("nudge") {
            let reason = plan
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            record.insert("reason".into(), json!(reason));
            env.record(pane_id, "held", Some(&reason), None);
            eprintln!("turn.end {pane_id}: no nudge (trigger=turn.end reason={reason})");
            return Value::Object(record);
        }
        // One prompt at a time per pane: stacking a second behind one still waiting for the
        // box is how a lane gets three contradictory prompts after one turn.
        let pending = env.pending_outbox(pane_id);
        if let Some(first) = pending.first() {
            record.insert("reason".into(), json!("held_delivery_pending"));
            env.record(pane_id, "held", Some("held_delivery_pending"), None);
            eprintln!("turn.end {pane_id}: no nudge (trigger=turn.end reason=held_delivery_pending message={first})");
            return Value::Object(record);
        }
        // Claiming a task is necessary but does not opt a lane into automatic prompts: the same
        // allowlist and readiness gate as the timer dispatcher.
        match env.readiness(pane_id, unread) {
            Ok(gate) => {
                if !gate.allowed {
                    record.insert("reason".into(), json!("not_allowed"));
                } else if !gate.candidate {
                    record.insert("reason".into(), json!("unavailable_lane"));
                } else if !gate.ready {
                    match gate.not_ready_reason {
                        Some(reason) => {
                            record.insert("reason".into(), json!(reason));
                        }
                        // Python's `next(...)` over the four names raises StopIteration inside
                        // its try, which lands in the fail-closed branch: readiness_unknown.
                        None => {
                            record.insert(
                                "error".into(),
                                json!("lane gate not ready with no named reason"),
                            );
                            record.insert("reason".into(), json!("readiness_unknown"));
                        }
                    }
                }
            }
            Err(error) => {
                record.insert("reason".into(), json!("readiness_unknown"));
                record.insert("error".into(), json!(error));
            }
        }
        if let Some(reason) = record
            .get("reason")
            .and_then(Value::as_str)
            .map(String::from)
        {
            env.record(pane_id, "held", Some(&reason), None);
            eprintln!("turn.end {pane_id}: no nudge (trigger=turn.end reason={reason})");
            return Value::Object(record);
        }
        let item = plan.get("item").cloned().unwrap_or(Value::Null);
        // `str(task.get("id"))`: a missing id is the text "None", like the Python.
        let task_id = item
            .get("id")
            .map(value_text)
            .unwrap_or_else(|| "None".to_string());
        record.insert("action".into(), json!("nudge"));
        record.insert("task_id".into(), json!(task_id));
        record.insert("item".into(), json!(format!("board:{task_id}")));
        let text = env.turn_end_text(&item);
        let bytes = text.len();
        let message = match env.send_now(pane_id, &text) {
            Ok(message) => message,
            Err(error) => {
                // Never break turn_end: a refusal is recorded, not raised, and is not delivered.
                record.insert("delivered".into(), json!(false));
                record.insert("outcome".into(), json!("refused"));
                record.insert("error".into(), json!(error));
                env.record(pane_id, "failed", Some("refused"), None);
                eprintln!("turn.end {pane_id}: trigger=turn.end nudge REFUSED for task {task_id} ({error}); not counted as delivered");
                return Value::Object(record);
            }
        };
        let outcome = message
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let delivered = outcome == "ok";
        record.insert(
            "message_id".into(),
            message.get("message_id").cloned().unwrap_or(Value::Null),
        );
        record.insert("delivered".into(), json!(delivered));
        record.insert("outcome".into(), json!(outcome));
        record.insert(
            "message".into(),
            message.get("message").cloned().unwrap_or(Value::Null),
        );
        if delivered {
            {
                let mut state = self.turn_dispatch.lock().unwrap_or_else(|p| p.into_inner());
                state.nudges.insert(pane_id.to_string(), now);
                state
                    .task_nudges
                    .insert((pane_id.to_string(), task_id.clone()), now);
            }
            env.record(pane_id, "ok", None, Some(bytes));
            eprintln!("turn.end {pane_id}: trigger=turn.end nudge DELIVERED task={task_id}");
        } else {
            // HELD, NOT DELIVERED: the message stays in the outbox and the worker retries when
            // the box clears. Saying "delivered" here is the claim the task forbids.
            env.record(pane_id, "held", Some(&outcome), Some(bytes));
            eprintln!("turn.end {pane_id}: trigger=turn.end nudge NOT delivered for task {task_id} (outcome={outcome}); held, not counted as delivered");
        }
        Value::Object(record)
    }
}

/// Python's `str(value)` for an id that is a string or a number.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

/// Releases a pane's turn-end dispatch reservation on drop (`finally: discard`).
struct Reservation<'a> {
    flock: &'a Flock,
    pane_id: &'a str,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.flock
            .turn_dispatch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .dispatching
            .remove(self.pane_id);
    }
}
