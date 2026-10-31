//! `_pane_info` / `_role_pane_info` behaviour from dunk_core.py (t535 cache, 2026-10-30
//! fail-open grace). Not a 1:1 port: the Python tests that exercise it go through caller_role
//! (T31).

use super::panes::*;
use super::Flock;
use serde_json::{json, Value};
use std::cell::RefCell;

#[derive(Default)]
struct Env {
    grouped: RefCell<Vec<Value>>,
    single: RefCell<Option<Value>>,
    calls: RefCell<u32>,
}

impl PaneInfoEnv for Env {
    fn list_panes_grouped(&self) -> Vec<Value> {
        *self.calls.borrow_mut() += 1;
        self.grouped.borrow().clone()
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        self.single.borrow().clone()
    }
}

fn flock() -> Flock {
    let dir = std::env::temp_dir();
    Flock::new(dir.join("x-launches.json"), dir.join("x-lanes.json"), false)
}

#[test]
fn pane_info_prefers_the_grouped_listing_then_get_pane() {
    let env = Env::default();
    assert_eq!(Flock::pane_info(&env, ""), None);
    *env.grouped.borrow_mut() = vec![json!({"pane_id": "%1", "tab_label": "T"})];
    *env.single.borrow_mut() = Some(json!({"pane_id": "%2"}));
    assert_eq!(
        Flock::pane_info(&env, "%1").unwrap()["tab_label"],
        json!("T")
    );
    assert_eq!(
        Flock::pane_info(&env, "%2").unwrap(),
        json!({"pane_id": "%2"})
    );
    assert_eq!(
        Flock::pane_info(&env, "%3").unwrap(),
        json!({"pane_id": "%2"})
    ); // get_pane answers
    *env.single.borrow_mut() = None;
    assert_eq!(Flock::pane_info(&env, "%3"), None);
}

#[test]
fn role_pane_info_is_cached_for_two_seconds() {
    let (flock, env) = (flock(), Env::default());
    *env.grouped.borrow_mut() = vec![json!({"pane_id": "%1"})];
    assert!(flock.role_pane_info(&env, "%1", 100.0).is_some());
    assert!(flock.role_pane_info(&env, "%1", 101.9).is_some());
    assert_eq!(*env.calls.borrow(), 1);
    assert!(flock.role_pane_info(&env, "%1", 102.1).is_some());
    assert_eq!(*env.calls.borrow(), 2);
    assert_eq!(flock.role_pane_info(&env, "", 100.0), None);
}

#[test]
fn a_lane_seen_recently_survives_one_empty_answer_for_180_seconds() {
    let (flock, env) = (flock(), Env::default());
    *env.grouped.borrow_mut() = vec![json!({"pane_id": "%1", "agent": "claude"})];
    assert!(flock.role_pane_info(&env, "%1", 1000.0).is_some());
    env.grouped.borrow_mut().clear(); // the listing timed out / came back empty
    let held = flock.role_pane_info(&env, "%1", 1100.0).unwrap();
    assert_eq!(held["agent"], json!("claude"));
    // 181 s after the last GOOD answer the pane is really gone.
    assert_eq!(flock.role_pane_info(&env, "%1", 1181.0), None);
}

#[test]
fn a_pane_never_seen_is_not_invented_by_the_grace_window() {
    let (flock, env) = (flock(), Env::default());
    assert_eq!(flock.role_pane_info(&env, "%9", 5.0), None);
}

#[test]
fn stores_start_empty_and_can_be_attached() {
    let flock = flock();
    assert!(flock.stores().tasks.is_none() && flock.stores().inbox.is_none());
    flock.attach_stores(Stores::default());
    assert!(flock.stores().tasks.is_none());
}
