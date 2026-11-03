//! Focused tests for the T32 dunk persistence facade. Whole-file restart semantics belong to
//! the whole-Flock persistence leaf; these tests cover extraction and the snapshot callback.

use super::panes::PaneInfoEnv;
use super::send_keys::KeyPressEnv;
use super::sends::FlushEnv;
use super::sends::{
    outbox_step, send_submit_key, DirectSendOptions, DirectTextMetadata, OutboxStep,
    OutboxWorkerPool, OutboxWorkerRunner, SendEnv, SendPreparationEnv, SnapshotStore, SubmitEnv,
};
use super::Flock;
use crate::error::DunkError;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

type DunkRows = Vec<Vec<(&'static str, Value)>>;
type DunkSnapshot = (i64, DunkRows);

#[derive(Default)]
struct CapturingSnapshotStore {
    calls: AtomicUsize,
    dunks: Mutex<Option<DunkSnapshot>>,
}

impl SnapshotStore for CapturingSnapshotStore {
    fn save(&self, flock: &Flock) -> Result<(), DunkError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.dunks.lock().unwrap() = Some(flock.dunks.snapshot());
        Ok(())
    }
}

struct OfflineSnapshotStore;

impl SnapshotStore for OfflineSnapshotStore {
    fn save(&self, _flock: &Flock) -> Result<(), DunkError> {
        Err(DunkError::Refused("offline".into()))
    }
}

fn flock() -> (tempfile::TempDir, Flock) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    (dir, flock)
}

fn map(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

struct FireEnv {
    panes: Vec<Value>,
    typed: Vec<(String, String)>,
    keys: Vec<(String, String)>,
    fail_send: bool,
}

impl SendEnv for FireEnv {
    fn read_pane(&self, _pane_id: &str, _lines: i64, _ansi: bool) -> Option<String> {
        Some(include_str!("../../tests/fixtures/screen/codex-idle-footer-pane17.ansi").into())
    }
    fn pane_harness(&self, _pane_id: &str) -> Option<String> {
        None
    }
    fn session_process_gone(&self, _pane_id: &str) -> bool {
        false
    }
    fn read_status(&self, _pane_id: &str) -> Option<String> {
        None
    }
    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.keys.push((pane_id.into(), key.into()));
        (true, "sent".into())
    }
    fn federated_input(&self) -> bool {
        false
    }
}

impl SubmitEnv for FireEnv {
    fn sleep_s(&mut self, _seconds: f64) {}
    fn now_s(&mut self) -> f64 {
        1.0
    }
    fn send_submit_key_guarded(
        &mut self,
        pane_id: &str,
        fallback: &str,
        busy_hint: bool,
    ) -> (bool, String, Option<String>) {
        send_submit_key(self, pane_id, fallback, busy_hint)
    }
    fn remembered_submit_key(&self, _pane_id: &str) -> Option<String> {
        None
    }
    fn remember_submit_key(&mut self, _pane_id: &str, _key: &str) {}
    fn record_submit_attempt(
        &mut self,
        _pane_id: &str,
        _outcome: &str,
        _attempts: usize,
        _bytes: usize,
    ) {
    }
}

impl PaneInfoEnv for FireEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane.get("pane_id").and_then(Value::as_str) == Some(pane_id))
            .cloned()
    }
}

impl FlushEnv for FireEnv {
    fn server_error_hint(&self) -> String {
        "pane listing unavailable".into()
    }
}

impl KeyPressEnv for FireEnv {
    fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String) {
        self.keys
            .extend(keys.iter().map(|key| (pane_id.to_owned(), key.clone())));
        (true, "sent".into())
    }
    fn pending_text(&mut self, _pane_id: &str) -> Option<String> {
        None
    }
    fn confirm_pending(&mut self, _pane_id: &str, _text: &str) -> (bool, String) {
        (true, "confirmed".into())
    }
}

impl SendPreparationEnv for FireEnv {
    fn send_text(&mut self, pane_id: &str, text: &str) -> (bool, String) {
        if self.fail_send {
            return (false, "boom".into());
        }
        self.typed.push((pane_id.into(), text.into()));
        (true, "typed".into())
    }
    fn attempt_generation(&self, _pane_id: &str) -> Option<String> {
        Some("pid-1:start-1".into())
    }
    fn persist_typed_attempt(
        &mut self,
        _message: Option<&mut super::sends::Message>,
        _pane_id: &str,
        _text: &str,
        _press_enter: bool,
        _generation: Option<&str>,
    ) -> bool {
        true
    }
}

struct FireRunner(Arc<Mutex<FireEnv>>);

impl OutboxWorkerRunner for FireRunner {
    fn run(&self, flock: Arc<Flock>, pane_id: &str, stopped: &std::sync::atomic::AtomicBool) {
        loop {
            if stopped.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let mut env = self.0.lock().unwrap();
            let mut permission = || None;
            let outcome = outbox_step(
                &flock,
                &mut env,
                pane_id,
                crate::clock::now_s(),
                stopped,
                &|seconds| {
                    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
                    stopped.load(std::sync::atomic::Ordering::Acquire)
                },
                &mut permission,
            );
            if matches!(outcome, OutboxStep::Empty | OutboxStep::Stopped) {
                return;
            }
            if matches!(outcome, OutboxStep::Held | OutboxStep::Due) {
                drop(env);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

fn fire_env(fail_send: bool) -> FireEnv {
    FireEnv {
        panes: vec![json!({"pane_id": "w1:p2", "tab_label": "Agent"})],
        typed: Vec::new(),
        keys: Vec::new(),
        fail_send,
    }
}

fn load_one_fire_dunk(flock: &Flock, text: &str, max_sends: Option<i64>) {
    let mut row = json!({
        "id": "d1",
        "target_pane_id": "w1:p2",
        "target_tab": "Agent",
        "text": text,
        "created_at": 100.0
    });
    if let Some(max_sends) = max_sends {
        row["max_sends"] = json!(max_sends);
    }
    flock._load_dunks(&map(json!({"next_id": 2, "dunkers": [row]})));
}

#[test]
fn fire_sends_expanded_text_synchronously_through_guarded_direct_send() {
    let (_dir, flock) = flock();
    load_one_fire_dunk(&flock, "hello from {id}", None);
    let flock = Arc::new(flock);
    let store = CapturingSnapshotStore::default();
    let runner_env = Arc::new(Mutex::new(fire_env(false)));
    let pool = OutboxWorkerPool::new(Arc::new(FireRunner(runner_env.clone())));
    let mut env = fire_env(false);

    let result = flock.fire("d1", &mut env, &pool, &store, || None).unwrap();
    pool.stop_outbox_workers(&flock);

    assert_eq!(json!(true), result["ok"]);
    assert_eq!(json!(false), result["queued"]);
    assert_eq!(json!("d1"), result["id"]);
    let delivered = runner_env.lock().unwrap();
    assert_eq!(
        vec![("w1:p2".into(), "hello from d1".into())],
        delivered.typed
    );
    assert!(delivered.keys.iter().any(|(_, key)| key == "Enter"));
    let dunk = flock.dunks.get("d1").unwrap();
    assert_eq!(json!(1), dunk["send_count"]);
    assert_eq!(json!("hello from d1"), dunk["last_sent_text"]);
    assert_eq!(json!(true), dunk["awaiting_consumption"]);
    assert!(dunk["status"].as_str().unwrap().starts_with("Tested "));
    assert_eq!(2, store.calls.load(Ordering::SeqCst));
}

#[test]
fn fire_reports_guarded_send_failure_and_records_last_error() {
    let (_dir, flock) = flock();
    load_one_fire_dunk(&flock, "continue", None);
    let flock = Arc::new(flock);
    let store = CapturingSnapshotStore::default();
    let runner_env = Arc::new(Mutex::new(fire_env(true)));
    let pool = OutboxWorkerPool::new(Arc::new(FireRunner(runner_env.clone())));
    let mut env = fire_env(false);

    let result = flock.fire("d1", &mut env, &pool, &store, || None).unwrap();
    pool.stop_outbox_workers(&flock);

    assert_eq!(json!(false), result["ok"]);
    assert_eq!(json!("boom"), result["message"]);
    assert!(
        runner_env.lock().unwrap().keys.is_empty(),
        "a refused type must not submit Enter"
    );
    let dunk = flock.dunks.get("d1").unwrap();
    assert_eq!(json!(0), dunk["send_count"]);
    assert_eq!(json!("boom"), dunk["last_error"]);
    assert_eq!(json!("Test failed"), dunk["status"]);
}

#[test]
fn save_dunks_invokes_the_whole_flock_snapshot_store_with_extracted_fields() {
    let (_dir, flock) = flock();
    flock._load_dunks(&map(json!({
        "next_id": 5,
        "dunkers": [
            {"id": "d4", "text": "continue", "running": true,
             "created_at": 100.0, "next_send_at": 1.0},
            "corrupt row"
        ]
    })));

    let store = CapturingSnapshotStore::default();
    flock._save_dunks(&store).unwrap();

    assert_eq!(1, store.calls.load(Ordering::SeqCst));
    let (next_id, rows) = store.dunks.lock().unwrap().clone().unwrap();
    assert_eq!(5, next_id);
    assert_eq!(1, rows.len(), "corrupt dunk rows are skipped");
    let row = &rows[0];
    let field = |name: &str| {
        row.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    };
    assert_eq!(Some(&json!("d4")), field("id"));
    assert_eq!(Some(&json!(true)), field("running"));
    assert!(
        field("status").is_none(),
        "volatile status is not persisted"
    );
    assert!(field("next_send_at").and_then(Value::as_f64).unwrap() > crate::clock::now_s());
}

#[test]
fn load_dunks_tolerates_a_corrupt_dunk_collection() {
    let (_dir, flock) = flock();
    flock._load_dunks(&map(json!({"next_id": "bad", "dunkers": "corrupt"})));

    let (next_id, rows) = flock.dunks.snapshot();
    assert_eq!(1, next_id);
    assert!(rows.is_empty());
}

#[test]
fn clear_historical_errors_only_clears_running_exact_live_targets() {
    let (_dir, flock) = flock();
    flock._load_dunks(&map(json!({
        "next_id": 5,
        "dunkers": [
            {"id": "d1", "running": true, "target_pane_id": "%1",
             "last_error": "tmux: target pane not found"},
            {"id": "d2", "running": true, "target_pane_id": "%2",
             "last_error": "target pane not found"},
            {"id": "d3", "running": false, "target_pane_id": "%1",
             "last_error": "target pane not found"},
            {"id": "d4", "running": true, "target_pane_id": "%1",
             "last_error": "other error"}
        ]
    })));

    assert!(flock
        .dunks
        .clear_historical_errors(&BTreeSet::from(["%1".to_string()])));
    let rows = flock.dunks.list();
    let error = |id: &str| {
        rows.iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|row| row.get("last_error"))
            .cloned()
    };
    assert_eq!(Some(Value::Null), error("d1"));
    assert_eq!(Some(json!("target pane not found")), error("d2"));
    assert_eq!(Some(json!("target pane not found")), error("d3"));
    assert_eq!(Some(json!("other error")), error("d4"));
    assert!(!flock
        .dunks
        .clear_historical_errors(&BTreeSet::from(["%1".to_string()])));
}

#[test]
fn save_dunks_forwards_snapshot_store_errors() {
    let (_dir, flock) = flock();
    let error = flock._save_dunks(&OfflineSnapshotStore).unwrap_err();
    assert!(matches!(error, DunkError::Refused(message) if message == "offline"));
}
