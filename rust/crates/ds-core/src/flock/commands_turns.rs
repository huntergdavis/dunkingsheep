//! `turn_start` / `turn_end` / `require_turn_caller` (T36, slice 1), ported from
//! `dunk_core.Flock`: the turn state machine and its authentication. A turn holds sends until
//! it ends, carries a TTL so a crashed agent cannot hold mail forever, and is bound to the
//! process that started it (`turn_id` + pid/start), so a forged pane label or a replayed token
//! cannot end it or supply idle evidence.
//!
//! The edges the Python reaches through `self` are a trait here ([`TurnHooks`]): the metrics
//! recorder, `next_jobs.observe_turn`, and the turn-end dispatch. `NoHooks` is the default.
//!
//! NOT PORTED yet (next T36 slices): `_dispatch_on_turn_end` / `_dispatch_claimed_turn_end` (the
//! guardrails and the send), `turn_expire`, and the loops (`_task_reaper_loop`,
//! `_task_push_loop`, `_stuck_observer_loop`, the next-job wake) in `flock/loops.rs`.

use std::collections::BTreeMap;
use std::io::Read;

use serde_json::{json, Value};

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
            let mut turns = self.turn_states.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
}
