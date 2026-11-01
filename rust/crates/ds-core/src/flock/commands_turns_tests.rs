//! T36 slice 1 tests: the turn state machine and its authentication, ported by name from the
//! authentication half of `tests/test_dunk_turn_end_dispatch.py` (the dispatch guardrails are the
//! next slice).

use std::sync::Mutex;

use serde_json::{json, Value};

use super::commands_turns::{NoHooks, TurnHooks, TurnState};
use super::Flock;
use crate::proc::Caller;

const PANE: &str = "w1:p1";

fn flock() -> Flock {
    let dir = std::env::temp_dir().join(format!("ds-turns-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false)
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
    fn observe_turn(&self, pane: &str, state: &TurnState) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("observe {pane} {}", state.turn_id.len()));
    }
    fn dispatch_on_turn_end(&self, pane: &str) -> Value {
        self.calls.lock().unwrap().push(format!("dispatch {pane}"));
        json!({ "trigger": "turn.end", "action": "claim", "delivered": true })
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
