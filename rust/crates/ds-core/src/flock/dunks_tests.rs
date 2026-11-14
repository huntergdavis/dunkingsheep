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

#[test]
fn scheduled_consumption_uses_visible_box_not_busy_or_transcript() {
    use super::dunks::input_pending;
    assert_eq!(None, input_pending(None, "continue"));
    assert_eq!(None, input_pending(Some("working on a task"), "continue"));
    assert_eq!(Some(true), input_pending(Some("❯ continue"), "continue"));
    assert_eq!(
        Some(false),
        input_pending(Some("continue\nworking\n❯ "), "continue")
    );
    assert_eq!(
        Some(true),
        input_pending(Some("❯ prefix continue suffix"), "continue")
    );
    assert_eq!(
        Some(false),
        input_pending(Some("❯ unrelated hint"), "continue")
    );
    assert_eq!(
        Some(true),
        input_pending(Some("──────────\ncontinue\n──────────\nmodel"), "continue")
    );
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
    fn send_recovery_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.send_key(pane_id, key)
    }
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

struct RefusingOutboxWake;

impl OutboxWake for RefusingOutboxWake {
    fn ensure_worker(&self, _flock: &Arc<Flock>, _pane_id: &str) -> Result<(), DunkError> {
        Err(DunkError::Refused("outbox worker refused".into()))
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
fn queued_fire_does_not_suppress_non_io_snapshot_or_worker_errors() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "held {id}", None);
    let flock = Arc::new(flock);
    let mut env = fire_env(false);
    env.screen = "Press Enter to confirm or Esc to cancel".into();

    let error = flock
        .fire(
            "d1",
            &mut env,
            &NoopOutboxWake,
            Arc::new(OfflineSnapshotStore),
            || None,
        )
        .unwrap_err();
    assert_eq!("offline", error.to_string());
    assert_eq!(
        json!("Queued (typing)"),
        flock.dunks.get("d1").unwrap()["status"]
    );
    assert_eq!(1, super::sends::outbox_lock(&flock.outbox)["w1:p2"].len());

    let (_other_dir, other) = new_flock();
    load_one_fire_dunk(&other, "held {id}", None);
    let other = Arc::new(other);
    let error = other
        .fire(
            "d1",
            &mut env,
            &RefusingOutboxWake,
            Arc::new(CapturingSnapshotStore::default()),
            || None,
        )
        .unwrap_err();
    assert_eq!("outbox worker refused", error.to_string());
    assert_eq!(
        json!("Queued (typing)"),
        other.dunks.get("d1").unwrap()["status"]
    );
    assert_eq!(1, super::sends::outbox_lock(&other.outbox)["w1:p2"].len());
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
fn scheduled_cancellation_at_guard_prevents_the_delivery_attempt() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "scheduled text", Some(1));
    let flock = Arc::new(flock);
    let handle = flock.dunks.handle("d1").unwrap();
    let mut env = fire_env(false);
    let store = Arc::new(CapturingSnapshotStore::default());
    let result = flock.scheduled_dunk_send(&handle, &mut env, store.clone(), || {
        Some("scheduled dunk cancelled before attempt".into())
    });
    assert!(!result.0);
    assert!(env.typed.is_empty());
    assert!(env.keys.is_empty());
    assert!(env.attempts.is_empty());
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
    assert_eq!(0, store.calls.load(Ordering::SeqCst));
}

#[test]
fn scheduled_max_removal_cannot_remove_a_same_id_replacement() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "old", Some(1));
    let old = flock.dunks.handle("d1").unwrap();
    flock.dunks.remove("d1").unwrap();
    load_one_fire_dunk(&flock, "replacement", Some(1));
    let replacement = flock.dunks.handle("d1").unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    assert!(!flock.dunks.remove_handle(&old));
    assert_eq!(json!("replacement"), flock.dunks.get("d1").unwrap()["text"]);
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
}

#[test]
fn restored_running_timer_gets_five_second_grace_and_preserves_future_deadline() {
    let (_dir, flock) = new_flock();
    let before = crate::clock::now_s();
    let future = before + 600.0;
    flock._load_dunks(&map(json!({"dunkers": [
        {"id": "d1", "running": true, "target_pane_id": "w1:p1", "next_send_at": before - 10.0},
        {"id": "d2", "running": true, "target_pane_id": "w1:p1", "next_send_at": future},
        {"id": "d3", "running": false, "target_pane_id": "w1:p1"}
    ]})));
    let after = crate::clock::now_s();
    let restored = flock.dunks.get("d1").unwrap()["next_send_at"]
        .as_f64()
        .unwrap();
    assert!(restored >= before + 5.0 && restored <= after + 5.0);
    assert_eq!(
        json!(future),
        flock.dunks.get("d2").unwrap()["next_send_at"]
    );
    assert_eq!(2, flock.dunks.running_handles().len());
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

// ---- t4681: a scheduled dunk is never typed over a person's draft; it waits in the inbox ----

/// A bordered composer that holds text nobody in this flock sent.
fn foreign_draft_screen() -> String {
    "\u{2502} \u{276f} my unfinished draft\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n".to_string()
}

struct DunkInbox {
    pool: crate::db::ReadPool,
    store: Arc<crate::inbox_store::Store>,
    _dir: tempfile::TempDir,
    notices: Mutex<Vec<(String, String)>>,
    queued: bool,
}

impl DunkInbox {
    fn new(queued: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let pool = crate::db::ReadPool::open(store.path(), 1).unwrap();
        Self {
            pool,
            store,
            _dir: dir,
            notices: Mutex::new(Vec::new()),
            queued,
        }
    }
}

impl super::mailbox::MailboxEnv for DunkInbox {
    fn list_panes(&self) -> Vec<Value> {
        vec![json!({"pane_id": "w1:p2", "tab_label": "Agent", "label": "Agent"})]
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.list_panes()
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl super::commands_inbox::InboxEnv for DunkInbox {
    fn caller_role(&self, _: &str) -> Result<String, DunkError> {
        Ok("system".into())
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    /// Every mailbox of the target is configured silent, as the owner lane's are.
    fn inbox_silent_mailboxes(&self) -> Option<String> {
        Some("Agent,w1:p2".into())
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("fixture".into())
    }
    fn lane_panes(&self) -> Vec<String> {
        Vec::new()
    }
    fn pool(&self) -> &crate::db::ReadPool {
        &self.pool
    }
    fn store(&self) -> &crate::inbox_store::Store {
        &self.store
    }
    fn now(&self) -> f64 {
        100.0
    }
    fn record_metric(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: Option<&str>,
        _: Option<i64>,
        _: Option<i64>,
    ) {
    }
    fn pm_routing(&self) -> Vec<super::commands_inbox::RouteEntry> {
        Vec::new()
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn send_now(
        &self,
        pane_id: &str,
        text: &str,
        _: &str,
    ) -> Result<super::commands_inbox::Nudge, DunkError> {
        self.notices
            .lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        Ok(super::commands_inbox::Nudge {
            queued: self.queued,
            message: None,
        })
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, _: &str) -> Result<Value, DunkError> {
        Ok(json!([]))
    }
}

#[test]
fn a_scheduled_dunk_types_nothing_into_a_box_that_holds_a_persons_draft() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "scheduled text", None);
    let flock = Arc::new(flock);
    let handle = flock.dunks.handle("d1").unwrap();
    let mut env = fire_env(false);
    env.screen = foreign_draft_screen();
    let store = Arc::new(CapturingSnapshotStore::default());

    let (sent, ok, detail, blocker) = flock.scheduled_dunk_send(&handle, &mut env, store, || None);

    assert!(!sent, "{detail}");
    assert_eq!(None, ok);
    assert_eq!(Some("human_draft"), blocker.as_deref(), "{detail}");
    assert!(env.typed.is_empty(), "nothing is typed after the draft");
    assert!(env.keys.is_empty(), "no key submits the person's draft");
    assert_eq!(json!(0), flock.dunks.get("d1").unwrap()["send_count"]);
}

#[test]
fn a_dunk_held_by_a_draft_is_one_inbox_row_and_wakes_even_a_silent_mailbox_each_interval() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "inspect the coordinators", None);
    let handle = flock.dunks.handle("d1").unwrap();
    let inbox = DunkInbox::new(true);

    let first = flock.store_scheduled_dunk(&handle, &inbox).unwrap();
    let super::dunks::DunkStored::Stored { message_id, notice } = first else {
        panic!("the first store writes a row: {first:?}");
    };
    assert_eq!("queued", notice);
    {
        let notices = inbox.notices.lock().unwrap();
        assert_eq!(1, notices.len());
        assert_eq!("w1:p2", notices[0].0);
        assert!(notices[0].1.starts_with("[inbox] "), "{}", notices[0].1);
        assert!(
            !notices[0].1.contains("inspect the coordinators"),
            "the notice is one line about the mailbox, never the dunk text"
        );
    }
    let (summary, body) = crate::inbox::get(&inbox.pool, &message_id, 100.0).unwrap();
    assert_eq!("inspect the coordinators", body);
    assert_eq!(Some("dunk d1"), summary.sender.as_deref());
    assert_eq!(Some("dunk:d1"), summary.digest_id.as_deref());
    // Agent audience: the row is mail for the agent in the pane. It is not part of the owner
    // conversation window over that mailbox (the query the owner thread uses, which reads
    // owner-lane mailboxes too), while an ordinary digest row in the same mailbox is.
    assert!(
        inbox
            .store
            .conversation_across_visible(std::slice::from_ref(&summary.mailbox), None, None)
            .unwrap()
            .is_empty(),
        "a stored dunk never shows in the owner's conversation"
    );
    inbox
        .store
        .upsert_digest(crate::inbox_store::DigestUpsert::new(
            &summary.mailbox,
            "other-digest",
            "a row for people",
        ))
        .unwrap();
    assert_eq!(
        1,
        inbox
            .store
            .conversation_across_visible(std::slice::from_ref(&summary.mailbox), None, None)
            .unwrap()
            .len()
    );
    inbox
        .store
        .delete(&[inbox.store.digest_waiting("other-digest").unwrap().unwrap()])
        .unwrap();

    // The mailbox is configured silent; the wake above was sent all the same.
    assert!(
        crate::flock::commands_inbox::recipient_is_quiet(
            Some("Agent,w1:p2"),
            &summary.mailbox,
            "Agent",
            Some("w1:p2"),
        ),
        "the fixture mailbox {} is a silent one",
        summary.mailbox
    );

    // The next interval, still unread: no second row, but the lane is woken again.
    let again = flock.store_scheduled_dunk(&handle, &inbox).unwrap();
    assert_eq!(
        super::dunks::DunkStored::Waiting {
            message_id: message_id.clone(),
            notice: "queued".into()
        },
        again
    );
    assert_eq!(2, inbox.notices.lock().unwrap().len());
    assert_eq!(
        1,
        crate::inbox::count(&inbox.pool, &summary.mailbox, &[], 100.0)
            .unwrap()
            .unread,
        "still exactly one unread row for the dunk"
    );

    // Once the lane has read it, the next interval refreshes the same row and announces it.
    inbox
        .store
        .mark_read(std::slice::from_ref(&message_id), Some(101.0))
        .unwrap();
    let later = flock.store_scheduled_dunk(&handle, &inbox).unwrap();
    assert!(
        matches!(&later, super::dunks::DunkStored::Stored { message_id: id, .. } if *id == message_id),
        "{later:?}"
    );
    assert_eq!(3, inbox.notices.lock().unwrap().len());
}

#[test]
fn a_stored_dunk_counts_as_a_send_and_leaves_nothing_to_be_consumed() {
    let (_dir, flock) = new_flock();
    load_one_fire_dunk(&flock, "scheduled text", Some(1));
    flock.dunks.start("d1").unwrap();
    let handle = flock.dunks.handle("d1").unwrap();

    assert!(
        super::dunks::DunkList::timer_stored(&handle),
        "max_sends 1 is reached"
    );

    let row = flock.dunks.get("d1").unwrap();
    assert_eq!(json!(1), row["send_count"]);
    assert_eq!(json!(false), row["awaiting_consumption"]);
    assert_eq!(Value::Null, row["last_sent_text"]);
    assert!(row["status"]
        .as_str()
        .unwrap()
        .starts_with("Stored (inbox) "));
}
