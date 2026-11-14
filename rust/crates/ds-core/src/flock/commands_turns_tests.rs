//! T36 slice 1 tests: the turn state machine and its authentication, ported by name from the
//! authentication half of `tests/test_dunk_turn_end_dispatch.py` (the dispatch guardrails are the
//! next slice).

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::commands_turns::{NoHooks, Readiness, TurnDispatchEnv, TurnHooks, TurnState};
use super::Flock;
use crate::proc::Caller;

const PANE: &str = "w1:p1";

fn flock() -> crate::testsupport::Scratch<Flock> {
    let dir = crate::testsupport::dir("turns");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    crate::testsupport::Scratch::new(flock, dir)
}

fn as_caller(pid: i64, start: &str) -> Value {
    json!({ "pid": pid, "start": start })
}

/// Records every call, and says what the dispatch edge was asked.
#[derive(Default)]
struct Spy {
    calls: Mutex<Vec<String>>,
}

impl TurnHooks for Spy {
    fn record(
        &self,
        kind: &str,
        pane: &str,
        _: Option<&str>,
        reason: Option<&str>,
        outcome: &str,
        _: f64,
        duration: Option<i64>,
    ) {
        self.calls.lock().unwrap().push(format!(
            "record {kind} {pane} {reason:?} {outcome} {}",
            duration.is_some()
        ));
    }
    fn observe_turn(&self, pane: &str, state: &TurnState) -> Result<(), crate::error::DunkError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("observe {pane} {}", state.turn_id.len()));
        Ok(())
    }
    fn dispatch_on_turn_end(&self, pane: &str) -> Result<Value, crate::error::DunkError> {
        self.calls.lock().unwrap().push(format!("dispatch {pane}"));
        Ok(json!({ "trigger": "turn.end", "action": "claim", "delivered": true }))
    }
}

fn started(flock: &Flock) -> String {
    flock.set_caller(Some(as_caller(41, "100")));
    flock
        .turn_start(None, None, Some("test"), Some(PANE), None, &NoHooks)
        .unwrap()["turn_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn ended_at(flock: &Flock) -> Option<f64> {
    flock
        .turn_states
        .lock()
        .unwrap()
        .get(PANE)
        .unwrap()
        .ended_at
}

#[test]
fn turn_start_records_the_turn_and_tells_the_hooks() {
    let (flock, spy) = (flock(), Spy::default());
    flock.set_caller(Some(as_caller(41, "100")));
    let out = flock
        .turn_start(None, Some(60.0), Some("pi"), Some(PANE), None, &spy)
        .unwrap();
    assert_eq!(out["pane_id"], PANE);
    assert_eq!(out["ttl_s"], 60.0);
    assert_eq!(out["turn_id"].as_str().unwrap().len(), 32);
    let state = flock
        .turn_states
        .lock()
        .unwrap()
        .get(PANE)
        .cloned()
        .unwrap();
    assert_eq!(state.caller, Some(as_caller(41, "100")));
    assert_eq!(
        spy.calls.lock().unwrap().len(),
        2,
        "metrics.record + next_jobs.observe_turn"
    );
    assert!(flock
        .turn_start(None, None, None, None, None, &NoHooks)
        .unwrap_err()
        .to_string()
        .contains("turn start requires a pane"));
    assert!(flock
        .turn_start(None, Some(-1.0), None, Some(PANE), None, &NoHooks)
        .unwrap_err()
        .to_string()
        .contains("ttl_s must be positive"));
    assert!(flock
        .turn_start(None, None, None, Some(PANE), Some("nj-nope"), &NoHooks)
        .unwrap_err()
        .to_string()
        .contains("invalid next-job handoff id"));
    let good = format!("nj-{}", "a".repeat(32));
    assert!(flock
        .turn_start(None, None, None, Some(PANE), Some(&good), &NoHooks)
        .is_ok());
}

#[test]
fn a_genuine_end_ends_the_turn_and_asks_for_the_dispatch() {
    let (flock, spy) = (flock(), Spy::default());
    let token = started(&flock);
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
        .unwrap();
    assert_eq!(out["ended"], true);
    assert_eq!(out["dispatch"]["action"], "claim");
    assert!(ended_at(&flock).is_some());
    assert_eq!(
        flock
            .turn_states
            .lock()
            .unwrap()
            .get(PANE)
            .unwrap()
            .expires_at,
        0.0
    );
    assert!(spy
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c == "dispatch w1:p1"));
}

#[test]
fn an_unauthenticated_end_never_dispatches() {
    let (flock, spy) = (flock(), Spy::default());
    started(&flock);
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), None, false, &spy)
        .unwrap();
    assert_eq!(
        out["ended"], true,
        "the in-process caller may end its own turn"
    );
    assert_eq!(out["dispatch"]["reason"], "unauthenticated");
    assert!(!spy
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.starts_with("dispatch")));
}

#[test]
fn foreign_explicit_target_cannot_end_or_dispatch() {
    let flock = flock();
    let me = Caller {
        pid: Some(41),
        start: Some("100".into()),
        ..Default::default()
    };
    let refused = flock
        .require_turn_caller(Some(PANE), Some("w1:p2"), Some(41), Some(&me))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("turn event must target the caller's own pane"),
        "{refused}"
    );
    assert!(
        flock
            .require_turn_caller(None, None, Some(41), Some(&me))
            .is_err(),
        "no self pane at all"
    );
}

#[test]
fn forging_both_pane_labels_does_not_authenticate_a_foreign_process() {
    let flock = flock();
    let stranger = Caller {
        pid: Some(99_999_999),
        start: Some("1".into()),
        ..Default::default()
    };
    let refused = flock
        .require_turn_caller(Some(PANE), Some(PANE), None, Some(&stranger))
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("turn event caller is not authenticated to this pane"),
        "{refused}"
    );
    let anonymous = flock
        .require_turn_caller(Some(PANE), Some(PANE), Some(41), None)
        .unwrap_err()
        .to_string();
    assert!(anonymous.contains("not authenticated"), "{anonymous}");
}

#[test]
fn missing_turn_never_dispatches_claimed_work() {
    let (flock, spy) = (flock(), Spy::default());
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), None, true, &spy)
        .unwrap();
    assert_eq!(out["ended"], false);
    assert_eq!(out["dispatch"]["reason"], "no_active_turn");
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[test]
fn missing_token_cannot_end_a_valid_turn() {
    let (flock, spy) = (flock(), Spy::default());
    started(&flock);
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), None, true, &spy)
        .unwrap();
    assert_eq!(out["ended"], false);
    assert_eq!(out["dispatch"]["reason"], "turn_mismatch");
    assert!(ended_at(&flock).is_none());
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[test]
fn ended_token_cannot_be_replayed() {
    let (flock, spy) = (flock(), Spy::default());
    let token = started(&flock);
    assert_eq!(
        flock
            .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
            .unwrap()["ended"],
        true
    );
    let again = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
        .unwrap();
    assert_eq!(again["ended"], false);
    assert_eq!(again["dispatch"]["reason"], "already_ended");
    let dispatches = spy
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("dispatch"))
        .count();
    assert_eq!(dispatches, 1);
}

#[test]
fn old_token_cannot_end_a_new_turn_from_the_same_process() {
    let (flock, spy) = (flock(), Spy::default());
    let first = started(&flock);
    let second = started(&flock);
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&first), true, &spy)
        .unwrap();
    assert_eq!(out["ended"], false);
    assert_eq!(out["dispatch"]["reason"], "turn_mismatch");
    assert_eq!(
        flock.turn_states.lock().unwrap().get(PANE).unwrap().turn_id,
        second
    );
    assert!(ended_at(&flock).is_none());
}

#[test]
fn expired_turn_cannot_supply_idle_evidence_or_dispatch() {
    let (flock, spy) = (flock(), Spy::default());
    let token = started(&flock);
    flock
        .turn_states
        .lock()
        .unwrap()
        .get_mut(PANE)
        .unwrap()
        .expires_at = 1.0;
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
        .unwrap();
    assert_eq!(out["ended"], false);
    assert_eq!(out["dispatch"]["reason"], "expired_turn");
    assert!(ended_at(&flock).is_none());
    assert!(spy.calls.lock().unwrap().is_empty());
}

#[test]
fn a_foreign_process_cannot_end_a_turn_it_did_not_start() {
    let (flock, spy) = (flock(), Spy::default());
    let token = started(&flock);
    flock.set_caller(Some(as_caller(77, "999")));
    let out = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
        .unwrap();
    assert_eq!(out["ended"], false);
    assert_eq!(out["dispatch"]["reason"], "foreign_turn");
    flock.set_caller(None);
    let none = flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &spy)
        .unwrap();
    assert_eq!(
        none["dispatch"]["reason"], "foreign_turn",
        "no caller pid is foreign too"
    );
    assert!(ended_at(&flock).is_none());
}

#[test]
fn the_activity_view_follows_the_turn() {
    // `lane_activity` reads the activity table; a started turn is busy, an ended one is not.
    let flock = flock();
    let token = started(&flock);
    let live = flock.turns.lock().unwrap().get(PANE).cloned().unwrap();
    assert!(live.started_at.is_some() && live.ended_at.is_none() && live.expires_at.unwrap() > 0.0);
    flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &NoHooks)
        .unwrap();
    let done = flock.turns.lock().unwrap().get(PANE).cloned().unwrap();
    assert!(done.ended_at.is_some());
    assert_eq!(done.expires_at, Some(0.0));
}

#[test]
fn a_declared_turn_holds_until_turn_end() {
    // test_a_declared_turn_holds_sends_until_turn_end (the hold decision; the send is T33).
    let flock = flock();
    let token = started(&flock);
    assert!(flock.turn_active(PANE));
    flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &NoHooks)
        .unwrap();
    assert!(!flock.turn_active(PANE), "an ended turn no longer holds");
    assert!(!flock.turn_active("w9:p9"), "no turn, no hold");
}

#[test]
fn a_crashed_turn_releases_after_ttl_and_logs_the_reason() {
    let (flock, spy) = (flock(), Spy::default());
    flock.set_caller(Some(as_caller(41, "100")));
    flock
        .turn_start(
            None,
            Some(0.05),
            Some("test-crash"),
            Some(PANE),
            None,
            &NoHooks,
        )
        .unwrap();
    assert!(flock.turn_active(PANE));
    std::thread::sleep(std::time::Duration::from_millis(80));
    // No turn_end arrives: the TTL must release the hold, not hold forever.
    let released = flock.turn_expire(None, &spy);
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["reason"], "ttl");
    assert_eq!(released[0]["pane_id"], PANE);
    assert!(!flock.turn_active(PANE));
    assert!(
        flock.turns.lock().unwrap().get(PANE).is_none(),
        "the activity view loses it too"
    );
    let calls = spy.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("record turn.expired w1:p1")
                && c.contains("\"ttl\"")
                && c.contains("released")),
        "{calls:?}"
    );
    assert!(
        flock.turn_expire(None, &spy).is_empty(),
        "nothing left to release"
    );
}

#[test]
fn an_unexpired_turn_survives_the_sweep_and_an_ended_one_is_swept() {
    let flock = flock();
    let token = started(&flock);
    assert!(
        flock.turn_expire(None, &NoHooks).is_empty(),
        "a live turn is not released"
    );
    assert!(flock.turn_active(PANE));
    flock
        .turn_end(None, "turn_end", Some(PANE), Some(&token), true, &NoHooks)
        .unwrap();
    let swept = flock.turn_expire(None, &NoHooks);
    assert_eq!(
        swept.len(),
        1,
        "an ended turn (expires_at 0) is swept like the Python's"
    );
    assert!(flock.turn_states.lock().unwrap().get(PANE).is_none());
}

#[test]
fn turn_expire_takes_an_explicit_now() {
    let flock = flock();
    started(&flock);
    let far = crate::clock::now_s() + 10_000.0;
    assert_eq!(flock.turn_expire(Some(far), &NoHooks).len(), 1);
}

// -- T36 slice 4 (t1412): the turn-end nudge, from the dispatch half of
//    tests/test_dunk_turn_end_dispatch.py. The planning functions are T54's, so the fake plans.

struct Env {
    committed_flock: Option<std::sync::Arc<Flock>>,
    enabled: bool,
    unread: Result<Option<i64>, String>,
    tasks: Result<Vec<Value>, String>,
    pending: Vec<String>,
    readiness: Result<Readiness, String>,
    send: Result<Value, String>,
    handoffs_open: bool,
    fail_advance: bool,
    fail_handoffs: bool,
    next_job: Value,
    plan_skip: Option<&'static str>,
    panic_in_plan: bool,
    sent: Mutex<Vec<String>>,
    metrics: Mutex<Vec<String>>,
    plan_saw: Mutex<Vec<PlanObservation>>,
}

type PlanObservation = (Option<i64>, Option<f64>, usize);

impl Env {
    fn ready() -> Env {
        Env {
            committed_flock: None,
            enabled: true,
            unread: Ok(Some(0)),
            tasks: Ok(vec![json!({ "id": "t1", "title": "vitals row colour" })]),
            pending: vec![],
            readiness: Ok(Readiness {
                allowed: true,
                candidate: true,
                ready: true,
                not_ready_reason: None,
            }),
            send: Ok(json!({ "outcome": "ok", "message_id": "m1", "message": "typed" })),
            handoffs_open: false,
            fail_advance: false,
            fail_handoffs: false,
            next_job: json!({ "action": "none", "reason": "no_handoff" }),
            plan_skip: None,
            panic_in_plan: false,
            sent: Mutex::new(vec![]),
            metrics: Mutex::new(vec![]),
            plan_saw: Mutex::new(vec![]),
        }
    }
    fn metric(&self, what: &str) -> bool {
        self.metrics.lock().unwrap().iter().any(|m| m == what)
    }
}

impl TurnDispatchEnv for Env {
    fn dispatch_enabled(&self) -> bool {
        self.enabled
    }
    fn unread_count(&self, _: &str) -> Result<Option<i64>, String> {
        self.unread.clone()
    }
    fn claimed_tasks(&self, _: &str) -> Result<Vec<Value>, String> {
        self.tasks.clone()
    }
    fn turn_end_plan(
        &self,
        _: &str,
        tasks: &[Value],
        unread: Option<i64>,
        last: Option<f64>,
        nudges: &BTreeMap<String, f64>,
        _: f64,
    ) -> Value {
        self.plan_saw
            .lock()
            .unwrap()
            .push((unread, last, nudges.len()));
        if self.panic_in_plan {
            panic!("a hook failed");
        }
        if let Some(reason) = self.plan_skip {
            return json!({ "action": "skip", "reason": reason });
        }
        if unread.is_none() {
            return json!({ "action": "skip", "reason": "unread_unknown" });
        }
        if last.is_some() {
            return json!({ "action": "skip", "reason": "cooldown" });
        }
        match tasks.first() {
            Some(task) => json!({ "action": "nudge", "item": task }),
            None => json!({ "action": "skip", "reason": "no_open_work" }),
        }
    }
    fn pending_outbox(&self, _: &str) -> Vec<String> {
        self.pending.clone()
    }
    fn readiness(&self, _: &str, _: Option<i64>) -> Result<Readiness, String> {
        self.readiness.clone()
    }
    fn turn_end_text(&self, task: &Value) -> String {
        format!(
            "TASK {} - {}",
            task["id"].as_str().unwrap_or(""),
            task["title"].as_str().unwrap_or("")
        )
    }
    fn send_now(&self, _: &str, text: &str) -> Result<Value, String> {
        self.sent.lock().unwrap().push(text.to_string());
        self.send.clone()
    }
    fn record(&self, pane: &str, outcome: &str, reason: Option<&str>, bytes: Option<usize>) {
        self.metrics
            .lock()
            .unwrap()
            .push(format!("{pane} {outcome} {reason:?} {}", bytes.is_some()));
    }
    fn advance_next_job(&self, pane: &str) -> Result<Value, crate::error::DunkError> {
        if let Some(flock) = &self.committed_flock {
            assert_eq!(pane, PANE);
            assert!(ended_at(flock).is_some());
            assert!(!flock.turn_active(pane));
            assert!(flock
                .turns
                .lock()
                .unwrap()
                .get(pane)
                .unwrap()
                .ended_at
                .is_some());
            assert!(
                !take_push_wake(flock),
                "wake must follow successful advance"
            );
        }
        if self.fail_advance {
            return Err(crate::error::DunkError::Refused("advance failed".into()));
        }
        Ok(self.next_job.clone())
    }
    fn open_handoffs(&self, _: &str, _: bool) -> Result<bool, crate::error::DunkError> {
        self.metrics.lock().unwrap().push("handoff_read".into());
        if self.fail_handoffs {
            return Err(crate::error::DunkError::Refused("handoff failed".into()));
        }
        Ok(self.handoffs_open)
    }
}

fn dispatch(flock: &Flock, env: &Env) -> Value {
    flock.dispatch_claimed_turn_end(PANE, 1000.0, env)
}

#[test]
fn a_lane_holding_work_and_holding_no_mail_is_prompted() {
    let (flock, env) = (flock(), Env::ready());
    let out = dispatch(&flock, &env);
    assert_eq!(out["action"], "nudge");
    assert_eq!(out["delivered"], true);
    assert_eq!(out["task_id"], "t1");
    assert_eq!(out["item"], "board:t1");
    assert_eq!(out["message_id"], "m1");
    assert_eq!(
        env.sent.lock().unwrap().as_slice(),
        ["TASK t1 - vitals row colour"]
    );
    assert!(
        env.metric("w1:p1 ok None true"),
        "an ok metric carrying the byte count"
    );
}

#[test]
fn the_trigger_can_be_switched_off() {
    let (flock, mut env) = (flock(), Env::ready());
    env.enabled = false;
    let out = dispatch(&flock, &env);
    assert_eq!(out["reason"], "disabled");
    assert!(env.sent.lock().unwrap().is_empty() && env.metrics.lock().unwrap().is_empty());
}

#[test]
fn a_lane_with_no_open_work_is_left_alone() {
    let (flock, mut env) = (flock(), Env::ready());
    env.tasks = Ok(vec![]);
    let out = dispatch(&flock, &env);
    assert_eq!(
        (out["reason"].as_str(), out["delivered"].as_bool()),
        (Some("no_open_work"), Some(false))
    );
    assert!(env.sent.lock().unwrap().is_empty());
    assert!(env.metric("w1:p1 held Some(\"no_open_work\") false"));
}

#[test]
fn an_unreadable_mailbox_is_a_skip_not_a_guess() {
    let (flock, mut env) = (flock(), Env::ready());
    env.unread = Err("db busy".into());
    let out = dispatch(&flock, &env);
    assert_eq!(out["reason"], "unread_unknown");
    assert_eq!(out["error"], "inbox count failed: db busy");
    assert_eq!(
        env.plan_saw.lock().unwrap()[0].0,
        None,
        "the plan is told the count is unknown"
    );
    assert!(env.sent.lock().unwrap().is_empty());
}

#[test]
fn an_unreadable_task_list_is_recorded_and_plans_with_none() {
    let (flock, mut env) = (flock(), Env::ready());
    env.tasks = Err("locked".into());
    let out = dispatch(&flock, &env);
    assert_eq!(out["error"], "task list failed: locked");
    assert_eq!(out["reason"], "no_open_work");
}

#[test]
fn a_prompt_still_waiting_is_not_stacked_by_the_next_turn_end() {
    let (flock, mut env) = (flock(), Env::ready());
    env.pending = vec!["m0".into()];
    let out = dispatch(&flock, &env);
    assert_eq!(out["reason"], "held_delivery_pending");
    assert!(env.sent.lock().unwrap().is_empty());
    assert!(env.metric("w1:p1 held Some(\"held_delivery_pending\") false"));
}

#[test]
fn empty_allowlist_never_prompts_a_claimed_lane() {
    let (flock, mut env) = (flock(), Env::ready());
    env.readiness = Ok(Readiness {
        allowed: false,
        candidate: true,
        ready: true,
        not_ready_reason: None,
    });
    assert_eq!(dispatch(&flock, &env)["reason"], "not_allowed");
    env.readiness = Ok(Readiness {
        allowed: true,
        candidate: false,
        ready: true,
        not_ready_reason: None,
    });
    assert_eq!(dispatch(&flock, &env)["reason"], "unavailable_lane");
    assert!(env.sent.lock().unwrap().is_empty());
}

#[test]
fn the_shared_gate_blocks_a_lane_that_is_not_ready_and_names_why() {
    for why in ["has_mail", "blocked", "dead", "working"] {
        let (flock, mut env) = (flock(), Env::ready());
        env.readiness = Ok(Readiness {
            allowed: true,
            candidate: true,
            ready: false,
            not_ready_reason: Some(why.into()),
        });
        let out = dispatch(&flock, &env);
        assert_eq!(out["reason"], why);
        assert!(env.sent.lock().unwrap().is_empty(), "{why}: nothing queued");
    }
}

#[test]
fn unreadable_lane_fails_closed() {
    let (flock, mut env) = (flock(), Env::ready());
    env.readiness = Err("pane lookup failed".into());
    let out = dispatch(&flock, &env);
    assert_eq!(out["reason"], "readiness_unknown");
    assert_eq!(out["error"], "pane lookup failed");
    assert!(env.sent.lock().unwrap().is_empty());
}

#[test]
fn a_held_prompt_is_never_counted_as_delivered() {
    let (flock, mut env) = (flock(), Env::ready());
    env.send = Ok(json!({ "outcome": "queued", "message_id": "m9", "message": "held" }));
    let out = dispatch(&flock, &env);
    assert_eq!(out["action"], "nudge");
    assert_eq!(out["delivered"], false);
    assert_eq!(out["outcome"], "queued");
    assert!(env.metric("w1:p1 held Some(\"queued\") true"));
    // ... and it does not start the cooldown clock: the next turn end still gets planned fresh.
    let again = dispatch(&flock, &env);
    assert_eq!(again["action"], "nudge", "no cooldown after a held send");
}

#[test]
fn a_refused_send_is_recorded_not_raised_and_not_delivered() {
    let (flock, mut env) = (flock(), Env::ready());
    env.send = Err("pane gone".into());
    let out = dispatch(&flock, &env);
    assert_eq!(
        (
            out["delivered"].as_bool(),
            out["outcome"].as_str(),
            out["error"].as_str()
        ),
        (Some(false), Some("refused"), Some("pane gone"))
    );
    assert!(env.metric("w1:p1 failed Some(\"refused\") false"));
}

#[test]
fn the_same_pane_is_not_prompted_twice_in_a_row() {
    let (flock, env) = (flock(), Env::ready());
    assert_eq!(dispatch(&flock, &env)["delivered"], true);
    let second = dispatch(&flock, &env);
    assert_eq!(second["reason"], "cooldown");
    assert_eq!(env.sent.lock().unwrap().len(), 1, "one prompt only");
    let saw = env.plan_saw.lock().unwrap().clone();
    assert_eq!(
        saw[1].1,
        Some(1000.0),
        "the plan is handed when the pane was last prompted"
    );
    assert_eq!(saw[1].2, 1, "and when each held task was");
}

#[test]
fn a_task_id_that_is_a_number_is_keyed_like_pythons_str() {
    let (flock, mut env) = (flock(), Env::ready());
    env.tasks = Ok(vec![json!({ "id": 7, "title": "n" })]);
    assert_eq!(dispatch(&flock, &env)["task_id"], "7");
}

#[test]
fn concurrent_turn_ends_cannot_enqueue_twice() {
    let (flock, env) = (flock(), Env::ready());
    flock
        .turn_dispatch
        .lock()
        .unwrap()
        .dispatching
        .insert(PANE.to_string());
    let second = flock.dispatch_on_turn_end(PANE, &env).unwrap();
    assert_eq!(second["reason"], "dispatch_in_progress");
    assert_eq!(second["delivered"], false);
    assert!(env.sent.lock().unwrap().is_empty());
    flock.turn_dispatch.lock().unwrap().dispatching.clear();
    assert_eq!(
        flock.dispatch_on_turn_end(PANE, &env).unwrap()["delivered"],
        true
    );
    assert!(
        flock.turn_dispatch.lock().unwrap().dispatching.is_empty(),
        "the reservation is released"
    );
}

#[test]
fn a_next_job_claim_or_an_open_handoff_or_a_closed_lane_ends_before_the_nudge() {
    let flock = flock();
    let mut env = Env::ready();
    env.next_job = json!({ "action": "claim", "reason": "handoff" });
    let out = flock.dispatch_on_turn_end(PANE, &env).unwrap();
    assert_eq!(
        (out["action"].as_str(), out["delivered"].as_bool()),
        (Some("claim"), Some(false))
    );
    let mut env = Env::ready();
    env.handoffs_open = true;
    assert_eq!(
        flock.dispatch_on_turn_end(PANE, &env).unwrap()["delivered"],
        false
    );
    for reason in [
        "disabled",
        "not_allowed",
        "unavailable_lane",
        "readiness_unknown",
    ] {
        let mut env = Env::ready();
        env.next_job = json!({ "action": "skip", "reason": reason });
        let out = flock.dispatch_on_turn_end(PANE, &env).unwrap();
        assert_eq!(out["reason"], reason);
        assert!(env.sent.lock().unwrap().is_empty());
    }
}

#[test]
fn a_panicking_hook_does_not_wedge_the_pane() {
    // Python releases the reservation in `finally`, so a failing dispatch cannot wedge a lane.
    // Review t1421: one panic in an env hook left the pane `dispatch_in_progress` for ever.
    let flock = flock();
    let mut broken = Env::ready();
    broken.panic_in_plan = true;
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        flock.dispatch_on_turn_end(PANE, &broken).unwrap()
    }));
    assert!(caught.is_err(), "the hook's panic propagates");
    assert!(
        flock.turn_dispatch.lock().unwrap().dispatching.is_empty(),
        "the reservation was released"
    );
    let healthy = Env::ready();
    let again = flock.dispatch_on_turn_end(PANE, &healthy).unwrap();
    assert_eq!(
        again["delivered"], true,
        "the same pane dispatches normally afterwards"
    );
    assert_eq!(
        healthy.plan_saw.lock().unwrap().len(),
        1,
        "the healthy plan actually ran"
    );
    // A different pane was never affected.
    assert_eq!(
        flock.dispatch_claimed_turn_end("w1:p2", 1000.0, &Env::ready())["delivered"],
        true
    );
}

#[test]
fn a_gate_that_is_not_ready_with_no_named_reason_fails_closed() {
    let (flock, mut env) = (flock(), Env::ready());
    env.readiness = Ok(Readiness {
        allowed: true,
        candidate: true,
        ready: false,
        not_ready_reason: None,
    });
    let out = dispatch(&flock, &env);
    assert_eq!(
        out["reason"], "readiness_unknown",
        "Python: StopIteration lands in the fail-closed branch"
    );
    assert!(env.sent.lock().unwrap().is_empty());
}

#[test]
fn a_task_with_no_id_is_the_text_none_like_pythons_str() {
    let (flock, mut env) = (flock(), Env::ready());
    env.tasks = Ok(vec![json!({ "title": "no id" })]);
    let out = dispatch(&flock, &env);
    assert_eq!(out["task_id"], "None");
    assert_eq!(out["item"], "board:None");
}

#[test]
fn turn_dispatch_errors_preserve_wake_and_nudge_order() {
    use std::time::Duration;
    let flock = flock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let wake = flock.push_wake();
    let mut env = Env::ready();
    env.fail_advance = true;
    assert!(flock.dispatch_on_turn_end(PANE, &env).is_err());
    assert!(runtime
        .block_on(async { tokio::time::timeout(Duration::from_millis(20), wake.notified()).await })
        .is_err());
    assert!(env.sent.lock().unwrap().is_empty());
    env.fail_advance = false;
    env.fail_handoffs = true;
    assert!(flock.dispatch_on_turn_end(PANE, &env).is_err());
    assert!(runtime
        .block_on(async { tokio::time::timeout(Duration::from_millis(100), wake.notified()).await })
        .is_ok());
    assert!(env.sent.lock().unwrap().is_empty());
    env.next_job = json!({"action":"claim", "reason":"handoff"});
    assert!(
        flock.dispatch_on_turn_end(PANE, &env).is_err(),
        "claim still reads the handoff store"
    );
    assert!(runtime
        .block_on(async { tokio::time::timeout(Duration::from_millis(100), wake.notified()).await })
        .is_ok());
    assert!(env.sent.lock().unwrap().is_empty());
}

// Poll without a wall-clock timeout: Notify retains a permit even before a waiter exists.
fn take_push_wake(flock: &Flock) -> bool {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let wake = flock.push_wake();
    let mut notified = std::pin::pin!(wake.notified());
    matches!(
        notified
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(())
    )
}

#[test]
fn turn_end_wakes_next_job_push_after_state_commit() {
    use crate::flock::panes::Stores;
    use crate::server::RequestCtx;
    use crate::server_runtime::RequestRuntimeAdapter;
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .unwrap(),
    );
    runtime.block_on(tasks.allocation_init()).unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    flock.attach_stores(Stores {
        tasks: Some(Arc::clone(&tasks)),
        inbox: Some(Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        )),
        ..Stores::default()
    });
    let gate_env = crate::config::Env::from_pairs(&[("DUNKINGSHEEP_TURN_END_DISPATCH", "0")]);
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            dir.path().join("state.json"),
            std::env::current_exe().unwrap(),
            gate_env.clone(),
        ))
        .unwrap();
    let jobs = crate::tasks::next_job_runtime::NextJobs::with_providers(
        Arc::clone(&flock),
        Arc::new(move || Ok(gate_env.clone())),
        Some(Arc::new(|| Box::new(FakeTerminal::new()))),
    );
    let request = RequestCtx {
        caller: Some(Caller {
            pane_id: Some(PANE.into()),
            pid: Some(41),
            start: Some("100".into()),
            ..Caller::default()
        }),
        ..RequestCtx::default()
    };
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
        .with_next_jobs(&jobs)
        .unwrap();
    // These are the actual request-bound TurnHooks and NextJobs, not a dispatch callback.
    let token = started(&flock);
    assert!(!take_push_wake(&flock));
    let result = flock
        .turn_end(None, "done", Some(PANE), Some(&token), true, &adapter)
        .unwrap();
    assert_eq!(result["ended"], true);
    assert_eq!(result["dispatch"]["reason"], "disabled");
    assert!(ended_at(&flock).is_some());
    assert!(!flock.turn_active(PANE));
    assert!(
        take_push_wake(&flock),
        "successful skip advance must wake push"
    );
    assert!(!take_push_wake(&flock), "no second pending wake");
    let decisions = runtime.block_on(tasks.next_job_decisions()).unwrap();
    assert!(decisions.iter().any(|(pane, plan)| pane == PANE
        && plan["reason"] == "disabled"
        && plan["caller"]["pid"] == 41
        && plan["caller"]["start"] == "100"));

    // Every refusal must return before the canonical adapter can advance or wake.
    for case in [
        "already_ended",
        "no_active_turn",
        "expired_turn",
        "turn_mismatch",
        "foreign_turn",
        "unauthenticated",
    ] {
        let token = started(&flock);
        match case {
            "already_ended" => {
                flock
                    .turn_states
                    .lock()
                    .unwrap()
                    .get_mut(PANE)
                    .unwrap()
                    .ended_at = Some(1.0);
            }
            "no_active_turn" => {
                flock.turn_states.lock().unwrap().remove(PANE);
            }
            "expired_turn" => {
                flock
                    .turn_states
                    .lock()
                    .unwrap()
                    .get_mut(PANE)
                    .unwrap()
                    .expires_at = 1.0;
            }
            "foreign_turn" => flock.set_caller(Some(as_caller(42, "101"))),
            _ => {}
        }
        let before = runtime.block_on(tasks.next_job_decisions()).unwrap();
        let out = flock
            .turn_end(
                None,
                "done",
                Some(PANE),
                Some(if case == "turn_mismatch" {
                    "wrong"
                } else {
                    &token
                }),
                case != "unauthenticated",
                &adapter,
            )
            .unwrap();
        assert_eq!(out["dispatch"]["reason"], case);
        assert_eq!(out["ended"], case == "unauthenticated");
        assert!(!take_push_wake(&flock), "unexpected wake for {case}");
        assert_eq!(
            runtime.block_on(tasks.next_job_decisions()).unwrap(),
            before
        );
    }
}

#[test]
fn turn_end_commit_metric_advance_wake_and_error_order() {
    struct Hooks<'a> {
        flock: &'a Flock,
        env: &'a Env,
        recorded: std::cell::Cell<bool>,
    }
    impl TurnHooks for Hooks<'_> {
        fn record(
            &self,
            kind: &str,
            pane: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: &str,
            _: f64,
            _: Option<i64>,
        ) {
            assert_eq!(kind, "turn.end");
            assert_eq!(pane, PANE);
            assert!(ended_at(self.flock).is_some());
            assert!(!take_push_wake(self.flock));
            self.recorded.set(true);
        }
        fn observe_turn(&self, _: &str, _: &TurnState) -> Result<(), crate::error::DunkError> {
            unreachable!()
        }
        fn dispatch_on_turn_end(&self, pane: &str) -> Result<Value, crate::error::DunkError> {
            assert!(self.recorded.get());
            let result = self.flock.dispatch_on_turn_end(pane, self.env);
            assert_eq!(take_push_wake(self.flock), !self.env.fail_advance);
            result
        }
    }
    for case in [
        "advance_error",
        "handoff_error",
        "disabled",
        "not_allowed",
        "unavailable_lane",
        "readiness_unknown",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let flock = std::sync::Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let token = started(&flock);
        let mut env = Env::ready();
        env.committed_flock = Some(std::sync::Arc::clone(&flock));
        env.fail_advance = case == "advance_error";
        env.fail_handoffs = case == "handoff_error";
        if !case.ends_with("error") {
            env.next_job = json!({"action":"skip", "reason":case});
        }
        let hooks = Hooks {
            flock: &flock,
            env: &env,
            recorded: std::cell::Cell::new(false),
        };
        let result = flock.turn_end(None, "done", Some(PANE), Some(&token), true, &hooks);
        assert_eq!(result.is_err(), case.ends_with("error"));
        assert!(hooks.recorded.get());
        assert_eq!(env.metric("handoff_read"), case == "handoff_error");
        assert!(ended_at(&flock).is_some());
        assert!(
            !take_push_wake(&flock),
            "turn_end must not add another wake after dispatch"
        );
        assert!(env.sent.lock().unwrap().is_empty(), "no nudge on {case}");
    }
}

// ---- t4692: turn events ask the runtime to renew the sender's leases ----

/// Records each lease-renewal request: the pane and the identity of the process that sent it.
#[derive(Default)]
struct RenewSpy {
    renewals: Mutex<Vec<(String, Option<Value>)>>,
}

impl TurnHooks for RenewSpy {
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
    fn observe_turn(&self, _: &str, _: &TurnState) -> Result<(), crate::error::DunkError> {
        Ok(())
    }
    fn dispatch_on_turn_end(&self, _: &str) -> Result<Value, crate::error::DunkError> {
        Ok(json!({ "trigger": "turn.end", "action": "none" }))
    }
    fn renew_leases(&self, pane: &str, caller: Option<&Value>, _: f64) {
        self.renewals
            .lock()
            .unwrap()
            .push((pane.to_string(), caller.cloned()));
    }
}

#[test]
fn a_turn_start_and_a_valid_turn_end_each_renew_the_senders_leases_and_a_rejected_end_does_not() {
    let flock = flock();
    let spy = RenewSpy::default();
    flock.set_caller(Some(as_caller(41, "100")));
    let turn_id = flock
        .turn_start(None, None, Some("test"), Some(PANE), None, &spy)
        .unwrap()["turn_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        vec![(PANE.to_string(), Some(as_caller(41, "100")))],
        *spy.renewals.lock().unwrap(),
        "the start carries the identity of the process that sent it"
    );

    // An end from another process is rejected: it changes nothing and renews nothing.
    flock.set_caller(Some(as_caller(42, "101")));
    let foreign = flock
        .turn_end(None, "done", Some(PANE), Some(&turn_id), true, &spy)
        .unwrap();
    assert_eq!(json!(false), foreign["ended"]);
    assert_eq!(1, spy.renewals.lock().unwrap().len());

    // The real end renews again, with the ender's identity.
    flock.set_caller(Some(as_caller(41, "100")));
    let ended = flock
        .turn_end(None, "done", Some(PANE), Some(&turn_id), true, &spy)
        .unwrap();
    assert_eq!(json!(true), ended["ended"]);
    assert_eq!(
        vec![
            (PANE.to_string(), Some(as_caller(41, "100"))),
            (PANE.to_string(), Some(as_caller(41, "100")))
        ],
        *spy.renewals.lock().unwrap()
    );

    // A second end has no active turn: nothing more.
    let again = flock
        .turn_end(None, "done", Some(PANE), Some(&turn_id), true, &spy)
        .unwrap();
    assert_eq!(json!(false), again["ended"]);
    assert_eq!(2, spy.renewals.lock().unwrap().len());
}
