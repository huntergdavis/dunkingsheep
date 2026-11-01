//! Ported from tests/test_dunk_state_capture_cost.py: `StalePidBindingTests.test_capture_never_
//! reads_env_of_a_recycled_pid` and `OneProcWalkPerCapture`, same names and order. The Python
//! patches `_proc_snapshot`/`_env_of`/`dunk_activity.activity`; here a counting wrapper around the
//! live reader and a fake [`CaptureEnv`] play those roles.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::capture::{capture, CaptureEnv, LaneActivity};
use super::proc::{descendants, Kids, LiveProc, ProcReader, Starts};
use super::StateStore;

struct Lanes {
    lanes: Vec<Value>,
}

impl CaptureEnv for Lanes {
    fn resolve_lanes(&self) -> Vec<Value> {
        self.lanes.clone()
    }
    fn activity(&self, _: &Value, _: f64) -> LaneActivity {
        LaneActivity {
            state: None,
            source: Some("test".into()),
            reason: Some("test".into()),
        }
    }
    fn lease_task_id(&self, _: &str, _: f64) -> Option<String> {
        None
    }
}

fn lane(pane_id: &str, pid: Option<i64>) -> Value {
    json!({"pane_id": pane_id, "pane_pid": pid, "agent": "pi", "cwd": "/tmp", "project": "p", "harness": "pi"})
}

/// The live reader, with the call counts the tests assert on and the Python's two patches
/// (a recycled child injected into the snapshot; a counting `_env_of`).
struct Probe {
    inner: LiveProc,
    snapshots: Cell<usize>,
    children_walks: Cell<usize>,
    env_reads: RefCell<Vec<i64>>,
    recycled_child: Option<(i64, i64)>, // (parent pid, child pid): the snapshot records a start it lacks
    foreign_env: Option<i64>,
}

impl Probe {
    fn new() -> Probe {
        Probe {
            inner: LiveProc::new("/nonexistent-home"),
            snapshots: Cell::new(0),
            children_walks: Cell::new(0),
            env_reads: RefCell::new(Vec::new()),
            recycled_child: None,
            foreign_env: None,
        }
    }
}

impl ProcReader for Probe {
    fn proc_children(&self) -> Kids {
        self.children_walks.set(self.children_walks.get() + 1);
        self.inner.proc_children()
    }
    fn proc_snapshot(&self) -> (Kids, Starts) {
        self.snapshots.set(self.snapshots.get() + 1);
        let (mut kids, mut starts) = self.inner.proc_snapshot();
        if let Some((parent, child)) = self.recycled_child {
            // pretend our own pid's child is a pid that has since been reused
            kids.entry(parent).or_default().push(child);
            starts.insert(child, 1); // a start time it does not have
        }
        (kids, starts)
    }
    fn proc_start_ticks(&self, pid: i64) -> Option<i64> {
        self.inner.proc_start_ticks(pid)
    }
    fn cmdline(&self, pid: i64) -> Vec<String> {
        self.inner.cmdline(pid)
    }
    fn open_session_file(&self, pid: i64) -> Option<String> {
        self.inner.open_session_file(pid)
    }
    fn env_of(&self, pid: i64) -> BTreeMap<String, String> {
        self.env_reads.borrow_mut().push(pid);
        if self.foreign_env == Some(pid) {
            return [("PI_SESSION_ID", "foreign"), ("PI_MODEL", "foreign-model")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        }
        BTreeMap::new()
    }
    fn proc_start(&self, pid: i64) -> Option<f64> {
        self.inner.proc_start(pid)
    }
    fn active_session_file(
        &self,
        cwd: Option<&str>,
        since: Option<f64>,
    ) -> (Option<String>, usize) {
        self.inner.active_session_file(cwd, since)
    }
}

fn store(dir: &std::path::Path) -> StateStore {
    StateStore::open(dir.join("state.db")).expect("store")
}

fn me() -> i64 {
    i64::from(std::process::id())
}

fn run(probe: &Probe, lanes: Vec<Value>, store: &StateStore, dir: &std::path::Path) -> Value {
    capture(&Lanes { lanes }, probe, store, dir, "test", Some(100.0)).expect("capture")
}

#[test]
fn test_capture_never_reads_env_of_a_recycled_pid() {
    // End to end: the foreign environment must not reach the row.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store(dir.path());
    let mut probe = Probe::new();
    probe.recycled_child = Some((me(), 999_999));
    probe.foreign_env = Some(999_999);
    let result = run(&probe, vec![lane("%1", Some(me()))], &store, dir.path());
    assert_eq!(result["rows"], 1);
    assert!(
        !probe.env_reads.borrow().contains(&999_999),
        "read the environment of a recycled pid"
    );
    let row = &store.latest(None, None, 1).unwrap()[0];
    assert_ne!(row["session_id"], "foreign");
    assert_ne!(row["model"], "foreign-model");
}

#[test]
fn test_capture_walks_proc_once_not_twice_per_lane() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store(dir.path());
    let probe = Probe::new();
    let lanes: Vec<Value> = ["%1", "%2", "%3", "%4"]
        .iter()
        .map(|p| lane(p, Some(me())))
        .collect();
    let count = lanes.len();
    let result = run(&probe, lanes, &store, dir.path());
    assert_eq!(
        probe.snapshots.get(),
        1,
        "capture rebuilt the /proc map {} times for {count} lanes",
        probe.snapshots.get()
    );
    assert_eq!(probe.children_walks.get(), 0, "a per-lane walk came back");
    assert_eq!(result["rows"], count);
}

#[test]
fn test_rows_still_carry_a_session_field_and_their_pane() {
    // The saving must not change what a row says.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store(dir.path());
    let result = run(
        &Probe::new(),
        vec![lane("%7", Some(me()))],
        &store,
        dir.path(),
    );
    assert_eq!(result["rows"], 1);
    let rows = store.latest(None, None, 5).unwrap();
    let row = rows.first().expect("a row");
    for key in [
        "pane_id",
        "project",
        "harness",
        "session_id",
        "session_reason",
        "state",
        "state_source",
    ] {
        assert!(row.get(key).is_some(), "{key}");
    }
    assert_eq!(row["pane_id"], "%7");
}

#[test]
fn test_a_pane_without_a_pid_still_costs_exactly_one_walk() {
    // The map is built eagerly, so a capture with no pid still scans once. That is a deliberate
    // tradeoff (reviewer t500): one scan keeps the map identity shared by both consumers, at the
    // cost of a scan even when no lane has a pid.
    let dir = tempfile::tempdir().expect("tempdir");
    let store = store(dir.path());
    let probe = Probe::new();
    let result = run(&probe, vec![lane("%8", None)], &store, dir.path());
    assert_eq!(probe.snapshots.get(), 1);
    assert_eq!(result["rows"], 1);
}

#[test]
fn test_session_of_accepts_a_prebuilt_map() {
    // The parameter existed and was ignored; it is the whole fix.
    let probe = Probe::new();
    let kids: Kids = [(1, vec![2]), (2, vec![3])].into_iter().collect();
    assert_eq!(descendants(1, &kids), [2, 3]);
    let starts: Starts = [(me(), 1)].into_iter().collect();
    super::session::session_of(
        &probe,
        Some(&json!(me())),
        None,
        Some(&Kids::new()),
        None,
        Some(&starts),
    );
    assert_eq!(probe.children_walks.get(), 0, "walked /proc");
}

#[test]
fn test_one_snapshot_feeds_both_maps() {
    let (kids, starts) = LiveProc::new("/nonexistent-home").proc_snapshot();
    // the snapshot records start times for the same pids it lists as children
    for children in kids.values() {
        for child in children {
            assert!(starts.contains_key(child), "{child}");
        }
    }
}

// -- beyond the Python tests ----------------------------------------------------------------

#[test]
fn test_argv_flags_and_the_environment_fill_model_and_thinking_with_their_reasons() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("launches.json"),
        json!({"panes": {"%1": {"argv": ["pi", "--model", "m1", "--effort=high"]}, "%2": {"argv": ["pi"]}}}).to_string(),
    )
    .unwrap();
    let store = store(dir.path());
    let lanes = vec![
        lane("%1", None),
        lane("%2", None),
        lane("%3", None),
        json!({"pane_id": "%4", "agent": ""}),
    ];
    let result = run(&Probe::new(), lanes, &store, dir.path());
    assert_eq!(result["rows"], 3, "a non-agent lane has no row");
    let rows = store.latest(None, None, 10).unwrap();
    let by = |id: &str| rows.iter().find(|r| r["pane_id"] == id).unwrap().clone();
    assert_eq!(
        (by("%1")["model"].as_str(), by("%1")["thinking"].as_str()),
        (Some("m1"), Some("high"))
    );
    assert_eq!(by("%1")["model_reason"], Value::Null);
    assert_eq!(
        by("%1")["argv"],
        json!(["pi", "--model", "m1", "--effort=high"])
    );
    assert_eq!(
        by("%2")["model_reason"],
        "no --model flag in the argv, and the process environment names no model"
    );
    assert_eq!(
        by("%3")["thinking_reason"],
        "no launch row for this pane: no argv to read a thinking level from, and the process environment names no reasoning level"
    );
    assert_eq!(by("%3")["session_reason"], "the pane carries no pid, so its session cannot be read from the process; using the launch row");
    assert_eq!(by("%1")["project"], "tmp");
}
