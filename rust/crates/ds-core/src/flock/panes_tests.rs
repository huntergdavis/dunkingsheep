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

fn flock() -> crate::testsupport::Scratch<Flock> {
    // t1490: a private directory, not files in the shared temp dir that every run reused.
    let dir = crate::testsupport::dir("panes");
    let flock = Flock::new(
        dir.path().join("x-launches.json"),
        dir.path().join("x-lanes.json"),
        false,
    );
    crate::testsupport::Scratch::new(flock, dir)
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

fn scratch(tag: &str) -> crate::testsupport::Scratch<std::path::PathBuf> {
    let scratch = crate::testsupport::dir(&format!("obs-{tag}"));
    crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch)
}

#[test]
fn a_flock_that_does_not_persist_gets_in_memory_metrics_and_no_memory_store() {
    let flock = flock();
    assert!(flock.stores().metrics.is_none() && flock.stores().memory.is_none());
    let obs = scratch("mem");
    flock
        .wire_observability(&obs, Some(std::path::Path::new("/nonexistent/m.db")))
        .unwrap();
    let stores = flock.stores();
    assert!(stores.metrics.is_some(), "metrics are always there");
    assert!(
        stores.memory.is_none(),
        "no memory store without persistence"
    );
}

#[test]
fn a_persisting_flock_keeps_its_metrics_beside_the_state_file() {
    let dir = scratch("persist");
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), true);
    // no memory db on disk: the flock still builds, memory stays None
    flock
        .wire_observability(&dir, Some(&dir.join("memory.db")))
        .unwrap();
    assert!(dir.join("metrics.db").exists());
    assert!(flock.stores().memory.is_none());
}

#[test]
fn metrics_record_prune_and_earliest() {
    use crate::metrics::{Event, MetricsStore, MAX_EVENTS};
    let metrics_dir = scratch("metrics");
    let store = MetricsStore::open(metrics_dir.join("metrics.db")).unwrap();
    assert_eq!(None, store.earliest().unwrap());
    let now = crate::clock::now_s();
    for (age, kind) in [
        (100.0 * 86400.0, "old"),
        (10.0, "message.sent"),
        (5.0, "message.sent"),
    ] {
        store.record(&Event {
            at: now - age,
            kind: kind.into(),
            pane_id: Some("w1:p1".into()),
            duration_ms: Some(7),
            ..Default::default()
        });
    }
    assert!(store.earliest().unwrap().unwrap() < now - 99.0 * 86400.0);
    // the 30-day retention drops only the old one
    assert_eq!(1, store.prune(None, MAX_EVENTS).unwrap());
    let kept = store.events_between(now - 60.0, now + 1.0).unwrap();
    assert_eq!(2, kept.len());
    assert_eq!(Some(7), kept[0].duration_ms);
    // the cap drops the oldest beyond max_events
    assert_eq!(1, store.prune(None, 1).unwrap());
    assert_eq!(
        1,
        store.events_between(now - 60.0, now + 1.0).unwrap().len()
    );
    // a disabled store records nothing
    let mut off = MetricsStore::open(":memory:").unwrap();
    off.enabled = false;
    off.record(&Event {
        kind: "x".into(),
        ..Default::default()
    });
    assert_eq!(None, off.earliest().unwrap());
}
