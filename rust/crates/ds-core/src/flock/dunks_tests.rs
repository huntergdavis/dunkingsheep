//! Focused tests for the T32 dunk persistence facade. Whole-file restart semantics belong to
//! the whole-Flock persistence leaf; these tests cover extraction and the snapshot callback.

use super::dunks::UpdateArgs;
use super::panes::PaneInfoEnv;
use super::send_keys::KeyPressEnv;
use super::sends::FlushEnv;
use super::sends::{
    outbox_step, send_submit_key, OutboxStep, OutboxWake, SendEnv, SendPreparationEnv,
    SnapshotStore, SubmitEnv,
};
use super::Flock;
use crate::error::DunkError;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

fn new_flock() -> (tempfile::TempDir, Flock) {
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
    screen: String,
    typed: Vec<(String, String)>,
    keys: Vec<(String, String)>,
    fail_send: bool,
    fail_attempt_receipt: bool,
    attempts: Vec<(String, bool, Option<String>, Option<String>)>,
    mutate_dunk_during_send: Option<Arc<Flock>>,
    mutate_dunk_target_on_read: Mutex<Option<(Arc<Flock>, Value)>>,
}

impl SendEnv for FireEnv {
    fn read_pane(&self, _pane_id: &str, _lines: i64, _ansi: bool) -> Option<String> {
        if let Some((flock, target)) = self
            .mutate_dunk_target_on_read
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            flock
                .dunks
                .update("d1", Some(&map(target)), &UpdateArgs::default())
                .expect("target can be retargeted during guarded preflight");
        }
        Some(self.screen.clone())
    }
    fn pane_harness(&self, _pane_id: &str) -> Option<String> {
        Some("codex".into())
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
        if let Some(flock) = self.mutate_dunk_during_send.take() {
            flock
                .dunks
                .update(
                    "d1",
                    None,
                    &UpdateArgs {
                        text: Some("updated during I/O".into()),
                        ..UpdateArgs::default()
                    },
                )
                .expect("Dunker state lock must be free during terminal I/O");
        }
        self.typed.push((pane_id.into(), text.into()));
        (true, "typed".into())
    }
    fn attempt_generation(&self, _pane_id: &str) -> Option<String> {
        Some("pid-1:start-1".into())
    }
    fn persist_typed_attempt(
        &mut self,
        message: Option<&mut super::sends::Message>,
        _pane_id: &str,
        _text: &str,
        _press_enter: bool,
        _generation: Option<&str>,
    ) -> bool {
        if let Some(message) = message {
            self.attempts.push((
                message.source.clone(),
                message.scheduled,
                message.schedule_template.clone(),
                message.attempt_generation.clone(),
            ));
        }
        !self.fail_attempt_receipt
    }
}

struct NoopOutboxWake;

impl OutboxWake for NoopOutboxWake {
    fn ensure_worker(&self, _flock: &Arc<Flock>, _pane_id: &str) -> Result<(), DunkError> {
        Ok(())
    }
}

fn fire_env(fail_send: bool) -> FireEnv {
    FireEnv {
        panes: vec![json!({"pane_id": "w1:p2", "tab_label": "Agent"})],
        screen: idle_codex_screen(),
        typed: Vec::new(),
        keys: Vec::new(),
        fail_send,
        fail_attempt_receipt: false,
        attempts: Vec::new(),
        mutate_dunk_during_send: None,
        mutate_dunk_target_on_read: Mutex::new(None),
    }
}

fn idle_codex_screen() -> String {
    let esc = "\x1b[";
    let background = format!("{esc}48;2;30;30;30m");
    let hint = format!(
        "{esc}0m{esc}1m{background}›{esc}0m{esc}38;2;49;49;49m{background}⠁{esc}0m{esc}2m{background}Ask Codex to do anything{esc}0m{background}   {esc}0m"
    );
    [
        format!("{esc}0m{esc}2m  done 6:03 PM{esc}0m"),
        String::new(),
        format!("{esc}0m{esc}38;2;40;40;40m{background}⠁{esc}0m{background}   {esc}0m{esc}38;2;70;70;70m{background}⠈{esc}0m"),
        hint,
        format!("{esc}0m{background}       {esc}0m{esc}38;2;56;56;56m{background}⢀{esc}0m{background}        {esc}0m"),
        format!("  {esc}0m{esc}38;2;246;226;183mgpt-6-astra xhigh{esc}0m{esc}2m · {esc}0m{esc}38;2;171;223;167m~/workspace/fire-tests{esc}0m"),
    ]
    .join("\r\n")
        + "\r\n"
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
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "hello from {id}", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, Arc::clone(&store), || None)
        .unwrap();

    assert_eq!(json!(true), result["ok"], "fire result: {result}");
    assert_eq!(json!(false), result["queued"]);
    assert_eq!(json!("d1"), result["id"]);
    assert_eq!(vec![("w1:p2".into(), "hello from d1".into())], env.typed);
    assert!(env.keys.iter().any(|(_, key)| key == "Enter"));
    let dunk = flock.dunks.get("d1").unwrap();
    assert_eq!(json!(1), dunk["send_count"]);
    assert_eq!(json!("hello from d1"), dunk["last_sent_text"]);
    assert_eq!(json!(true), dunk["awaiting_consumption"]);
    assert!(dunk["status"].as_str().unwrap().starts_with("Tested "));
    assert_eq!(1, store.calls.load(Ordering::SeqCst));
    assert_eq!(
        vec![(
            "d1".into(),
            true,
            Some("hello from {id}".into()),
            Some("pid-1:start-1".into())
        )],
        env.attempts
    );
}

#[test]
fn fire_reports_guarded_send_failure_and_records_last_error() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(true);

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, Arc::clone(&store), || None)
        .unwrap();

    assert_eq!(json!(false), result["ok"], "fire result: {result}");
    assert_eq!(json!("boom"), result["message"]);
    assert!(env.keys.is_empty(), "a refused type must not submit Enter");
    let dunk = flock.dunks.get("d1").unwrap();
    assert_eq!(json!(0), dunk["send_count"]);
    assert_eq!(json!("boom"), dunk["last_error"]);
    assert_eq!(json!("Test failed"), dunk["status"]);
}

#[test]
fn immediate_dunk_retarget_during_guarded_preflight_keeps_target_placeholder_pinned() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "deliver to {target} {count}", None);
    let flock = Arc::new(flock);
    let mut env = fire_env(false);
    *env.mutate_dunk_target_on_read
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
        Arc::clone(&flock),
        json!({"pane_id": "w1:p3", "tab_label": "Other agent"}),
    ));

    let result = flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(CapturingSnapshotStore::default()),
            || None,
        )
        .unwrap();

    assert_eq!(json!(true), result["ok"]);
    assert_eq!(
        vec![("w1:p2".into(), "deliver to Agent 1".into())],
        env.typed
    );
    assert_eq!("w1:p3", flock.dunks.get("d1").unwrap()["target_pane_id"]);
}

#[test]
fn queued_fire_uses_live_template_and_updates_only_captured_same_id_dunker() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "first {id}", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);
    env.screen = "Press Enter to confirm or Esc to cancel".into();

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, Arc::clone(&store), || None)
        .unwrap();
    assert_eq!(json!(true), result["queued"]);
    let captured = flock.dunks.begin_fire("d1").unwrap();
    // Restore the queued volatile status after taking the stable object handle for assertions.
    super::dunks::DunkList::queue_fire(&captured);
    captured
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send_count = 4;

    flock
        .dunks
        .update(
            "d1",
            None,
            &UpdateArgs {
                text: Some("updated {count}".into()),
                ..UpdateArgs::default()
            },
        )
        .unwrap();
    flock.dunks.remove("d1").unwrap();
    flock._load_dunks(&map(json!({
        "next_id": 2,
        "dunkers": [{
            "id": "d1",
            "target_pane_id": "w1:p2",
            "target_tab": "Agent",
            "text": "replacement {id}",
            "created_at": 100.0
        }]
    })));

    let stopped = AtomicBool::new(false);
    let mut permission = || None;
    assert_eq!(
        OutboxStep::Held,
        outbox_step(
            &flock,
            &mut env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );
    assert!(
        env.attempts.is_empty(),
        "a held retry allocates no attempt frame"
    );
    env.screen = idle_codex_screen();
    let drained = outbox_step(
        &flock,
        &mut env,
        "w1:p2",
        crate::clock::now_s(),
        &stopped,
        &|_| false,
        &mut permission,
    );

    assert_eq!(OutboxStep::Delivered, drained);
    assert_eq!(
        OutboxStep::Empty,
        outbox_step(
            &flock,
            &mut env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );
    assert_eq!(
        vec![("w1:p2".into(), "updated 5".into())],
        env.typed,
        "the queued fire expands its current template when drained"
    );
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
    let captured = captured.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(5, captured.send_count);
    assert_eq!(Some("updated 5"), captured.last_sent_text.as_deref());
    assert_eq!(
        vec![(
            "d1".into(),
            true,
            Some("updated {count}".into()),
            Some("pid-1:start-1".into())
        )],
        env.attempts
    );
    assert_eq!(2, store.calls.load(Ordering::SeqCst));
    let history = flock
        .message_history
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let outer = history
        .iter()
        .find(|message| message.id == result["message_id"].as_str().unwrap())
        .expect("outer queued message lifecycle remains separate");
    assert_eq!("first {id}", outer.text);
    assert_eq!("d1", outer.source);
    assert_eq!("sent", outer.status);
}

#[test]
fn empty_text_skips_inner_attempt_and_post_send_snapshot_error_does_not_rollback() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "   ", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);
    let empty = flock
        .fire("d1", &mut env, &NoopOutboxWake, Arc::clone(&store), || None)
        .unwrap();
    assert_eq!(json!(true), empty["ok"]);
    assert_eq!(0, store.calls.load(Ordering::SeqCst));
    assert!(env.attempts.is_empty());
    assert!(env.keys.is_empty());

    let (_other_dir, other) = new_flock();
    load_one_fire_dunk(&other, "continue", None);
    let other = Arc::new(other);
    let failed = other
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(OfflineSnapshotStore),
            || None,
        )
        .unwrap();
    assert_eq!(json!(false), failed["ok"]);
    let dunk = other.dunks.get("d1").unwrap();
    assert_eq!(json!(1), dunk["send_count"]);
    assert_eq!(json!("offline"), dunk["last_error"]);
    assert!(env.keys.iter().any(|(_, key)| key == "Enter"));
}

#[test]
fn attempt_receipt_failure_prevents_enter_and_does_not_increment_count() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);
    env.fail_attempt_receipt = true;

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, store, || None)
        .unwrap();
    assert_eq!(json!(false), result["ok"]);
    assert!(!env.keys.iter().any(|(_, key)| key == "Enter"));
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
    assert_eq!(1, env.attempts.len());
}

#[test]
fn deferred_empty_expansion_skips_inner_attempt_and_restored_queue_is_plain_text() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "first {id}", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut held_env = fire_env(false);
    held_env.screen = "Press Enter to confirm or Esc to cancel".into();
    let queued = flock
        .fire(
            "d1",
            &mut held_env,
            &NoopOutboxWake,
            Arc::clone(&store),
            || None,
        )
        .unwrap();
    assert_eq!(json!(true), queued["queued"]);

    let saved = flock.snapshot();
    let (_restore_dir, restored) = new_flock();
    restored.restore_outbox(saved["outbox"].as_array().unwrap());
    let mut plain_env = fire_env(false);
    let stopped = AtomicBool::new(false);
    let mut permission = || None;
    assert_eq!(
        OutboxStep::Delivered,
        outbox_step(
            &restored,
            &mut plain_env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );
    assert_eq!(vec![("w1:p2".into(), "first {id}".into())], plain_env.typed);

    let (_empty_dir, empty_flock) = new_flock();
    load_one_fire_dunk(&empty_flock, "first {id}", None);
    let empty_flock = Arc::new(empty_flock);
    let empty_store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);
    env.screen = "Press Enter to confirm or Esc to cancel".into();
    empty_flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::clone(&empty_store),
            || None,
        )
        .unwrap();
    empty_flock
        .dunks
        .update(
            "d1",
            None,
            &UpdateArgs {
                text: Some("   ".into()),
                ..UpdateArgs::default()
            },
        )
        .unwrap();
    env.screen = idle_codex_screen();
    assert_eq!(
        OutboxStep::Delivered,
        outbox_step(
            &empty_flock,
            &mut env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );
    assert!(env.typed.is_empty());
    assert!(env.attempts.is_empty());
    assert_eq!(json!(0), empty_flock.dunks.get("d1").unwrap()["send_count"]);
}

#[test]
fn cancelled_queued_dunk_does_not_allocate_or_submit_an_attempt() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", None);
    let flock = Arc::new(flock);
    let mut env = fire_env(false);
    env.screen = "Press Enter to confirm or Esc to cancel".into();
    let result = flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(CapturingSnapshotStore::default()),
            || None,
        )
        .unwrap();
    let message_id = result["message_id"].as_str().unwrap();
    {
        let mut outbox = super::sends::outbox_lock(&flock.outbox);
        let row = outbox
            .get_mut("w1:p2")
            .unwrap()
            .iter_mut()
            .find(|row| row.id == message_id)
            .unwrap();
        row.status = "cancelled".into();
    }
    let stopped = AtomicBool::new(false);
    let mut permission = || None;
    assert_eq!(
        OutboxStep::Empty,
        outbox_step(
            &flock,
            &mut env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );
    assert!(env.attempts.is_empty());
    assert!(env.typed.is_empty());
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
}

#[test]
fn deferred_dunk_retarget_during_guarded_preflight_keeps_the_outer_pane() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "deliver to {target} {count}", None);
    let flock = Arc::new(flock);
    let mut env = fire_env(false);
    env.screen = "Press Enter to confirm or Esc to cancel".into();
    let queued = flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(CapturingSnapshotStore::default()),
            || None,
        )
        .unwrap();
    assert_eq!(json!(true), queued["queued"]);

    // The outbox row and its eventual guard target are p2. Retarget the live Dunker while
    // that guard is performing composer preflight, before the delivery action snapshots it.
    env.screen = idle_codex_screen();
    *env.mutate_dunk_target_on_read
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((
        Arc::clone(&flock),
        json!({"pane_id": "w1:p3", "tab_label": "Other agent"}),
    ));

    let stopped = AtomicBool::new(false);
    let mut permission = || None;
    assert_eq!(
        OutboxStep::Delivered,
        outbox_step(
            &flock,
            &mut env,
            "w1:p2",
            crate::clock::now_s(),
            &stopped,
            &|_| false,
            &mut permission,
        )
    );

    assert_eq!(
        vec![("w1:p2".into(), "deliver to Agent 1".into())],
        env.typed
    );
    assert_eq!("w1:p3", flock.dunks.get("d1").unwrap()["target_pane_id"]);
    assert_eq!(
        vec![(
            "d1".into(),
            true,
            Some("deliver to {target} {count}".into()),
            Some("pid-1:start-1".into())
        )],
        env.attempts
    );
}

#[test]
fn dunk_state_lock_is_released_during_terminal_io() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", None);
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);
    env.mutate_dunk_during_send = Some(Arc::clone(&flock));

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, store, || None)
        .unwrap();
    assert_eq!(json!(true), result["ok"]);
    assert_eq!(vec![("w1:p2".into(), "continue".into())], env.typed);
    assert_eq!(json!(1), flock.dunks.get("d1").unwrap()["send_count"]);
    assert_eq!(
        json!("updated during I/O"),
        flock.dunks.get("d1").unwrap()["text"]
    );
}

#[test]
fn manual_fire_with_max_sends_does_not_remove_the_dunk() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", Some(1));
    let flock = Arc::new(flock);
    let store = Arc::new(CapturingSnapshotStore::default());
    let mut env = fire_env(false);

    let result = flock
        .fire("d1", &mut env, &NoopOutboxWake, Arc::clone(&store), || None)
        .unwrap();

    assert_eq!(json!(true), result["ok"]);
    assert_eq!(json!(1), flock.dunks.get("d1").unwrap()["send_count"]);
}

#[test]
fn successful_dunk_attempt_surfaces_and_clears_detector_fallback() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "continue", None);
    flock.dunks.set_detector_fallback("d1", true);
    let flock = Arc::new(flock);
    let mut env = fire_env(false);

    let result = flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(CapturingSnapshotStore::default()),
            || None,
        )
        .unwrap();
    assert_eq!(json!(true), result["ok"]);
    assert_eq!(
        json!("input box not recognised on this pane; sent anyway after repeated skips (detector fallback)"),
        flock.dunks.get("d1").unwrap()["last_error"]
    );
}

#[test]
fn captured_dunker_handle_survives_remove_and_same_id_reload() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "first {id}", None);
    let captured = flock.dunks.begin_fire("d1").unwrap();
    flock
        .dunks
        .update(
            "d1",
            None,
            &UpdateArgs {
                text: Some("updated {id}".into()),
                ..UpdateArgs::default()
            },
        )
        .unwrap();

    flock.dunks.remove("d1").unwrap();
    flock._load_dunks(&map(json!({
        "next_id": 2,
        "dunkers": [{
            "id": "d1",
            "target_pane_id": "w1:p2",
            "target_tab": "Agent",
            "text": "replacement {id}",
            "created_at": 100.0
        }]
    })));
    let replacement = flock.dunks.begin_fire("d1").unwrap();

    assert!(!Arc::ptr_eq(&captured, &replacement));
    assert_eq!(
        "updated {id}",
        captured.lock().unwrap_or_else(|e| e.into_inner()).text
    );
    assert_eq!(
        "replacement {id}",
        replacement.lock().unwrap_or_else(|e| e.into_inner()).text
    );
}

#[test]
fn save_dunks_invokes_the_whole_flock_snapshot_store_with_extracted_fields() {
    let (_dir, flock) = new_flock();
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
    let (_dir, flock) = new_flock();
    flock._load_dunks(&map(json!({"next_id": "bad", "dunkers": "corrupt"})));

    let (next_id, rows) = flock.dunks.snapshot();
    assert_eq!(1, next_id);
    assert!(rows.is_empty());
}

#[test]
fn clear_historical_errors_only_clears_running_exact_live_targets() {
    let (_dir, flock) = new_flock();
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
    let (_dir, flock) = new_flock();
    let error = flock._save_dunks(&OfflineSnapshotStore).unwrap_err();
    assert!(matches!(error, DunkError::Refused(message) if message == "offline"));
}
