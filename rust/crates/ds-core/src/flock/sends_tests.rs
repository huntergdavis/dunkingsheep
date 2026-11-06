//! Tests for `sends.rs`, ported by name from tests/test_busy_native_queue.py,
//! tests/test_codex_ambiguous_composer.py (the `_box_holds` / `send_blocker` halves) and
//! tests/test_dunk_core.py (the `_clear_to_send` and `_press_until_submitted` tests). The scripted
//! pane below is the `FakeHerdr` surface those tests drive. NOT ported (later slices):
//! `submit_text`/`send_guarded`/`delivery_state`, and the thread-timing assertions of the
//! clear-to-send tests (the settle wait is injected, so wall-clock is not asserted).

use super::commands_inbox::{InboxEnv, Nudge, RouteEntry};
use super::mailbox::MailboxEnv;
use super::sends::*;
use super::Flock;
use crate::db::ReadPool;
use crate::error::DunkError;
use crate::flock::panes::PaneInfoEnv;
use crate::inbox_store::Store;
use crate::terminal::screen::{box_text, AMBIGUOUS_COMPOSER};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

/// t3355 group D rows (t820 proof D): child module with its own file, see sends_tests/group_d.rs.
#[cfg(test)]
mod group_d;

#[derive(Default)]
struct Pane {
    screens: BTreeMap<String, String>,
    harness: BTreeMap<String, String>,
    statuses: BTreeMap<String, String>,
    gone: bool,
    federated: bool,
    keys: Vec<(String, String)>,
    after_key_screen: Option<String>,
    key_refusal: Option<String>,
    reads: RefCell<Vec<(String, bool)>>,
    on_read: RefCell<Option<Box<dyn FnMut()>>>,
    on_send_text: RefCell<Option<Box<dyn FnMut()>>>,
    screen_script: RefCell<VecDeque<Option<String>>>,
    status_script: RefCell<VecDeque<Option<String>>>,
    attempt_generation_script: RefCell<VecDeque<Option<String>>>,
    /// The generation read once the script is empty. `generation_after_send` replaces it when the
    /// terminal action (`send_text`) runs, as a pane swap does mid-attempt.
    generation: RefCell<Option<String>>,
    generation_after_send: RefCell<Option<String>>,
    now: f64,
    sleeps: Vec<f64>,
    remembered: BTreeMap<String, String>,
    submit_metrics: Vec<(String, String, usize, usize)>,
    panes: Vec<serde_json::Value>,
    hint: String,
    typed: Vec<(String, String)>,
    typed_attempts: Vec<(String, String, bool, Option<String>)>,
    receipt_ok: bool,
    events: Vec<&'static str>,
    pending_override: Option<String>,
    receipt_frames: Vec<Option<String>>,
    receipt_forward: Option<Box<ReceiptForward>>,
    send_refusal: Option<String>,
}

impl SendEnv for Pane {
    fn read_pane(&self, pane_id: &str, _lines: i64, ansi: bool) -> Option<String> {
        self.reads.borrow_mut().push((pane_id.to_string(), ansi));
        if let Some(callback) = self.on_read.borrow_mut().as_mut() {
            callback();
        }
        if let Some(screen) = self.screen_script.borrow_mut().pop_front() {
            return screen;
        }
        self.screens.get(pane_id).cloned()
    }
    fn pane_harness(&self, pane_id: &str) -> Option<String> {
        self.harness.get(pane_id).cloned()
    }
    fn session_process_gone(&self, _: &str) -> bool {
        self.gone
    }
    fn read_status(&self, pane_id: &str) -> Option<String> {
        if let Some(status) = self.status_script.borrow_mut().pop_front() {
            return status;
        }
        self.statuses.get(pane_id).cloned()
    }
    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.events.push("key");
        if let Some(reason) = &self.key_refusal {
            return (false, reason.clone());
        }
        self.keys.push((pane_id.to_string(), key.to_string()));
        if let Some(screen) = self.after_key_screen.take() {
            self.screens.insert(pane_id.to_string(), screen);
        }
        (true, "sent".to_string())
    }
    fn federated_input(&self) -> bool {
        self.federated
    }
}

impl SubmitEnv for Pane {
    fn sleep_s(&mut self, seconds: f64) {
        self.sleeps.push(seconds);
        self.now += seconds;
    }

    fn now_s(&mut self) -> f64 {
        self.now
    }

    fn send_submit_key_guarded(
        &mut self,
        pane_id: &str,
        fallback: &str,
        busy_hint: bool,
    ) -> (bool, String, Option<String>) {
        send_submit_key(self, pane_id, fallback, busy_hint)
    }

    fn remembered_submit_key(&self, pane_id: &str) -> Option<String> {
        self.remembered.get(pane_id).cloned()
    }

    fn remember_submit_key(&mut self, pane_id: &str, key: &str) {
        self.remembered.insert(pane_id.to_string(), key.to_string());
    }

    fn record_submit_attempt(
        &mut self,
        pane_id: &str,
        outcome: &str,
        attempts: usize,
        bytes: usize,
    ) {
        self.submit_metrics
            .push((pane_id.to_string(), outcome.to_string(), attempts, bytes));
    }
}

impl PaneInfoEnv for Pane {
    fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<serde_json::Value> {
        self.panes
            .iter()
            .find(|pane| pane.get("pane_id").and_then(serde_json::Value::as_str) == Some(pane_id))
            .cloned()
    }
}

impl FlushEnv for Pane {
    fn server_error_hint(&self) -> String {
        self.hint.clone()
    }
}

impl super::send_keys::KeyPressEnv for Pane {
    fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String) {
        for key in keys {
            self.keys.push((pane_id.to_string(), key.clone()));
        }
        (true, "sent".into())
    }

    fn pending_text(&mut self, pane_id: &str) -> Option<String> {
        if self.pending_override.is_some() {
            return self.pending_override.clone();
        }
        let screen = self.read_pane(pane_id, CONSUMPTION_READ_LINES, true)?;
        let (text, detail) = crate::terminal::screen::box_text(&screen);
        if detail.starts_with(crate::terminal::screen::AMBIGUOUS_COMPOSER) {
            None
        } else {
            Some(text)
        }
    }

    fn confirm_pending(&mut self, pane_id: &str, text: &str) -> (bool, String) {
        press_until_submitted(self, pane_id, text, false, false, StatusBaseline::ReadNow)
    }
}

impl SendPreparationEnv for Pane {
    fn send_text(&mut self, pane_id: &str, text: &str) -> (bool, String) {
        if let Some(callback) = self.on_send_text.borrow_mut().as_mut() {
            callback();
        }
        if let Some(next) = self.generation_after_send.borrow_mut().take() {
            *self.generation.borrow_mut() = Some(next);
        }
        self.events.push("typed");
        self.typed.push((pane_id.into(), text.into()));
        match &self.send_refusal {
            Some(reason) => (false, reason.clone()),
            None => (true, "typed".into()),
        }
    }

    fn attempt_generation(&self, pane_id: &str) -> Option<String> {
        if let Some(generation) = self.attempt_generation_script.borrow_mut().pop_front() {
            return generation;
        }
        if let Some(generation) = self.generation.borrow().clone() {
            return Some(generation);
        }
        pane_generation(self, pane_id)
    }

    fn persist_typed_attempt(
        &mut self,
        message: Option<&mut Message>,
        pane_id: &str,
        text: &str,
        press_enter: bool,
        generation: Option<&str>,
    ) -> bool {
        self.events.push("receipt");
        self.receipt_frames
            .push(message.as_deref().map(|message| message.id.clone()));
        self.typed_attempts.push((
            pane_id.into(),
            text.into(),
            press_enter,
            generation.map(str::to_owned),
        ));
        if let Some(forward) = self.receipt_forward.as_mut() {
            forward(message, pane_id, text, press_enter, generation)
        } else {
            self.receipt_ok
        }
    }
}

const P: &str = "w2:p1";

type ReceiptForward = dyn FnMut(Option<&mut Message>, &str, &str, bool, Option<&str>) -> bool;

fn real_receipt_forward(
    flock: std::sync::Arc<Flock>,
    pane: serde_json::Value,
) -> Box<ReceiptForward> {
    Box::new(move |message, pane_id, text, press_enter, generation| {
        let Some(message) = message else {
            // Python's low-level submit_text call has no message receipt context.
            return true;
        };
        assert_eq!(message.attempt_generation.as_deref(), generation);
        flock
            .persist_typed_attempt(message, pane_id, text, press_enter, &pane)
            .unwrap_or(false)
    })
}

struct WorkerInbox {
    pool: ReadPool,
    store: Store,
}

impl MailboxEnv for WorkerInbox {
    fn list_panes(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl InboxEnv for WorkerInbox {
    fn caller_role(&self, _: &str) -> Result<String, DunkError> {
        Ok("worker".into())
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("test".into())
    }
    fn lane_panes(&self) -> Vec<String> {
        Vec::new()
    }
    fn pool(&self) -> &ReadPool {
        &self.pool
    }
    fn store(&self) -> &Store {
        &self.store
    }
    fn now(&self) -> f64 {
        crate::clock::now_s()
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
    fn pm_routing(&self) -> Vec<RouteEntry> {
        Vec::new()
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn send_now(&self, _: &str, _: &str, _: &str) -> Result<Nudge, DunkError> {
        Ok(Nudge::default())
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, _: &str) -> Result<serde_json::Value, DunkError> {
        Ok(serde_json::json!([]))
    }
}

fn worker_inbox() -> (tempfile::TempDir, WorkerInbox) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
    let pool = ReadPool::open(store.path(), 1).unwrap();
    (dir, WorkerInbox { pool, store })
}

fn persistent_receipt_flock(dir: &std::path::Path) -> Flock {
    let flock = Flock::new(dir.join("launches.json"), dir, true);
    flock
        .attach_status_state(super::status_runtime::StatusState::at_start(
            "test",
            dir.join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    flock
}

fn receipt_store(dir: &std::path::Path) -> (std::sync::Arc<Store>, ReadPool) {
    let store = std::sync::Arc::new(
        Store::open(dir.join("fold-inbox.db"), dir.join("fold-blobs")).unwrap(),
    );
    let pool = ReadPool::open(store.path(), 1).unwrap();
    (store, pool)
}

fn queued_receipt(store: &Store, body: &str) -> String {
    let id = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "worker",
            body: Some(body),
            ..Default::default()
        })
        .unwrap();
    store
        .record_notify(&id, "queued", Some("awaiting notice"), None, None)
        .unwrap();
    id
}

fn receipt_state(pool: &ReadPool, id: &str) -> String {
    crate::inbox::status(pool, id, crate::clock::now_s())
        .unwrap()
        .notify_state
        .unwrap()
}

#[derive(Clone, Copy)]
struct FoldInterleaving {
    fold: bool,
    cancel: bool,
    replace: bool,
    same_created_at: bool,
}

struct FoldResult {
    first: String,
    folded: String,
    replacement_id: String,
    replacement_receipt: String,
    outcome: DeliveryOutcome,
    queued: Vec<Message>,
    generation: Option<String>,
    typed: Vec<(String, String)>,
    done: bool,
}

fn run_inflight_fold(interleaving: FoldInterleaving, send_ok: bool) -> FoldResult {
    let dir = tempfile::tempdir().unwrap();
    let (store, pool) = receipt_store(dir.path());
    let first = queued_receipt(&store, "first receipt");
    let folded = queued_receipt(&store, "folded receipt");
    let replacement_receipt = queued_receipt(&store, "replacement receipt");
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    flock.attach_stores(super::panes::Stores {
        inbox: Some(store.clone()),
        ..Default::default()
    });

    let mut message = Message::new("m-fold", P, "worker", "notice text", "notice");
    message.is_notice = true;
    message.created_at = 1000.0;
    message.notify_for = vec![first.clone()];
    message.press_enter = false;
    let waiter = message.done.clone();
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);

    let callback_flock = flock.clone();
    let callback_folded = folded.clone();
    let callback_replacement_receipt = replacement_receipt.clone();
    let callback_interleaving = interleaving;
    let mut pane = Pane::default();
    pane.attempt_generation_script
        .borrow_mut()
        .push_back(Some("original-generation".into()));
    pane.send_refusal = (!send_ok).then(|| "controlled terminal failure".into());
    pane.receipt_forward = Some(real_receipt_forward(
        flock.clone(),
        serde_json::json!({"pane_id": P}),
    ));
    pane.on_send_text.replace(Some(Box::new(move || {
        let mut outbox = outbox_lock(&callback_flock.outbox);
        let queue = outbox.get_mut(P).unwrap();
        if callback_interleaving.fold {
            assert!(queue[0].append_notify_for(&callback_folded));
            queue[0].text = "folded notice text".into();
        }
        if callback_interleaving.cancel {
            queue[0].status = "cancelled".into();
            queue.remove(0);
        }
        if callback_interleaving.replace {
            let mut replacement = Message::new("m-fold", P, "worker", "replacement", "replacement");
            replacement.is_notice = true;
            replacement.notify_for = vec![callback_replacement_receipt.clone()];
            replacement.attempt_generation = Some("replacement-generation".into());
            replacement.created_at = if callback_interleaving.same_created_at {
                1000.0
            } else {
                99.0
            };
            queue.clear();
            queue.push(replacement);
        }
    })));

    let outcome = flock.deliver_message("m-fold", 10.0, &mut pane, || None);
    let queued = flock
        .outbox
        .lock()
        .unwrap()
        .get(P)
        .cloned()
        .unwrap_or_default();
    let generation = {
        let history = flock.message_history.lock().unwrap();
        history
            .back()
            .and_then(|message| message.attempt_generation.clone())
    };
    FoldResult {
        first: receipt_state(&pool, &first),
        folded: receipt_state(&pool, &folded),
        replacement_id: replacement_receipt.clone(),
        replacement_receipt: receipt_state(&pool, &replacement_receipt),
        outcome,
        queued,
        generation,
        typed: pane.typed.clone(),
        done: waiter.is_set(),
    }
}

#[test]
fn fold_during_success_and_failure_settles_the_exact_receipt_frame() {
    for (send_ok, expected, outcome) in [
        (true, "sent", DeliveryOutcome::Sent),
        (false, "failed", DeliveryOutcome::Failed),
    ] {
        let result = run_inflight_fold(
            FoldInterleaving {
                fold: true,
                cancel: false,
                replace: false,
                same_created_at: false,
            },
            send_ok,
        );
        assert_eq!(outcome, result.outcome);
        assert_eq!(expected, result.first);
        assert_eq!(expected, result.folded);
        assert_eq!("queued", result.replacement_receipt);
        assert!(result.queued.is_empty());
        assert_eq!(Some("original-generation".into()), result.generation);
        assert_eq!(vec![(P.into(), "notice text".into())], result.typed);
        assert!(result.done);
    }
}

#[test]
fn fold_then_cancel_still_settles_the_actual_attempt_receipts() {
    let result = run_inflight_fold(
        FoldInterleaving {
            fold: true,
            cancel: true,
            replace: false,
            same_created_at: false,
        },
        true,
    );
    assert_eq!(DeliveryOutcome::Sent, result.outcome);
    assert_eq!("sent", result.first);
    assert_eq!("sent", result.folded);
    assert_eq!("queued", result.replacement_receipt);
    assert!(result.queued.is_empty());
    assert!(result.done);
}

#[test]
fn same_id_replacement_never_inherits_completion_even_with_matching_stamp() {
    for same_created_at in [false, true] {
        let result = run_inflight_fold(
            FoldInterleaving {
                fold: false,
                cancel: false,
                replace: true,
                same_created_at,
            },
            true,
        );
        assert_eq!(DeliveryOutcome::Sent, result.outcome);
        assert_eq!("sent", result.first);
        assert_eq!("queued", result.folded);
        assert_eq!("queued", result.replacement_receipt);
        assert_eq!(1, result.queued.len());
        assert_eq!("m-fold", result.queued[0].id);
        assert_eq!("replacement", result.queued[0].text);
        assert_eq!("queued", result.queued[0].status);
        assert_eq!(
            vec![result.replacement_id.clone()],
            result.queued[0].notify_for
        );
        assert_eq!(
            Some("replacement-generation".into()),
            result.queued[0].attempt_generation
        );
    }
}

#[test]
fn fold_cancel_and_same_id_replacement_settles_only_original_receipts() {
    for send_ok in [true, false] {
        let result = run_inflight_fold(
            FoldInterleaving {
                fold: true,
                cancel: true,
                replace: true,
                same_created_at: true,
            },
            send_ok,
        );
        assert_eq!(
            if send_ok {
                DeliveryOutcome::Sent
            } else {
                DeliveryOutcome::Failed
            },
            result.outcome
        );
        assert_eq!(if send_ok { "sent" } else { "failed" }, result.first);
        assert_eq!(if send_ok { "sent" } else { "failed" }, result.folded);
        assert_eq!("queued", result.replacement_receipt);
        assert_eq!(1, result.queued.len());
        assert_eq!("replacement", result.queued[0].text);
        assert_eq!("queued", result.queued[0].status);
        assert_eq!(
            Some("replacement-generation".into()),
            result.queued[0].attempt_generation
        );
    }
}

#[test]
fn guard_held_fold_stays_queued_and_cancel_before_start_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, pool) = receipt_store(dir.path());
    let first = queued_receipt(&store, "first");
    let folded = queued_receipt(&store, "folded");
    let flock = std::sync::Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    flock.attach_stores(super::panes::Stores {
        inbox: Some(store.clone()),
        ..Default::default()
    });
    let mut held = Message::new("m-held-fold", P, "worker", "notice", "notice");
    held.is_notice = true;
    held.notify_for = vec![first.clone()];
    held.press_enter = false;
    flock.outbox.lock().unwrap().insert(P.into(), vec![held]);
    let permission_flock = flock.clone();
    let permission_folded = folded.clone();
    let mut pane = Pane::default();
    assert_eq!(
        DeliveryOutcome::Held,
        flock.deliver_message("m-held-fold", 1.0, &mut pane, move || {
            let mut outbox = outbox_lock(&permission_flock.outbox);
            assert!(outbox.get_mut(P).unwrap()[0].append_notify_for(&permission_folded));
            Some("held for test".into())
        })
    );
    assert!(pane.typed.is_empty());
    assert_eq!("queued", receipt_state(&pool, &first));
    assert_eq!("queued", receipt_state(&pool, &folded));

    let mut cancelled = Message::new("m-cancel-before", P, "worker", "cancelled", "test");
    cancelled.status = "cancelled".into();
    cancelled.notify_for = vec![first.clone()];
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![cancelled]);
    let guard_calls = std::cell::Cell::new(0);
    assert_eq!(
        DeliveryOutcome::AlreadyDecided,
        flock.deliver_message("m-cancel-before", 2.0, &mut pane, || {
            guard_calls.set(guard_calls.get() + 1);
            None
        })
    );
    assert_eq!(0, guard_calls.get());
    assert!(pane.typed.is_empty());
    assert_eq!("queued", receipt_state(&pool, &first));
}

struct NoopOutboxWake;
impl OutboxWake for NoopOutboxWake {
    fn ensure_worker(&self, _: &std::sync::Arc<Flock>, _: &str) -> Result<(), DunkError> {
        Ok(())
    }
}

struct NoopSnapshotStore;
impl SnapshotStore for NoopSnapshotStore {
    fn save(&self, _: &Flock) -> Result<(), DunkError> {
        Ok(())
    }
}

#[test]
fn immediate_send_receipt_uses_the_exact_nonqueued_message() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "worker"})],
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };

    let result = flock
        .send_now(
            &serde_json::json!(P),
            "same text",
            DirectSendOptions {
                hold_while_typing: false,
                ..DirectSendOptions::default()
            },
            DirectTextMetadata {
                press_enter: false,
                ..DirectTextMetadata::default()
            },
            &mut pane,
            &NoopOutboxWake,
            &NoopSnapshotStore,
            || None,
        )
        .unwrap();

    let message_id = result["message_id"].as_str().unwrap();
    assert_eq!("sent", result["status"]);
    assert!(flock.outbox_snapshot().is_empty());
    assert_eq!(vec![Some(message_id.to_owned())], pane.receipt_frames);
    assert_eq!(
        message_id,
        flock.stuck_candidates.lock().unwrap()[P].message_id
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!(message_id, snapshot["stuck_candidates"][P]["message_id"]);
}

#[test]
fn queued_identical_text_receipts_keep_each_actual_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    let mut first = Message::new("m-first", P, "worker", "identical", "test");
    first.hold_while_typing = false;
    first.press_enter = false;
    let mut second = Message::new("m-second", P, "worker", "identical", "test");
    second.hold_while_typing = false;
    second.press_enter = false;
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![first, second]);

    let mut pane = Pane {
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };
    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message("m-first", 10.0, &mut pane, || None)
    );
    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message("m-second", 11.0, &mut pane, || None)
    );

    assert_eq!(
        vec![Some("m-first".into()), Some("m-second".into())],
        pane.receipt_frames
    );
    assert_eq!(
        vec![Some("m-first".into()), Some("m-second".into())],
        flock
            .message_history
            .lock()
            .unwrap()
            .iter()
            .map(|message| Some(message.id.clone()))
            .collect::<Vec<_>>()
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!("m-second", snapshot["stuck_candidates"][P]["message_id"]);
}

#[test]
fn captured_none_generation_is_forwarded_without_a_second_read() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut captured = Message::new("m-captured-generation", P, "worker", "first", "test");
    captured.hold_while_typing = false;
    captured.press_enter = false;
    captured.attempt_generation = Some("captured-generation".into());
    let mut message = Message::new("m-none-generation", P, "worker", "body", "test");
    message.hold_while_typing = false;
    message.press_enter = false;
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![captured, message]);
    let mut pane = Pane {
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };

    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message("m-captured-generation", 9.0, &mut pane, || None)
    );
    assert_eq!(Some("captured-generation".into()), pane.typed_attempts[0].3);
    pane.attempt_generation_script
        .borrow_mut()
        .extend([None, Some("replacement-generation".into())]);
    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message("m-none-generation", 10.0, &mut pane, || None)
    );
    assert_eq!(None, pane.typed_attempts[1].3);
    assert_eq!(
        Some("replacement-generation".into()),
        pane.attempt_generation_script
            .borrow()
            .front()
            .cloned()
            .flatten()
    );
    assert_eq!(
        vec![
            Some("m-captured-generation".into()),
            Some("m-none-generation".into())
        ],
        pane.receipt_frames
    );
}

#[test]
fn low_level_text_submission_has_no_message_frame_or_recovery_authority() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    let mut pane = Pane {
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };
    let guard = flock.typing_guard(P).unwrap();
    let action = submit_text(&flock, &mut pane, P, "unframed", false, false, Some(&guard));
    drop(guard);

    assert!(action.ok);
    assert_eq!(vec![None], pane.receipt_frames);
    assert!(flock.stuck_candidates.lock().unwrap().is_empty());
    assert!(!dir.path().join("state.json").exists());

    pane.panes = vec![serde_json::json!({"pane_id": P, "tab_label": "worker"})];
    let result = flock
        .send_now(
            &serde_json::json!(P),
            "framed after low-level send",
            DirectSendOptions {
                hold_while_typing: false,
                ..DirectSendOptions::default()
            },
            DirectTextMetadata {
                press_enter: false,
                ..DirectTextMetadata::default()
            },
            &mut pane,
            &NoopOutboxWake,
            &NoopSnapshotStore,
            || None,
        )
        .unwrap();
    assert_eq!("m1", result["message_id"]);
    assert_eq!(vec![None, Some("m1".into())], pane.receipt_frames);
    assert_eq!("m1", flock.stuck_candidates.lock().unwrap()[P].message_id);
}

#[test]
fn failed_real_receipt_snapshot_prevents_the_first_enter() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"file").unwrap();
    let flock = std::sync::Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        true,
    ));
    flock
        .attach_status_state(super::status_runtime::StatusState::at_start(
            "test",
            blocker.join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "worker"})],
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };

    let result = flock
        .send_now(
            &serde_json::json!(P),
            "do not press enter",
            DirectSendOptions {
                hold_while_typing: false,
                ..DirectSendOptions::default()
            },
            DirectTextMetadata::default(),
            &mut pane,
            &NoopOutboxWake,
            &NoopSnapshotStore,
            || None,
        )
        .unwrap();

    assert_eq!("failed", result["status"]);
    assert_eq!(vec![Some("m1".into())], pane.receipt_frames);
    assert!(pane.keys.iter().all(|(_, key)| key != "Enter"));
}

#[test]
fn outbox_worker_accepts_distinct_send_and_inbox_environment_types() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let (_inbox_dir, inbox) = worker_inbox();
    let mut pane = Pane::default();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let mut reporter = super::loops_stuck::StalePanelReporter::new();
    outbox_worker(
        &flock,
        &mut pane,
        OutboxWorkerContext {
            inbox: &inbox,
            pane_id: P,
            stopped: &stopped,
            reporter: &mut reporter,
            wait: &|_| true,
            permission: || None,
        },
    );
    assert!(flock.outbox_snapshot().is_empty());
}

#[test]
fn direct_key_send_serializes_keys_and_immediate_mode_delivers_without_queueing() {
    use std::sync::Arc;

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            panic!("immediate send must not start an outbox worker")
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "Agent A"})],
        ..Pane::default()
    };
    let result = flock
        .send_keys_now(
            &serde_json::json!(P),
            &["Escape".into(), "".into()],
            DirectSendOptions {
                hold_while_typing: false,
                ..DirectSendOptions::default()
            },
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap();

    assert_eq!("Escape", result["text"]);
    assert_eq!("send_keys", result["source"]);
    assert_eq!("sent", result["status"]);
    assert_eq!("Escape", pane.keys[0].1);
    assert!(flock.outbox_snapshot().is_empty());
}

#[test]
fn direct_key_send_preserves_whitespace_inside_one_argument_and_multiple_arguments() {
    use std::sync::Arc;

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            panic!("immediate send must not start an outbox worker")
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "Agent A"})],
        ..Pane::default()
    };
    let options = DirectSendOptions {
        hold_while_typing: false,
        ..DirectSendOptions::default()
    };

    let single = flock
        .send_keys_now(
            &serde_json::json!(P),
            &["ctrl+c Enter".into()],
            options.clone(),
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap();
    assert_eq!("sent", single["status"]);
    assert_eq!(vec![(P.to_owned(), "ctrl+c Enter".to_owned())], pane.keys);

    pane.keys.clear();
    let multiple = flock
        .send_keys_now(
            &serde_json::json!(P),
            &["ctrl+c".into(), "Enter".into()],
            options,
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap();
    assert_eq!("sent", multiple["status"]);
    assert_eq!(
        vec![
            (P.to_owned(), "ctrl+c".to_owned()),
            (P.to_owned(), "Enter".to_owned())
        ],
        pane.keys
    );
}

#[test]
fn queued_key_send_keeps_live_key_argument_boundaries() {
    use std::sync::Arc;

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "Agent A"})],
        ..Pane::default()
    };
    let queued = flock
        .send_keys_now(
            &serde_json::json!(P),
            &["ctrl+c Enter".into()],
            DirectSendOptions::default(),
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap();
    assert_eq!("queued", queued["status"]);
    assert_eq!("ctrl+c Enter", flock.outbox_snapshot()[P][0].text);

    // Deliver through the same queue-head frame; skip the unrelated typing-settle gate here.
    flock.outbox.lock().unwrap().get_mut(P).unwrap()[0].hold_while_typing = false;
    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message(
            queued["message_id"].as_str().unwrap(),
            100.0,
            &mut pane,
            || None
        )
    );
    assert_eq!(vec![(P.to_owned(), "ctrl+c Enter".to_owned())], pane.keys);
}

#[test]
fn restored_key_message_uses_legacy_whitespace_split() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut live = Message::new("m-restored-keys", P, "Agent A", "ctrl+c Enter", "send_keys");
    live.kind = "keys".into();
    let snapshot = message_snapshot(&live);
    assert!(snapshot.get("key_arguments").is_none());
    let mut restored = message_from_snapshot(&snapshot).unwrap();
    restored.hold_while_typing = false;
    assert_eq!("keys", restored.kind);
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![restored]);

    let mut pane = Pane::default();
    assert_eq!(
        DeliveryOutcome::Sent,
        flock.deliver_message("m-restored-keys", 100.0, &mut pane, || None)
    );
    assert_eq!(
        vec![
            (P.to_owned(), "ctrl+c".to_owned()),
            (P.to_owned(), "Enter".to_owned())
        ],
        pane.keys
    );
}

#[test]
fn direct_text_send_preserves_metadata_and_queues_by_default() {
    use std::sync::Arc;

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "Agent A"})],
        ..Pane::default()
    };
    let result = flock
        .send_now(
            &serde_json::json!(P),
            "[inbox] hello",
            DirectSendOptions::default(),
            DirectTextMetadata {
                press_enter: false,
                notify_for: Some("i42"),
                source: "test",
                ..DirectTextMetadata::default()
            },
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap();

    assert_eq!("queued", result["status"]);
    let queued = flock.outbox_snapshot();
    let message = &queued[P][0];
    assert_eq!("test", message.source);
    assert!(!message.press_enter);
    assert!(message.is_notice);
    assert_eq!(vec!["i42"], message.notify_for);
}

#[test]
fn completion_waiter_timeout_is_bounded_and_observes_release() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let message = Message::new("m1", P, "Agent A", "hello", "test");
    assert!(!message.done.wait_timeout(std::time::Duration::ZERO));
    flock.record_delivery_outcome(&message, true, None, "ok", None);
    assert!(message.done.wait_timeout(std::time::Duration::ZERO));
}

#[test]
fn direct_send_methods_reject_blank_text_and_key_sequences() {
    use std::sync::Arc;

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut pane = Pane {
        panes: vec![serde_json::json!({"pane_id": P, "tab_label": "Agent A"})],
        ..Pane::default()
    };
    let text_error = flock
        .send_now(
            &serde_json::json!(P),
            "  ",
            DirectSendOptions::default(),
            DirectTextMetadata::default(),
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap_err();
    assert!(text_error.to_string().contains("text is required"));
    let keys_error = flock
        .send_keys_now(
            &serde_json::json!(P),
            &["  ".into()],
            DirectSendOptions::default(),
            &mut pane,
            &Wake,
            &Store,
            || None,
        )
        .unwrap_err();
    assert!(keys_error.to_string().contains("keys are required"));
}

// Single producer makes these lock probes a deterministic proof: wake runs while enqueue owns
// the lock, then save runs after enqueue releases it. The concurrent test below checks only
// producer/FIFO/callback counts because cross-producer contention is legitimate there.
#[test]
fn enqueue_appends_starts_worker_then_saves_the_queued_message() {
    use std::sync::{Arc, Mutex, TryLockError};

    #[derive(Default)]
    struct Events {
        order: Mutex<Vec<&'static str>>,
        saved: Mutex<Vec<Outbox>>,
    }

    impl OutboxWake for Events {
        fn ensure_worker(
            &self,
            flock: &Arc<Flock>,
            pane_id: &str,
        ) -> Result<(), crate::error::DunkError> {
            assert!(matches!(
                flock.outbox.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            assert_eq!("pane-a", pane_id);
            self.order.lock().unwrap().push("wake");
            Ok(())
        }
    }

    impl SnapshotStore for Events {
        fn save(&self, flock: &Flock) -> Result<(), crate::error::DunkError> {
            let outbox = flock.outbox.try_lock().map_err(|_| {
                crate::error::DunkError::Refused("outbox lock held during snapshot save".into())
            })?;
            self.saved.lock().unwrap().push(outbox.clone());
            self.order.lock().unwrap().push("save");
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let mut message = flock
        .new_message("pane-a", "Agent A", "check the queue", "t1944")
        .unwrap();
    message.kind = "keys".into();
    message.hold_while_typing = false;
    message.press_enter = false;
    message.is_notice = true;
    message.notify_for = vec!["i7".into(), "i8".into()];
    message.created_at = 1234.5;
    let expected = message.clone();
    let events = Arc::new(Events::default());

    flock
        .enqueue(message, events.as_ref(), events.as_ref())
        .unwrap();

    assert_eq!(vec!["wake", "save"], *events.order.lock().unwrap());
    let snapshots = events.saved.lock().unwrap();
    assert_eq!(1, snapshots.len());
    let saved = &snapshots[0]["pane-a"][0];
    assert_eq!(expected.id, saved.id);
    assert_eq!(expected.source, saved.source);
    assert_eq!(expected.kind, saved.kind);
    assert_eq!(expected.hold_while_typing, saved.hold_while_typing);
    assert_eq!(expected.press_enter, saved.press_enter);
    assert_eq!(expected.is_notice, saved.is_notice);
    assert_eq!(expected.notify_for, saved.notify_for);
    assert_eq!(expected.created_at, saved.created_at);
}

#[test]
fn enqueue_keeps_fifo_order_and_allocates_unique_ids_for_concurrent_producers() {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // Lock ownership is proved by the deterministic single-producer callback test above. Here,
    // another producer may legitimately hold the queue while this producer is in `save`, so
    // probing `try_lock` would conflate unrelated contention with enqueue's own lock lifetime.
    struct Wake(AtomicUsize);
    impl OutboxWake for Wake {
        fn ensure_worker(&self, _: &Arc<Flock>, _: &str) -> Result<(), crate::error::DunkError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct Store(AtomicUsize);
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let wake = Arc::new(Wake(AtomicUsize::new(0)));
    let store = Arc::new(Store(AtomicUsize::new(0)));
    let mut producers = Vec::new();
    for producer in 0..4 {
        let flock = flock.clone();
        let wake = wake.clone();
        let store = store.clone();
        producers.push(std::thread::spawn(move || {
            let mut ids = Vec::new();
            for sequence in 0..8 {
                let message = flock
                    .new_message(
                        "pane-a",
                        "Agent A",
                        &format!("{producer}:{sequence}"),
                        "test",
                    )
                    .unwrap();
                ids.push(message.id.clone());
                flock
                    .enqueue(message, wake.as_ref(), store.as_ref())
                    .unwrap();
            }
            ids
        }));
    }
    let producer_ids: Vec<Vec<String>> = producers
        .into_iter()
        .map(|producer| producer.join().unwrap())
        .collect();

    let queue = flock.outbox_snapshot();
    let messages = &queue["pane-a"];
    assert_eq!(32, messages.len());
    assert_eq!(32, wake.0.load(Ordering::SeqCst));
    assert_eq!(32, store.0.load(Ordering::SeqCst));
    let ids: HashSet<&str> = messages.iter().map(|message| message.id.as_str()).collect();
    assert_eq!(32, ids.len());
    for sequence in producer_ids {
        let positions: Vec<usize> = sequence
            .iter()
            .map(|id| {
                messages
                    .iter()
                    .position(|message| &message.id == id)
                    .unwrap()
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn outbox_worker_pool_registers_one_worker_per_pane_and_joins_on_stop() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Runner {
        started: AtomicUsize,
        active: AtomicUsize,
    }
    impl OutboxWorkerRunner for Runner {
        fn run(&self, _flock: Arc<Flock>, _pane_id: &str, stopped: &std::sync::atomic::AtomicBool) {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            while !stopped.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let runner = Arc::new(Runner::default());
    let pool = OutboxWorkerPool::new(runner.clone());
    let store = Store;
    for (pane, text) in [("pane-a", "one"), ("pane-a", "two"), ("pane-b", "other")] {
        let message = flock.new_message(pane, pane, text, "test").unwrap();
        flock.enqueue(message, &pool, &store).unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while runner.active.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(2, runner.started.load(Ordering::SeqCst));
    assert_eq!(2, runner.active.load(Ordering::SeqCst));

    pool.stop_outbox_workers(&flock);

    assert_eq!(0, runner.active.load(Ordering::SeqCst));
}

#[test]
fn outbox_worker_pool_reaps_completed_handles_between_idle_bursts() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct DrainingRunner(AtomicUsize);
    impl OutboxWorkerRunner for DrainingRunner {
        fn run(&self, flock: Arc<Flock>, pane_id: &str, _: &std::sync::atomic::AtomicBool) {
            self.0.fetch_add(1, Ordering::SeqCst);
            outbox_lock(&flock.outbox).shift_remove(pane_id);
        }
    }
    struct Store;
    impl SnapshotStore for Store {
        fn save(&self, _: &Flock) -> Result<(), crate::error::DunkError> {
            Ok(())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path(),
        false,
    ));
    let runner = Arc::new(DrainingRunner::default());
    let pool = OutboxWorkerPool::new(runner.clone());
    let store = Store;

    for burst in 0..8 {
        let message = flock
            .new_message(P, "pane", &format!("burst {burst}"), "test")
            .unwrap();
        flock.enqueue(message, &pool, &store).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let (retired, finished) = pool.retired_worker_counts_for_test();
            if retired > 0 && retired == finished {
                assert!(
                    retired <= 1,
                    "completed worker handles accumulated: {retired}"
                );
                break;
            }
            assert!(Instant::now() < deadline, "worker did not retire");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    assert_eq!(8, runner.0.load(Ordering::SeqCst));
    pool.stop_outbox_workers(&flock);
    assert_eq!((0, 0), pool.retired_worker_counts_for_test());
}

fn busy_codex() -> Pane {
    let mut pane = Pane::default();
    pane.harness.insert(P.into(), "codex".into());
    pane.statuses.insert(P.into(), "working".into());
    pane.screens.insert(
        P.into(),
        "\u{2022} Working (1m \u{b7} esc to interrupt)\n\u{203a} Ask Codex to do anything\n".into(),
    );
    pane
}

fn submit_live_screen() -> String {
    hint_screen()
}

fn submit_pending_screen(text: &str) -> String {
    format!("\u{2502} \u{276f} {text}\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n")
}

fn submit_modal_screen() -> String {
    "Press Enter to confirm or Esc to cancel\n".to_string()
}

fn submit_pane(status: &str) -> Pane {
    let mut pane = Pane::default();
    pane.harness.insert(P.into(), "claude".into());
    pane.statuses.insert(P.into(), status.into());
    pane.screen_script
        .borrow_mut()
        .push_back(Some(submit_live_screen())); // dead-session check
    pane
}

fn queue_submit_attempt(
    pane: &Pane,
    key_screen: Option<String>,
    hold_reads: &[Option<bool>],
    text: &str,
) {
    if let Some(screen) = key_screen {
        pane.screen_script.borrow_mut().push_back(Some(screen));
    }
    for hold in hold_reads {
        pane.screen_script.borrow_mut().push_back(match hold {
            Some(true) => Some(submit_pending_screen(text)),
            Some(false) => Some(submit_live_screen()),
            None => None,
        });
    }
}

fn repeated_hold(hold: Option<bool>) -> [Option<bool>; SUBMIT_POLL_S.len()] {
    [hold; SUBMIT_POLL_S.len()]
}

fn submit(pane: &mut Pane, text: &str, baseline: StatusBaseline) -> (bool, String) {
    press_until_submitted(pane, P, text, false, false, baseline)
}

fn queue_clear_confirmation(pane: &Pane, text: &str) {
    queue_submit_attempt(
        pane,
        Some(submit_live_screen()),
        &[Some(false), Some(false), Some(false)],
        text,
    );
}

fn queue_stuck_attempt(pane: &Pane, text: &str) {
    queue_submit_attempt(
        pane,
        Some(submit_live_screen()),
        &repeated_hold(Some(true)),
        text,
    );
}

mod press_until_submitted_tests {
    use super::*;

    #[test]
    fn test_one_enter_is_enough_when_it_works() {
        let mut pane = submit_pane("idle");
        queue_clear_confirmation(&pane, "hello");

        assert_eq!(
            (true, "sent".to_string()),
            submit(&mut pane, "hello", StatusBaseline::ReadNow)
        );
        assert_eq!(vec![(P.to_string(), "Enter".to_string())], pane.keys);
        assert_eq!(
            vec![(P.to_string(), "ok".to_string(), 1, 5)],
            pane.submit_metrics
        );
    }

    #[test]
    fn test_a_busy_lane_confirms_by_the_turn_it_started() {
        let mut pane = submit_pane("idle");
        pane.status_script
            .borrow_mut()
            .extend([Some("idle".into()), Some("working".into())]);
        queue_stuck_attempt(&pane, "a nudge");

        let (ok, detail) = submit(&mut pane, "a nudge", StatusBaseline::ReadNow);
        assert!(ok, "{detail}");
        assert_eq!("sent (confirmed by the turn it started)", detail);
        assert_eq!(1, pane.keys.len());
        assert_eq!("ok-turn", pane.submit_metrics[0].1);
    }

    #[test]
    fn test_a_composer_that_needs_carriage_return_still_submits() {
        let mut pane = submit_pane("idle");
        queue_stuck_attempt(&pane, "a nudge");
        queue_stuck_attempt(&pane, "a nudge");
        queue_clear_confirmation(&pane, "a nudge");

        let (ok, detail) = submit(&mut pane, "a nudge", StatusBaseline::ReadNow);
        assert!(ok, "{detail}");
        assert_eq!("sent (C-m x3)", detail);
        assert_eq!(
            vec!["Enter", "Enter", "C-m"],
            pane.keys
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(Some(&"C-m".to_string()), pane.remembered.get(P));
    }

    #[test]
    fn test_a_pane_that_needs_two_enters_still_submits() {
        let mut pane = submit_pane("idle");
        queue_stuck_attempt(&pane, "a long message");
        queue_clear_confirmation(&pane, "a long message");

        assert_eq!(
            (true, "sent (Enter x2)".to_string()),
            submit(&mut pane, "a long message", StatusBaseline::ReadNow)
        );
        assert_eq!(2, pane.keys.len());
        assert_eq!(Some(&"Enter".to_string()), pane.remembered.get(P));
        assert_eq!(2, pane.submit_metrics[0].2);
    }

    #[test]
    fn test_a_box_that_reads_clear_then_fills_again_is_not_counted_as_sent() {
        let mut pane = submit_pane("idle");
        queue_submit_attempt(
            &pane,
            Some(submit_live_screen()),
            &[Some(false), Some(true)],
            "fragile",
        );
        for _ in 0..SUBMIT_ATTEMPTS - 1 {
            queue_stuck_attempt(&pane, "fragile");
        }

        let (ok, detail) = submit(&mut pane, "fragile", StatusBaseline::ReadNow);
        assert!(!ok);
        assert!(detail.contains("did not submit"), "{detail}");
        assert_eq!(SUBMIT_ATTEMPTS, pane.keys.len());
        assert!(pane.submit_metrics.is_empty());
    }

    #[test]
    fn test_delayed_redraw_after_two_clear_reads_is_not_sent() {
        let mut pane = submit_pane("idle");
        queue_submit_attempt(
            &pane,
            Some(submit_live_screen()),
            &[Some(false), Some(false), Some(true)],
            "delayed",
        );
        for _ in 0..SUBMIT_ATTEMPTS - 1 {
            queue_stuck_attempt(&pane, "delayed");
        }

        let (ok, detail) = submit(&mut pane, "delayed", StatusBaseline::ReadNow);
        assert!(!ok);
        assert!(detail.contains("did not submit"), "{detail}");
        assert_eq!(SUBMIT_ATTEMPTS, pane.keys.len());
    }

    #[test]
    fn test_unreadable_box_does_not_claim_success() {
        let mut pane = submit_pane("idle");
        for _ in 0..SUBMIT_ATTEMPTS {
            queue_submit_attempt(
                &pane,
                Some(submit_live_screen()),
                &repeated_hold(None),
                "unconfirmed",
            );
        }

        let (ok, detail) = submit(&mut pane, "unconfirmed", StatusBaseline::ReadNow);
        assert!(!ok);
        assert_eq!(
        format!("submission unconfirmed: input box unreadable or changing after {SUBMIT_ATTEMPTS} presses; the lane may or may not have taken it - check the pane before resending"),
        detail
    );
        assert_eq!(SUBMIT_ATTEMPTS, pane.keys.len());
    }

    #[test]
    fn test_text_that_never_submits_is_reported_as_failed_not_sent() {
        let mut pane = submit_pane("idle");
        for _ in 0..SUBMIT_ATTEMPTS {
            queue_stuck_attempt(&pane, "stubborn");
        }

        let (ok, detail) = submit(&mut pane, "stubborn", StatusBaseline::ReadNow);
        assert!(!ok);
        assert_eq!(
        format!("text did not submit after {SUBMIT_ATTEMPTS} Enter presses; it is still in the pane's input box"),
        detail
    );
        assert_eq!(
            vec!["Enter", "Enter", "C-m", "Enter"],
            pane.keys
                .iter()
                .map(|(_, key)| key.as_str())
                .collect::<Vec<_>>()
        );
        assert!(pane.submit_metrics.is_empty());
    }

    #[test]
    fn test_a_dead_agent_session_is_refused_before_any_enter() {
        let mut pane = Pane::default();
        pane.screen_script.borrow_mut().push_back(Some(
            "\u{25a0} Server connection could not be restored\n".into(),
        ));

        let (ok, detail) = submit(&mut pane, "a message", StatusBaseline::ReadNow);
        assert!(!ok);
        assert!(detail.starts_with("agent session is dead"), "{detail}");
        assert!(pane.keys.is_empty());
    }

    #[test]
    fn submit_ladder_dialog_opening_before_retry_gets_no_key() {
        let mut pane = submit_pane("idle");
        queue_stuck_attempt(&pane, "a message");
        queue_submit_attempt(&pane, Some(submit_modal_screen()), &[], "a message");

        let (ok, detail) = submit(&mut pane, "a message", StatusBaseline::ReadNow);
        assert!(!ok);
        assert_eq!("submission held: a dialog opened before the key", detail);
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn test_a_slash_command_that_opens_a_panel_counts_as_submitted() {
        let mut pane = submit_pane("idle");
        queue_submit_attempt(&pane, Some(submit_live_screen()), &[], "/usage");
        for _ in 0..3 {
            pane.screen_script
                .borrow_mut()
                .push_back(Some(submit_modal_screen()));
        }

        let (ok, detail) = submit(&mut pane, "/usage", StatusBaseline::ReadNow);
        assert!(ok, "{detail}");
        assert_eq!("sent", detail);
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn first_pressed_does_not_send_a_duplicate_key() {
        let mut pane = submit_pane("idle");
        pane.screen_script.borrow_mut().extend([
            Some(submit_live_screen()),
            Some(submit_live_screen()),
            Some(submit_live_screen()),
        ]);

        assert_eq!(
            (true, "sent".to_string()),
            press_until_submitted(
                &mut pane,
                P,
                "already pressed",
                true,
                false,
                StatusBaseline::Captured(Some("idle".into()))
            )
        );
        assert!(pane.keys.is_empty());
    }

    #[test]
    fn captured_unreadable_baseline_does_not_confirm_turn() {
        let mut pane = submit_pane("working");
        pane.status_script
            .borrow_mut()
            .push_back(Some("working".into()));
        for _ in 0..SUBMIT_ATTEMPTS {
            queue_stuck_attempt(&pane, "captured unreadable");
        }

        let (ok, detail) = press_until_submitted(
            &mut pane,
            P,
            "captured unreadable",
            false,
            false,
            StatusBaseline::Captured(None),
        );
        assert!(!ok);
        assert!(!detail.contains("confirmed by the turn"), "{detail}");
        assert!(detail.contains("did not submit"), "{detail}");
        assert_eq!(SUBMIT_ATTEMPTS, pane.keys.len());
        assert_eq!(
            1,
            pane.status_script.borrow().len(),
            "unreadable baseline is not upgraded by a later read"
        );
    }

    #[test]
    fn captured_working_baseline_does_not_confirm_an_existing_turn() {
        let mut pane = submit_pane("working");
        for _ in 0..SUBMIT_ATTEMPTS {
            queue_stuck_attempt(&pane, "old turn");
        }

        let (ok, detail) = press_until_submitted(
            &mut pane,
            P,
            "old turn",
            false,
            false,
            StatusBaseline::Captured(Some("working".into())),
        );
        assert!(!ok);
        assert!(
            detail.contains("already working before this send"),
            "{detail}"
        );
        assert_eq!(SUBMIT_ATTEMPTS, pane.keys.len());
    }
}

mod guarded_preparation_tests {
    use super::*;

    fn flock() -> Flock {
        Flock::new(
            "/tmp/ds-sends-test-launches.json",
            "/tmp/ds-sends-test-lanes.json",
            false,
        )
    }

    #[test]
    fn guarded_text_captures_and_persists_before_first_key_then_uses_short_rounds() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.receipt_ok = true;
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(submit_live_screen()),           // dead-session check
            Some(submit_live_screen()),           // pre-type turn indicator
            Some(submit_pending_screen("hello")), // first submit key check
            Some(submit_live_screen()),           // short confirmation
            Some(submit_live_screen()),           // first verification
            Some(submit_live_screen()),           // second verification
        ]);
        let guard = flock.typing_guard(P).unwrap();

        let mut action = submit_text(&flock, &mut pane, P, "hello", true, false, Some(&guard));
        drop(guard);
        confirm_send_action(&mut pane, &mut action);

        assert_eq!((true, "sent".to_string()), (action.ok, action.detail));

        assert_eq!(vec!["typed", "receipt", "key"], pane.events);
        assert_eq!(
            vec![(P.to_string(), "hello".to_string(), true, None)],
            pane.typed_attempts
        );
        assert_eq!(vec![(P.to_string(), "Enter".to_string())], pane.keys);
        assert_eq!(vec![SUBMIT_POLL_S[0], 0.2, 0.4], pane.sleeps);
    }

    #[test]
    fn press_enter_false_records_typed_attempt_without_a_key_or_confirmation() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.receipt_ok = true;
        pane.screen_script
            .borrow_mut()
            .extend([Some(submit_live_screen()), Some(submit_live_screen())]);
        let guard = flock.typing_guard(P).unwrap();

        let action = submit_text(&flock, &mut pane, P, "draft", false, false, Some(&guard));
        drop(guard);

        assert_eq!((true, "sent".to_string()), (action.ok, action.detail));

        assert_eq!(vec!["typed", "receipt"], pane.events);
        assert!(pane.keys.is_empty());
        assert!(pane.sleeps.is_empty());
    }

    #[test]
    fn submit_text_refuses_a_dead_session_before_box_preflight_can_press_enter() {
        let flock = flock();
        let mut pane = Pane::default();
        pane.screen_script.borrow_mut().push_back(Some(
            "\u{25a0} Server connection could not be restored\n".into(),
        ));
        let guard = flock.typing_guard(P).unwrap();

        let action = submit_text(&flock, &mut pane, P, "ours", true, true, Some(&guard));

        assert!(!action.ok);
        assert!(action.detail.starts_with("agent session is dead"));
        assert!(pane.keys.is_empty());
        assert!(pane.typed.is_empty());
    }

    #[test]
    fn composer_preflight_submits_foreign_text_once_only_when_draft_preservation_is_disabled() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(submit_pending_screen("foreign draft")),
            Some(submit_pending_screen("foreign draft")),
            Some(submit_live_screen()),
        ]);
        let guard = flock.typing_guard(P).unwrap();

        assert_eq!(
            Ok(()),
            ensure_box_clear(&flock, &mut pane, P, false, false, false, &guard)
        );
        assert_eq!(1, pane.keys.len());
        assert_eq!(vec![SUBMIT_POLL_S[0]], pane.sleeps);
    }

    #[test]
    fn composer_preflight_preserves_a_draft_and_does_not_send_a_key() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.screen_script.borrow_mut().clear();
        pane.screens
            .insert(P.into(), submit_pending_screen("human draft"));
        let guard = flock.typing_guard(P).unwrap();

        assert_eq!(
            Err("human_draft"),
            ensure_box_clear(&flock, &mut pane, P, false, true, false, &guard)
        );
        assert!(pane.keys.is_empty());
    }

    fn styled_echo_frame(echo_rows: &str, composer: &str) -> String {
        let frame = format!(
            "\x1b[1;2m\u{203a}\x1b[0m {echo_rows}\n\x1b[1m\u{203a}\x1b[0m {composer}\n\n\n  gpt \u{b7} ~/w\n"
        );
        let (_, detail) = box_text(&frame);
        assert!(
            detail.starts_with(AMBIGUOUS_COMPOSER),
            "fixture must exercise styled ambiguity, got {detail:?}"
        );
        frame
    }

    #[test]
    fn ambiguous_preflight_flushes_exact_owned_region_when_dim_span_is_accounted() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        let frame = styled_echo_frame("old prompt", "live draft");
        flock.remember_typed(P, "live draft");
        flock.remember_typed(P, "old prompt");
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(frame.clone()), // ambiguous composer preflight
            Some(frame),         // one-shot submit key decision
            Some(submit_live_screen()),
        ]);
        let guard = flock.typing_guard(P).unwrap();

        assert_eq!(
            Ok(()),
            ensure_box_clear(&flock, &mut pane, P, false, false, false, &guard)
        );
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn styled_dim_span_requires_exact_full_text_and_checks_all_outbox_statuses() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        // The entire span through the next sigil is evidence. Prefix ownership of only the
        // first dim row must not authorize the composer.
        let frame = styled_echo_frame("old prompt\nadded transcript output", "live draft");
        flock.remember_typed(P, "live draft");
        flock.remember_typed(P, "old prompt");
        let mut message = flock
            .new_message(P, "test", "old prompt added transcript output", "t2457")
            .unwrap();
        message.text = "old prompt".into();
        message.status = "sent".into();
        flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
        pane.screen_script.borrow_mut().clear();
        pane.screens.insert(P.into(), frame);
        let guard = flock.typing_guard(P).unwrap();

        assert_eq!(
            Err("ambiguous-composer"),
            ensure_box_clear(&flock, &mut pane, P, false, false, false, &guard)
        );
        assert!(pane.keys.is_empty());

        // Exact complete-span provenance is accepted even when the outbox row is not queued.
        flock.outbox.lock().unwrap().get_mut(P).unwrap()[0].text =
            "old prompt added transcript output".into();
        let frame = styled_echo_frame("old prompt\nadded transcript output", "live draft");
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(frame.clone()),
            Some(frame),
            Some(submit_live_screen()),
        ]);
        assert_eq!(
            Ok(()),
            ensure_box_clear(&flock, &mut pane, P, false, false, false, &guard)
        );
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn guarded_send_routes_owned_ambiguous_text_through_preflight_before_any_blocker() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        let frame = styled_echo_frame("old prompt", "ours");
        flock.remember_typed(P, "old prompt");
        flock.remember_typed(P, "ours");
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(frame.clone()), // dead-session check
            Some(frame.clone()), // ownership-aware preflight
            Some(frame),         // one-shot submit key decision
            Some(submit_live_screen()),
        ]);
        let mut action_ran = false;

        let result = send_guarded(
            &flock,
            &mut pane,
            P,
            SendGuardOptions::default(),
            || None,
            |_, _| {
                action_ran = true;
                Ok(SendAction::completed(true, "sent"))
            },
        );

        assert!(action_ran);
        assert_eq!((true, Some(true), "sent".into()), result);
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn federated_one_shot_failure_preserves_its_pending_input_hold_reason() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.federated = true;
        let frame = concat!(
            ">_ OpenAI Codex (v0.165.0)\n\n",
            "\x1b[1m\u{203a} pending text\x1b[0m\n\n",
            "  GPT-6-Astra high \u{b7} ~/workspace/x\n",
            "  ? for shortcuts\n"
        );
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(frame.into()),
            Some(frame.into()),
            Some(frame.into()),
            Some(frame.into()),
        ]);

        let result = send_guarded(
            &flock,
            &mut pane,
            P,
            SendGuardOptions::default(),
            || None,
            |_, _| Ok(SendAction::completed(true, "unexpected")),
        );
        assert_eq!(1, pane.keys.len());
        assert!(!result.0);
        assert_eq!(None, result.1);
        assert!(result.2.starts_with(BOX_HOLD));
        assert!(result.2.contains("federated-pending-input"));
    }

    #[test]
    fn press_keys_defers_pending_text_confirmation_until_after_guard_release() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.pending_override = Some("existing text".into());
        let guard = flock.typing_guard(P).unwrap();
        let mut action = press_keys(&mut pane, P, &["Enter".into()], &guard);

        assert!(pane.keys.is_empty());
        drop(guard);
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(submit_live_screen()),
            Some(submit_pending_screen("existing text")),
            Some(submit_live_screen()),
            Some(submit_live_screen()),
            Some(submit_live_screen()),
        ]);
        confirm_send_action(&mut pane, &mut action);

        assert!(action.ok, "{}", action.detail);
        assert_eq!(1, pane.keys.len());
    }

    #[test]
    fn guarded_send_rechecks_permission_under_lock_and_keeps_terminal_failure_distinct() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        let mut permission_checks = 0;

        let result = send_guarded(
            &flock,
            &mut pane,
            P,
            SendGuardOptions::default(),
            || {
                permission_checks += 1;
                let lock_was_held = std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            flock
                                .typing_guard_with_timeout(P, std::time::Duration::ZERO)
                                .is_err()
                        })
                        .join()
                        .unwrap()
                });
                assert!(
                    lock_was_held,
                    "permission check must be inside typing guard"
                );
                None
            },
            |_, _| Err("terminal failure".into()),
        );

        assert_eq!(1, permission_checks);
        assert_eq!((true, Some(false), "terminal failure".into()), result);
    }

    #[test]
    fn guarded_send_reports_a_box_hold_without_running_the_action() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.screen_script.borrow_mut().clear();
        pane.screens
            .insert(P.into(), submit_pending_screen("human draft"));
        let mut action_ran = false;

        let result = send_guarded(
            &flock,
            &mut pane,
            P,
            SendGuardOptions {
                hold_while_typing: true,
                allow_modal: false,
                preserve_draft: true,
            },
            || None,
            |_, _| {
                action_ran = true;
                Ok(SendAction::completed(true, "unexpected"))
            },
        );

        assert!(!action_ran);
        assert!(!result.0);
        assert_eq!(None, result.1);
        assert!(result.2.starts_with(BOX_HOLD));
        assert!(pane.keys.is_empty());
    }

    #[test]
    fn guarded_send_preserves_foreign_text_before_typing_ours() {
        let flock = flock();
        let mut pane = submit_pane("idle");
        pane.receipt_ok = true;
        pane.screen_script.borrow_mut().clear();
        pane.screen_script.borrow_mut().extend([
            Some(submit_pending_screen("foreign text")), // dead-session check
            Some(submit_pending_screen("foreign text")), // final preflight
            Some(submit_pending_screen("foreign text")), // one-shot Enter decision
            Some(submit_live_screen()),                  // one-shot box check
            Some(submit_live_screen()),                  // submit_text dead check
            Some(submit_live_screen()),                  // pre-type status context
            Some(submit_pending_screen("ours")),         // first submit key decision
            Some(submit_live_screen()),                  // short confirmation
            Some(submit_live_screen()),                  // first verification
            Some(submit_live_screen()),                  // second verification
        ]);

        let result = send_guarded(
            &flock,
            &mut pane,
            P,
            SendGuardOptions {
                hold_while_typing: true,
                allow_modal: false,
                preserve_draft: false,
            },
            || None,
            |env, guard| {
                Ok(submit_text(
                    &flock,
                    env,
                    P,
                    "ours",
                    true,
                    false,
                    Some(guard),
                ))
            },
        );

        assert_eq!((true, Some(true), "sent".to_string()), result);
        assert_eq!(vec!["key", "typed", "receipt", "key"], pane.events);
        assert_eq!(2, pane.keys.len());
    }
}

// --- test_busy_native_queue.py ---------------------------------------------------------------

#[test]
fn test_codex_busy_is_deliverable() {
    assert_eq!(None, send_blocker(&busy_codex(), P));
}

#[test]
fn test_codex_uses_tab_and_recomputes_after_turn_finishes() {
    let mut pane = busy_codex();
    assert_eq!(
        (true, "sent".to_string(), Some("Tab".to_string())),
        send_submit_key(&mut pane, P, "Enter", false)
    );
    pane.statuses.insert(P.into(), "idle".into());
    pane.screens
        .insert(P.into(), "\u{203a} Ask Codex to do anything\n".into());
    // A learned Tab must never leak into a later idle send: the fallback here is what the
    // Python's `_submit_key[pane]` held, and the recomputed key is Enter.
    assert_eq!(
        Some("Enter".to_string()),
        send_submit_key(&mut pane, P, "Tab", false).2
    );
}

#[test]
fn test_busy_claude_and_pi_keep_enter() {
    for harness in ["claude", "pi"] {
        let mut pane = busy_codex();
        pane.harness.insert(P.into(), harness.into());
        assert_eq!(None, send_blocker(&pane, P));
        assert_eq!(
            Some("Enter".to_string()),
            send_submit_key(&mut pane, P, "Enter", false).2
        );
    }
}

#[test]
fn test_dialog_opening_before_retry_gets_no_key() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "Would you like to run the following command?\n\u{276f} 1. Yes\n  2. No\n\n      Press Enter to confirm or Esc to cancel\n".into(),
    );
    let (ok, detail, key) = send_submit_key(&mut pane, P, "Enter", false);
    assert!(!ok);
    assert!(detail.contains("dialog"));
    assert_eq!(None, key);
    assert!(pane.keys.is_empty());
}

#[test]
fn test_long_busy_composer_with_unknown_backend_status_queues() {
    let mut pane = busy_codex();
    pane.statuses.remove(P);
    pane.screens.insert(
        P.into(),
        format!(
            "\u{2022} Working (1m \u{b7} esc to interrupt)\n\u{203a} message\n{}  GPT-6-Astra high \u{b7} ~/project\n",
            "  continuation\n".repeat(12)
        ),
    );
    assert_eq!(
        Some("Tab".to_string()),
        send_submit_key(&mut pane, P, "Enter", false).2
    );
}

#[test]
fn test_an_unreadable_pane_gets_no_key() {
    let mut pane = Pane::default();
    let (ok, detail, key) = send_submit_key(&mut pane, P, "Enter", false);
    assert!(!ok && key.is_none());
    assert_eq!("submission unconfirmed: pane unreadable before key", detail);
}

// --- send_blocker (policy halves of test_dunk_delivery / test_dunk_inbox) --------------------

#[test]
fn test_a_dead_session_refuses_and_names_the_marker() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "\u{25a0} Server connection could not be restored\n".into(),
    );
    let (state, reason) = send_blocker(&pane, P).unwrap();
    assert_eq!("dead", state);
    assert!(reason.contains("server connection could not be restored"));
}

#[test]
fn test_a_gone_process_refuses_even_when_the_screen_looks_alive() {
    let mut pane = busy_codex();
    pane.gone = true;
    assert_eq!(
        Some(("dead", "the lane's recorded process is gone".to_string())),
        send_blocker(&pane, P)
    );
}

#[test]
fn test_an_unknown_harness_keeps_the_capability_hold_while_working() {
    let mut pane = busy_codex();
    pane.harness.insert(P.into(), "mystery".into());
    let (state, reason) = send_blocker(&pane, P).unwrap();
    assert_eq!("blocked", state);
    assert!(reason.starts_with("mystery does not queue text typed during a turn"));
    pane.harness.remove(P);
    assert!(send_blocker(&pane, P)
        .unwrap()
        .1
        .starts_with("this harness does not queue"));
}

#[test]
fn test_a_modal_is_not_a_hold() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "  \u{2514}   1. Yes, proceed (y)\n\n      Press Enter to confirm or Esc to cancel\n"
            .into(),
    );
    assert_eq!(None, send_blocker(&pane, P));
}

// --- test_codex_ambiguous_composer.py (the _box_holds / send_blocker halves) -------------------

const NOTICE: &str =
    "[inbox] new message for fixture from reviewer. Read it with: dunkingsheep i\u{2026}";
const FOOTER: &str = "  model \u{b7} /tmp/synthetic-fixture \u{b7} task";

fn frame(body: &str, continuation: &[&str]) -> String {
    let mut rows: Vec<String> = vec!["synthetic transcript".into()];
    rows.resize(18, String::new());
    rows.push(format!("\u{203a} {body}"));
    rows.extend(continuation.iter().map(|c| c.to_string()));
    rows.push(String::new());
    rows.push(String::new());
    rows.push(FOOTER.to_string());
    rows.join("\n") + "\n"
}

fn with_screen(screen: String) -> Pane {
    let mut pane = Pane::default();
    pane.harness.insert(P.into(), "codex".into());
    pane.screens.insert(P.into(), screen);
    pane
}

fn check_ambiguous(screen: String) {
    let pane = with_screen(screen.clone());
    let (_text, detail) = crate::terminal::screen::box_text(&screen);
    assert!(detail.contains("ambiguous composer"), "{detail}");
    assert!(!detail.contains("box is empty"), "{detail}");
    assert_eq!(None, box_holds(&pane, P, NOTICE));
    let (state, _reason) = send_blocker(&pane, P).expect("an ambiguous composer must block");
    assert_eq!("unknown", state);
}

#[test]
fn test_ellipsised_notice_is_ambiguous_not_empty() {
    check_ambiguous(frame(NOTICE, &[]));
}

#[test]
fn test_wrapped_human_draft_is_preserved() {
    check_ambiguous(frame(
        "my unfinished draft",
        &["  second line", "  third line"],
    ));
}

#[test]
fn test_notice_prefix_with_appended_human_text_stays_unknown() {
    check_ambiguous(frame(NOTICE, &["  do not submit this human addition"]));
}

#[test]
fn test_placeholder_with_footer_shaped_human_continuation_is_unknown() {
    check_ambiguous(frame(
        "\x1b[2mAsk Codex to do anything\x1b[0m",
        &["  human draft \u{b7} still unfinished"],
    ));
}

#[test]
fn test_whitespace_and_ansi_only_rows_do_not_prove_empty() {
    let screen = frame(NOTICE, &[]).replace(
        &format!("\n\n\n{FOOTER}"),
        &format!("\n \x1b[0m\n\t\n{FOOTER}"),
    );
    check_ambiguous(screen);
}

#[test]
fn test_empty_placeholder_is_still_explicitly_empty() {
    let pane = with_screen(frame("\x1b[2mAsk Codex to do anything\x1b[0m", &[]));
    assert_eq!(Some(false), box_holds(&pane, P, NOTICE));
    assert_eq!(None, send_blocker(&pane, P));
}

#[test]
fn test_box_holds_sees_our_own_text_and_a_federated_composer() {
    // Our text still in the box (claude-style bordered composer): held.
    let screen = "\u{2502} \u{276f} check your inbox now\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n".to_string();
    let pane = with_screen(screen);
    assert_eq!(Some(true), box_holds(&pane, P, "check your inbox now"));
    // A human draft is NOT ours.
    assert_eq!(Some(false), box_holds(&pane, P, "something else entirely"));
    // Unreadable pane: unknown.
    assert_eq!(None, box_holds(&Pane::default(), P, "x"));
    // A modal open: our text left the box and opened it.
    let modal = with_screen("      Press Enter to confirm or Esc to cancel\n".into());
    assert_eq!(Some(false), box_holds(&modal, P, "x"));
}

// --- test_dunk_core.py: _clear_to_send ---------------------------------------------------------

fn hint_screen() -> String {
    "\u{276f}\u{a0}\n\x1b[2mnew task? /clear to save 497.4k tokens\x1b[0m\n".to_string()
}

#[test]
fn test_two_clear_checks_a_second_apart_allow_the_send() {
    let pane = with_screen(hint_screen());
    let waited = RefCell::new(Vec::new());
    let wait = |s: f64| {
        waited.borrow_mut().push(s);
        false
    };
    assert!(clear_to_send(&pane, P, &wait));
    assert_eq!(
        vec![TYPING_SETTLE_S],
        *waited.borrow(),
        "one settle wait of TYPING_SETTLE_S"
    );
    // send_blocker reads the box once (ansi) and the dead check once (plain); the redraw check
    // then reads the styled screen again, and the dead check once more.
    let ansi = pane.reads.borrow().iter().filter(|(_, ansi)| *ansi).count();
    assert_eq!(2, ansi, "two styled reads, a settle apart");
}

#[test]
fn test_text_in_the_box_no_longer_stops_a_send() {
    let pane = with_screen("\u{276f} mid sentence\n".to_string());
    assert!(clear_to_send(&pane, P, &|_| false));
}

#[test]
fn test_a_stopped_dunk_does_not_finish_its_settle_wait() {
    let pane = with_screen(hint_screen());
    assert!(!clear_to_send(&pane, P, &|_| true));
}

#[test]
fn test_a_dead_session_is_not_clear_to_send_and_a_redraw_into_one_is_caught() {
    let pane = with_screen("\u{25a0} Reconnect failed\n".to_string());
    assert!(!clear_to_send(&pane, P, &|_| false));
    // A screen that cannot be read on the second look is inconclusive, not a hold.
    struct Flaky(RefCell<u32>, Pane);
    impl SendEnv for Flaky {
        fn read_pane(&self, p: &str, l: i64, a: bool) -> Option<String> {
            *self.0.borrow_mut() += 1;
            // reads 1 (dead check) and 2 (box) succeed, 3+ (second look) do not
            if *self.0.borrow() <= 2 {
                self.1.read_pane(p, l, a)
            } else {
                None
            }
        }
        fn pane_harness(&self, p: &str) -> Option<String> {
            self.1.pane_harness(p)
        }
        fn session_process_gone(&self, p: &str) -> bool {
            self.1.session_process_gone(p)
        }
        fn read_status(&self, p: &str) -> Option<String> {
            self.1.read_status(p)
        }
        fn send_key(&mut self, p: &str, k: &str) -> (bool, String) {
            self.1.send_key(p, k)
        }
        fn federated_input(&self) -> bool {
            false
        }
    }
    let flaky = Flaky(RefCell::new(0), with_screen(hint_screen()));
    assert!(clear_to_send(&flaky, P, &|_| false));
}

// --- Message ----------------------------------------------------------------------------------

#[test]
fn test_message_outcomes_and_wire_form() {
    let mut message = Message::new("m1", "w2:p1", "Sheep", "hello", "send_text");
    assert_eq!("queued", message.outcome());
    let queued = message.to_dict_at(message.created_at + 7.9);
    assert_eq!(serde_json::json!(true), queued["queued"]);
    assert_eq!(serde_json::Value::Null, queued["ok"]);
    assert_eq!(serde_json::json!(7), queued["waiting_s"]);
    assert_eq!(
        serde_json::json!("queued for Sheep, delivering once the input box is clear"),
        queued["message"]
    );
    message.holds = 2;
    assert_eq!(
        "queued: the box in Sheep would not clear",
        message.queued_message()
    );
    message.status = "sent".into();
    assert_eq!("ok", message.outcome());
    let sent = message.to_dict();
    assert_eq!(serde_json::json!(true), sent["ok"]);
    assert_eq!(serde_json::json!("sent"), sent["message"]);
    assert_eq!(serde_json::Value::Null, sent["waiting_s"]);
    message.status = "failed".into();
    message.detail = Some("submission unconfirmed: pane unreadable".into());
    assert_eq!("unconfirmed", message.outcome());
    message.detail = Some("agent session is dead (x)".into());
    assert_eq!("refused", message.outcome());
    message.detail = Some("the Enter did not submit".into());
    assert_eq!("unconfirmed", message.outcome());
    for outcome in ["ok", "queued", "unconfirmed", "refused"] {
        assert!(OUTCOMES.contains(&outcome));
    }
}

mod flush_box_tests {
    use super::*;

    fn pane() -> Pane {
        Pane {
            panes: vec![serde_json::json!({
                "pane_id": P,
                "tab_label": "Sheep",
                "label": "Sheep shell",
                "agent": "claude"
            })],
            hint: "no backend panes".into(),
            ..Pane::default()
        }
    }

    #[test]
    fn flush_box_errors_when_the_input_box_cannot_be_read() {
        let mut pane = pane();
        pane.screen_script.borrow_mut().push_back(None);

        let error = flush_box(&mut pane, &serde_json::json!(P), None).unwrap_err();

        assert_eq!("could not read the input box of w2:p1", error.to_string());
        assert!(pane.keys.is_empty());
        assert_eq!(1, pane.reads.borrow().len());
    }

    #[test]
    fn flush_box_refuses_an_ambiguous_composer_without_a_key() {
        let mut pane = pane();
        pane.screen_script.borrow_mut().push_back(Some(
            "❯ possible transcript text\n\n\nstatus · ready\n".into(),
        ));

        let result = flush_box(&mut pane, &serde_json::json!(P), None).unwrap();

        assert_eq!(serde_json::json!(false), result["flushed"]);
        assert_eq!(serde_json::json!("unknown"), result["state"]);
        assert!(result["detail"]
            .as_str()
            .unwrap()
            .starts_with(crate::terminal::screen::AMBIGUOUS_COMPOSER));
        assert!(pane.keys.is_empty());
        assert_eq!(1, pane.reads.borrow().len());
    }

    #[test]
    fn test_flush_says_what_it_read_when_there_is_nothing_to_submit() {
        let mut pane = pane();
        pane.screen_script
            .borrow_mut()
            .push_back(Some(hint_screen()));

        let result = flush_box(&mut pane, &serde_json::json!({"pane_id": P}), None).unwrap();

        assert_eq!(serde_json::json!(false), result["flushed"]);
        assert_eq!(serde_json::Value::Null, result["text"]);
        assert!(result["message"]
            .as_str()
            .unwrap()
            .starts_with("nothing to submit: "));
        assert_eq!(
            format!("nothing to submit: {}", result["detail"].as_str().unwrap()),
            result["message"].as_str().unwrap()
        );
        assert_eq!(serde_json::json!("Sheep"), result["target_label"]);
        assert!(pane.keys.is_empty());
        assert_eq!(1, pane.reads.borrow().len());
    }

    #[test]
    fn test_flush_reports_the_evidence_when_the_box_reads_clear() {
        let mut pane = pane();
        pane.statuses.insert(P.into(), "idle".into());
        pane.screen_script.borrow_mut().extend([
            Some(submit_pending_screen("stranded text")), // initial flush read
            Some(submit_live_screen()),                   // dead-session check
        ]);
        queue_clear_confirmation(&pane, "stranded text");

        let result = flush_box(&mut pane, &serde_json::json!("Sheep"), None).unwrap();

        assert_eq!(serde_json::json!(P), result["pane_id"]);
        assert_eq!(serde_json::json!("Sheep"), result["target_label"]);
        assert_eq!(serde_json::json!(true), result["flushed"]);
        assert_eq!(serde_json::json!("stranded text"), result["text"]);
        assert_eq!(serde_json::json!("sent"), result["message"]);
        assert_eq!(vec![(P.to_string(), "Enter".to_string())], pane.keys);
        assert_eq!(6, pane.reads.borrow().len());
    }
}

#[test]
fn pane_generation_requires_pid_and_process_start_and_binds_both() {
    let pid = serde_json::json!({"pane_pid": 41, "pid": 99});
    assert_eq!(
        Some("pid:41:start:1001".to_string()),
        pane_generation_with(&pid, |seen| {
            assert_eq!(seen, 41);
            Some("1001".into())
        })
    );
    assert_ne!(
        pane_generation_with(&pid, |_| Some("1002".into())),
        pane_generation_with(&pid, |_| Some("1001".into()))
    );
    assert_ne!(
        pane_generation_with(&serde_json::json!({"pane_pid": 42}), |_| Some(
            "1001".into()
        )),
        pane_generation_with(&pid, |_| Some("1001".into()))
    );
    assert_eq!(
        None,
        pane_generation_with(&serde_json::json!({"pane_pid": 41}), |_| None)
    );
    assert_eq!(
        None,
        pane_generation_with(
            &serde_json::json!({"pane_pid": 41}),
            |_| Some(String::new())
        )
    );
}

#[test]
fn pane_generation_falls_back_to_pid_and_reads_real_process_start() {
    use crate::flock::panes::PaneInfoEnv;
    use serde_json::Value;

    struct Pane(Option<Value>);
    impl PaneInfoEnv for Pane {
        fn list_panes_grouped(&self) -> Vec<Value> {
            Vec::new()
        }
        fn get_pane(&self, pane_id: &str) -> Option<Value> {
            assert_eq!(pane_id, "%7");
            self.0.clone()
        }
    }

    let env = Pane(Some(serde_json::json!({
        "pane_pid": 0,
        "pid": std::process::id(),
    })));
    let start = crate::proc::process_start(i64::from(std::process::id())).unwrap();
    assert_eq!(
        Some(format!("pid:{}:start:{start}", std::process::id())),
        pane_generation(&env, "%7")
    );
    assert_eq!(None, pane_generation(&Pane(None), "%7"));
    assert_eq!(
        None,
        pane_generation(
            &Pane(Some(serde_json::json!({"pane_pid": 999_999_999}))),
            "%7"
        )
    );
}

#[cfg(test)]
mod owned_text_tests {
    use super::super::sends::{pane_generation, text_identity, Message};
    use super::Flock;
    use super::{Pane, P};

    fn flock() -> Flock {
        let dir = tempfile::tempdir().unwrap();
        // Persisting is disabled; the in-memory ownership state starts empty.
        Flock::new(dir.path().join("launches.json"), dir.path(), false)
    }

    #[test]
    fn looks_like_ours_recognizes_inbox_notice_after_restart_without_memory() {
        let restarted = flock();

        assert!(restarted.looks_like_ours(
            "%1",
            "[inbox] new message for codex-rust-writer; read it with inbox read"
        ));
        assert!(!restarted.looks_like_ours("%1", "a human's draft"));
    }

    #[test]
    fn looks_like_ours_recognizes_normalized_recent_typed_text_by_prefix() {
        let flock = flock();
        flock.remember_typed("%1", "finish the review\nthen update the owner");

        assert!(flock.looks_like_ours("%1", "finish   the review"));
        assert!(flock.looks_like_ours("%1", "finish the review then update the owner"));
        assert!(!flock.looks_like_ours("%2", "finish the review"));
    }

    #[test]
    fn looks_like_ours_recognizes_queued_text_only_while_queued() {
        let flock = flock();
        let message = Message::new("m1", "%1", "writer", "queued body text", "t1749");
        flock
            .outbox
            .lock()
            .unwrap()
            .entry("%1".into())
            .or_default()
            .push(message.clone());

        assert!(flock.looks_like_ours("%1", "queued body"));
        flock.outbox.lock().unwrap().get_mut("%1").unwrap()[0].status = "sent".into();
        assert!(!flock.looks_like_ours("%1", "queued body"));
    }

    #[test]
    fn looks_like_ours_uses_the_80_character_consumption_prefix_for_wrapped_text() {
        let flock = flock();
        let full = format!("{} trailing source text", "界".repeat(96));
        flock.remember_typed("%1", &full);
        let shown = format!("{}…", "界".repeat(80));

        assert!(flock.looks_like_ours("%1", &shown));
    }

    #[test]
    fn looks_like_ours_bounds_recent_history_and_rejects_empty_text() {
        let flock = flock();
        for index in 0..5 {
            flock.remember_typed("%1", &format!("owned message {index}"));
        }

        assert_eq!(4, flock.sent_text_by_pane.lock().unwrap()["%1"].len());
        assert!(!flock.looks_like_ours("%1", "owned message 0"));
        assert!(flock.looks_like_ours("%1", "owned message 4"));

        flock.remember_typed("%2", "");
        assert!(!flock.looks_like_ours("%2", ""));
        assert!(!flock.looks_like_ours("%2", "human text"));
    }

    #[test]
    fn late_older_failure_cannot_replace_newer_no_enter_intent() {
        let flock = flock();
        let mut older = Message::new("m9", "%1", "writer", "same text", "direct");
        older.created_at = 1.0;
        older.status = "failed".into();
        older.attempt_generation = Some("pid:9:start:1".into());
        flock.note_stuck_candidate(&older);

        let mut newer = Message::new("m10", "%1", "writer", "same text", "direct");
        newer.created_at = 2.0;
        newer.status = "sent".into();
        newer.press_enter = false;
        newer.attempt_generation = Some("pid:9:start:1".into());
        flock.note_stuck_candidate(&newer);
        flock.note_stuck_candidate(&older);

        let candidates = flock.stuck_candidates.lock().unwrap();
        let candidate = &candidates["%1"];
        assert_eq!("m10", candidate.message_id);
        assert_eq!("sent", candidate.status);
        assert!(!candidate.press_enter);
        assert!(!candidate.eligible);
    }

    #[test]
    fn explicit_no_enter_send_is_never_recoverable() {
        let flock = flock();
        let mut message = Message::new("m1", "%1", "writer", "draft", "direct");
        message.status = "failed".into();
        message.press_enter = false;
        message.attempt_generation = Some("pid:9:start:1".into());

        flock.note_stuck_candidate(&message);

        let candidates = flock.stuck_candidates.lock().unwrap();
        assert!(!candidates["%1"].eligible);
        assert!(!candidates["%1"].press_enter);
    }

    #[test]
    fn key_intent_is_never_recoverable() {
        let flock = flock();
        let mut message = Message::new("m1", "%1", "writer", "Escape", "direct");
        message.status = "failed".into();
        message.kind = "keys".into();
        message.attempt_generation = Some("pid:9:start:1".into());

        flock.note_stuck_candidate(&message);

        let candidates = flock.stuck_candidates.lock().unwrap();
        assert!(!candidates["%1"].eligible);
        assert_eq!("keys", candidates["%1"].kind);
    }

    #[test]
    fn newer_cancelled_intent_tombstones_older_failed_candidate() {
        let flock = flock();
        let mut older = Message::new("m8", "%1", "writer", "body", "direct");
        older.created_at = 1.0;
        older.status = "failed".into();
        older.attempt_generation = Some("pid:9:start:1".into());
        flock.note_stuck_candidate(&older);

        let mut cancelled = Message::new("m9", "%1", "writer", "body", "direct");
        cancelled.created_at = 2.0;
        cancelled.status = "cancelled".into();
        cancelled.attempt_generation = Some("pid:9:start:1".into());
        flock.note_stuck_candidate(&cancelled);

        let candidates = flock.stuck_candidates.lock().unwrap();
        assert_eq!("m9", candidates["%1"].message_id);
        assert_eq!("cancelled", candidates["%1"].status);
        assert!(!candidates["%1"].eligible);
    }

    #[test]
    fn candidate_without_attempt_identity_is_never_eligible() {
        let flock = flock();
        let mut message = Message::new("m1", "%1", "writer", "maybe sent", "direct");
        message.status = "failed".into();

        flock.note_stuck_candidate(&message);

        let candidates = flock.stuck_candidates.lock().unwrap();
        assert!(!candidates["%1"].eligible);
        assert_eq!(None, candidates["%1"].generation);
    }

    #[test]
    fn typed_text_with_enter_and_attempt_identity_is_recoverable() {
        let flock = flock();
        let mut message = Message::new("m2", "%1", "writer", "awaiting confirmation", "direct");
        message.status = "typed".into();
        message.attempt_generation = Some("pid:9:start:1".into());

        flock.note_stuck_candidate(&message);

        let candidates = flock.stuck_candidates.lock().unwrap();
        let candidate = &candidates["%1"];
        assert_eq!("typed", candidate.status);
        assert!(candidate.press_enter);
        assert!(candidate.eligible);
    }

    #[test]
    fn typed_no_enter_intent_is_not_recoverable() {
        let flock = flock();
        let mut message = Message::new("m3", "%1", "writer", "deliberate draft", "direct");
        message.status = "typed".into();
        message.press_enter = false;
        message.attempt_generation = Some("pid:9:start:1".into());

        flock.note_stuck_candidate(&message);

        let candidates = flock.stuck_candidates.lock().unwrap();
        assert!(!candidates["%1"].eligible);
        assert!(!candidates["%1"].press_enter);
    }

    #[test]
    fn candidate_retains_message_metadata_and_repeated_update_is_idempotent() {
        let flock = flock();
        let mut message = Message::new("m17", "%1", "writer label", "exact text", "dunk-4");
        message.created_at = 42.5;
        message.status = "sent".into();
        message.scheduled = true;
        message.attempt_generation = Some("pid:17:start:9".into());
        message.notify_for = vec!["i20".into(), "i21".into()];

        flock.note_stuck_candidate(&message);
        let first = flock.stuck_candidates.lock().unwrap()["%1"].clone();
        flock.note_stuck_candidate(&message);
        let second = flock.stuck_candidates.lock().unwrap()["%1"].clone();

        assert_eq!(first, second);
        assert_eq!(42.5, second.created_at);
        assert_eq!(Some("pid:17:start:9".into()), second.generation);
        assert_eq!(vec!["i20", "i21"], second.notify_for);
        assert_eq!("dunk-4", second.source);
        assert_eq!("exact text", second.text);
        assert_eq!("writer label", second.label);
        assert!(second.scheduled);
        assert!(second.eligible);
    }

    #[test]
    fn candidate_updates_preserve_recovery_metadata_only_for_same_identity() {
        let flock = flock();
        let mut original = Message::new("m20", "%1", "writer", "original", "direct");
        original.created_at = 20.0;
        original.status = "failed".into();
        original.attempt_generation = Some("pid:20:start:4".into());
        flock.note_stuck_candidate(&original);
        {
            let mut candidates = flock.stuck_candidates.lock().unwrap();
            let candidate = candidates.get_mut("%1").unwrap();
            candidate.native_binding = Some((20, "start-4".into(), "session-20".into()));
            candidate.recovery_outcome = Some("consumed".into());
            candidate.recovery_detail = Some("receipt matched".into());
            candidate.recovered_at = Some(21.0);
            candidate.receipts_reconciled = Some(true);
            candidate.native_refusal = Some("boundary".into());
            candidate.unobservable_logged = Some(true);
            candidate.native_recovery = Some(true);
            candidate.schedule_template = Some("schedule template".into());
            candidate.retired_at = Some(22.0);
            candidate.consumed_at = Some(23.0);
            candidate
                .extra_fields
                .insert("extension".into(), serde_json::json!("kept"));
        }

        let mut same = original.clone();
        same.status = "typed".into();
        same.text = "updated body".into();
        flock.note_stuck_candidate(&same);
        let preserved = flock.stuck_candidates.lock().unwrap()["%1"].clone();
        assert_eq!(
            Some((20, "start-4".into(), "session-20".into())),
            preserved.native_binding
        );
        assert_eq!(Some("consumed"), preserved.recovery_outcome.as_deref());
        assert_eq!("consumed", preserved.status);
        assert!(!preserved.eligible);
        assert_eq!(
            Some("receipt matched"),
            preserved.recovery_detail.as_deref()
        );
        assert_eq!(Some(21.0), preserved.recovered_at);
        assert_eq!(Some(true), preserved.receipts_reconciled);
        assert_eq!(Some("boundary"), preserved.native_refusal.as_deref());
        assert_eq!(Some(true), preserved.unobservable_logged);
        assert_eq!(Some(true), preserved.native_recovery);
        assert_eq!(
            Some("schedule template"),
            preserved.schedule_template.as_deref()
        );
        assert_eq!(Some(22.0), preserved.retired_at);
        assert_eq!(Some(23.0), preserved.consumed_at);
        assert_eq!(
            serde_json::json!("kept"),
            preserved.extra_fields["extension"]
        );

        let mut replacement = Message::new("m21", "%1", "writer", "replacement", "direct");
        replacement.created_at = 21.0;
        replacement.status = "failed".into();
        replacement.attempt_generation = Some("pid:20:start:4".into());
        flock.note_stuck_candidate(&replacement);
        let replacement = flock.stuck_candidates.lock().unwrap()["%1"].clone();
        assert_eq!("m21", replacement.message_id);
        assert_eq!(None, replacement.native_binding);
        assert_eq!(None, replacement.recovery_outcome);
        assert_eq!(None, replacement.recovery_detail);
        assert_eq!(None, replacement.recovered_at);
        assert_eq!(None, replacement.receipts_reconciled);
        assert_eq!(None, replacement.native_refusal);
        assert_eq!(None, replacement.unobservable_logged);
        assert_eq!(None, replacement.native_recovery);
        assert_eq!(None, replacement.schedule_template);
        assert_eq!(None, replacement.retired_at);
        assert_eq!(None, replacement.consumed_at);
        assert!(replacement.extra_fields.is_empty());
    }

    fn pane_with_current_process(pane_id: &str) -> Pane {
        let mut env = Pane::default();
        env.panes.push(serde_json::json!({
            "pane_id": pane_id,
            "pane_pid": std::process::id(),
        }));
        env
    }

    #[test]
    fn stranded_message_matches_composer_text_instead_of_fifo_head() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
        let head = Message::new("m1", P, "writer", "queued head", "send_text");
        let actual = Message::new("m2", P, "writer", "actual composer", "send_text");
        flock
            .outbox
            .lock()
            .unwrap()
            .insert(P.into(), vec![head, actual]);
        let env = pane_with_current_process(P);

        let found = flock.stranded_message(P, "actual composer", &env).unwrap();
        assert_eq!("m2", found.id);
        assert!(flock
            .stranded_message(P, "foreign earlier draft", &env)
            .is_none());
    }

    #[test]
    fn stranded_message_prefers_newest_exact_match_then_wrapped_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
        let first = Message::new("m1", P, "writer", "same", "send_text");
        let newest = Message::new("m2", P, "writer", "same", "send_text");
        let long = Message::new(
            "m3",
            P,
            "writer",
            "a long notice whose first forty characters are stable across wrapped display lines",
            "dunk-notice",
        );
        flock
            .outbox
            .lock()
            .unwrap()
            .insert(P.into(), vec![first, newest, long]);
        let env = pane_with_current_process(P);

        assert_eq!(
            "m2",
            flock.stranded_message(P, "same", &env).unwrap().id,
            "the newest exact text wins"
        );
        assert_eq!(
            "m3",
            flock
                .stranded_message(P, "a long notice whose first forty characters", &env)
                .unwrap()
                .id,
            "wrapped notice may match by the source's 40-character prefix"
        );
    }

    #[test]
    fn stranded_message_prefers_a_current_bound_stuck_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
        let env = pane_with_current_process(P);
        let mut candidate = Message::new("m-candidate", P, "writer", "stuck text", "send_text");
        candidate.status = "failed".into();
        candidate.attempt_generation = pane_generation(&env, P);
        candidate.created_at = 123.0;
        flock.note_stuck_candidate(&candidate);
        let queued = Message::new("m-queued", P, "writer", "stuck text", "send_text");
        flock.outbox.lock().unwrap().insert(P.into(), vec![queued]);

        let found = flock.stranded_message(P, "stuck text", &env).unwrap();
        assert_eq!("m-candidate", found.id);
        assert_eq!("stuck_candidate", found.source);
    }

    #[test]
    fn stuck_candidate_reconstruction_fails_closed_for_missing_or_changed_generation() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
        let env = pane_with_current_process(P);
        let mut candidate = Message::new("m-candidate", P, "writer", "stuck text", "send_text");
        candidate.status = "failed".into();
        candidate.attempt_generation = Some("pid:1:start:old".into());
        flock.note_stuck_candidate(&candidate);

        assert!(flock.stuck_candidate_message(P, &env).is_none());
        assert!(flock.stranded_message(P, "", &env).is_none());
    }

    #[test]
    fn text_identity_is_stable_for_collapsed_content_and_absent_for_empty_text() {
        assert_eq!(
            Some("text:b94d27b9934d3e08".into()),
            text_identity(" hello  world ")
        );
        assert_eq!(text_identity("a b"), text_identity("a\n  b"));
        assert_eq!(None, text_identity(" \n\t "));
    }
}

#[test]
fn record_delivery_outcome_records_metrics_receipts_and_waiters_once() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::{Outgoing, Store};
    use crate::metrics::MetricsStore;
    use rusqlite::Connection;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let notification_ids: Vec<_> = (0..3)
        .map(|index| {
            inbox
                .send(&Outgoing {
                    mailbox: "writer",
                    body: Some(&format!("notice {index}")),
                    ..Outgoing::default()
                })
                .unwrap()
        })
        .collect();
    let observer = Connection::open(&inbox_path).unwrap();
    observer
        .execute_batch(
            "CREATE TABLE notify_updates (count INTEGER NOT NULL);
             INSERT INTO notify_updates VALUES (0);
             CREATE TRIGGER count_notify_updates AFTER UPDATE OF notify_state ON messages
             BEGIN UPDATE notify_updates SET count = count + 1; END;",
        )
        .unwrap();

    let metrics = Arc::new(MetricsStore::open(":memory:").unwrap());
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        metrics: Some(metrics.clone()),
        ..Stores::default()
    });
    let cases = [
        (true, "ok", Some("message.sent"), None, "sent"),
        (
            false,
            "failed",
            Some("message.sent"),
            Some("timeout"),
            "failed",
        ),
        (
            false,
            "undeliverable",
            None,
            Some("undeliverable"),
            "failed",
        ),
    ];

    for (index, (ok, outcome, metric, detail, notify_state)) in cases.into_iter().enumerate() {
        let mut message = Message::new(
            &format!("m{}", index + 1),
            "%1",
            "writer mailbox",
            "é",
            "dunk-7",
        );
        message.created_at = 10.0;
        message.finished_at = Some(15.5);
        message.holds = 2;
        message.notify_for.push(notification_ids[index].clone());
        let cloned = message.clone();

        std::thread::scope(|scope| {
            scope.spawn(|| flock.record_delivery_outcome(&message, ok, detail, outcome, metric));
            scope.spawn(|| flock.record_delivery_outcome(&cloned, ok, detail, outcome, metric));
        });
        message.done.wait();

        let actual_state: Option<String> = observer
            .query_row(
                "SELECT notify_state FROM messages WHERE id = ?1",
                [&notification_ids[index]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(Some(notify_state.to_string()), actual_state);
    }

    let update_count: i64 = observer
        .query_row("SELECT count FROM notify_updates", [], |row| row.get(0))
        .unwrap();
    assert_eq!(3, update_count);

    let events = metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    assert_eq!(2, events.len());
    let sent = events
        .iter()
        .find(|event| event.outcome.as_deref() == Some("ok"))
        .unwrap();
    assert_eq!("message.sent", sent.kind);
    assert_eq!(Some("%1".into()), sent.pane_id);
    assert_eq!(Some("writer mailbox".into()), sent.mailbox);
    assert_eq!(Some("dunk-7".into()), sent.source);
    assert_eq!(None, sent.reason);
    assert_eq!(Some(5_500), sent.duration_ms);
    assert_eq!(Some(2), sent.attempts);
    assert_eq!(Some(2), sent.bytes);
    let failed = events
        .iter()
        .find(|event| event.outcome.as_deref() == Some("failed"))
        .unwrap();
    assert_eq!("message.failed", failed.kind);
    assert_eq!(Some("timeout".into()), failed.reason);
}

#[test]
fn deliver_message_persists_hold_backoff_and_keeps_original_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), true);
    flock
        .attach_status_state(super::status_runtime::StatusState::at_start(
            "test",
            &state_path,
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    let mut message = Message::new("m-hold", P, "pane", "hello", "test");
    message.hold_while_typing = false;
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane::default();
    pane.screens
        .insert(P.into(), submit_pending_screen("human draft"));

    assert_eq!(
        DeliveryOutcome::Held,
        flock.deliver_message("m-hold", 100.0, &mut pane, || Some("disabled".into()))
    );
    let queue = flock.outbox_snapshot();
    let held = &queue[P][0];
    assert_eq!("queued", held.status);
    assert_eq!(Some("disabled".into()), held.detail);
    assert_eq!(Some(105.0), held.next_attempt_at);
    assert_eq!(1, held.holds);
    assert_eq!(Some("human draft".into()), held.hold_signature);
    assert_eq!(Some("box".into()), held.hold_signature_kind);
    assert!(!held.done.is_set());

    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(1, snapshot["outbox"][0]["holds"]);
    assert_eq!(105.0, snapshot["outbox"][0]["next_attempt_at"]);
    assert_eq!(
        serde_json::json!(["box", "human draft"]),
        snapshot["outbox"][0]["hold_signature"]
    );
    let restored = Flock::new(dir.path().join("restored-launches.json"), dir.path(), false);
    restored.restore_snapshot(&snapshot);
    let restored_outbox = restored.outbox_snapshot();
    let restored_message = &restored_outbox[P][0];
    assert_eq!(1, restored_message.holds);
    assert_eq!(Some(105.0), restored_message.next_attempt_at);
    assert_eq!(
        Some("human draft"),
        restored_message.hold_signature.as_deref()
    );
    assert_eq!(Some("box"), restored_message.hold_signature_kind.as_deref());
}

#[test]
fn deliver_message_uses_normalized_block_reason_for_hold_metrics_and_terminal_detail() {
    use crate::flock::panes::Stores;
    use crate::metrics::MetricsStore;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(MetricsStore::open(":memory:").unwrap());
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.attach_stores(Stores {
        metrics: Some(metrics.clone()),
        ..Stores::default()
    });
    flock.outbox.lock().unwrap().insert(
        P.into(),
        vec![Message::new("m-dialog", P, "pane", "hello", "test")],
    );
    let mut pane = Pane::default();
    pane.screens.insert(P.into(), submit_modal_screen());

    for attempt in 0..12 {
        let outcome = flock.deliver_message("m-dialog", 100.0 + attempt as f64, &mut pane, || None);
        assert_eq!(DeliveryOutcome::Held, outcome);
        if attempt == 0 {
            let held = &flock.outbox_snapshot()[P][0];
            assert_eq!(
                Some(format!("{BOX_HOLD} (dialog)").as_str()),
                held.detail.as_deref()
            );
        }
    }

    let held = &flock.outbox_snapshot()[P][0];
    assert_eq!("queued", held.status);
    assert_eq!(12, held.holds);
    assert!(flock.message_history.lock().unwrap().is_empty());

    let events = metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    let held_events: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "message.held")
        .collect();
    assert_eq!(12, held_events.len());
    assert!(held_events
        .iter()
        .all(|event| event.reason.as_deref() == Some("dialog")));
    assert!(!events
        .iter()
        .any(|event| event.kind == "message.undeliverable"));
}

#[test]
fn deliver_message_does_not_use_permission_or_dead_session_detail_as_block_reason() {
    use crate::flock::panes::Stores;
    use crate::metrics::MetricsStore;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(MetricsStore::open(":memory:").unwrap());
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.attach_stores(Stores {
        metrics: Some(metrics.clone()),
        ..Stores::default()
    });
    let mut pane = Pane::default();

    for (id, dead, permission_detail) in [
        ("m-permission", false, Some("disabled")),
        ("m-dead", true, None),
    ] {
        pane.gone = dead;
        let mut message = Message::new(id, P, "pane", "hello", "test");
        if permission_detail.is_some() {
            message.hold_while_typing = false;
        }
        flock.outbox.lock().unwrap().insert(P.into(), vec![message]);

        let attempts = if dead { 10 } else { 12 };
        for attempt in 0..attempts {
            let outcome = flock.deliver_message(id, 100.0 + attempt as f64, &mut pane, || {
                permission_detail.map(str::to_owned)
            });
            assert_eq!(
                if dead && attempt == 9 {
                    DeliveryOutcome::Undeliverable
                } else {
                    DeliveryOutcome::Held
                },
                outcome
            );
            if attempt == 0 {
                let held = &flock.outbox_snapshot()[P][0];
                let expected_detail = permission_detail
                    .or_else(|| dead.then_some("the lane's recorded process is gone"));
                assert_eq!(expected_detail, held.detail.as_deref());
            }
        }
        if !dead {
            assert_eq!(12, flock.outbox_snapshot()[P][0].holds);
            flock.outbox.lock().unwrap().shift_remove(P);
        }
    }

    let history = flock.message_history.lock().unwrap();
    let terminal = history
        .iter()
        .find(|message| message.id == "m-dead")
        .unwrap();
    assert_eq!("undeliverable", terminal.status);
    assert!(history.iter().all(|message| message.id != "m-permission"));
    drop(history);

    let events = metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    assert!(events
        .iter()
        .filter(|event| event.kind == "message.held" || event.kind == "message.undeliverable")
        .all(|event| event.reason.is_none()));
}

#[test]
fn python_hold_signature_list_survives_restore_and_resnapshot() {
    let python_row = serde_json::json!({
        "id": "m-held",
        "pane_id": P,
        "target_label": "worker",
        "text": "held text",
        "source": "test",
        "hold_signature": ["modal", "Press Enter to confirm"],
        "holds": 2,
        "next_attempt_at": 80.0
    });

    let message = message_from_snapshot(&python_row).unwrap();
    assert_eq!(Some("modal"), message.hold_signature_kind.as_deref());
    assert_eq!(
        Some("Press Enter to confirm"),
        message.hold_signature.as_deref()
    );
    assert_eq!(
        serde_json::json!(["modal", "Press Enter to confirm"]),
        message_snapshot(&message)["hold_signature"]
    );
}

#[test]
fn hold_signature_preserves_modal_kind_and_collapsed_screen_text() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut pane = Pane::default();
    pane.screens.insert(
        P.into(),
        "Select Model and Effort\n  enter   select · esc back\n".into(),
    );

    assert_eq!(
        (
            "modal".to_owned(),
            "Select Model and Effort enter select · esc back".to_owned()
        ),
        hold_signature(&flock, &pane, P)
    );
}

#[test]
fn hold_signature_preserves_owned_ambiguous_composer_region() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let owned = "queued message owned by this flock";
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert(P.into(), vec![owned.into()]);
    let mut pane = Pane::default();
    pane.screens
        .insert(P.into(), format!("❯ {owned}\n\n\nstatus · ready\n"));

    assert_eq!(
        ("box".into(), owned.into()),
        hold_signature(&flock, &pane, P)
    );
}

#[test]
fn deliver_message_exhaustion_removes_once_remembers_and_releases_waiter() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let message = Message::new("m-exhausted", P, "pane", "hello", "test");
    let waiter = message.done.clone();
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane {
        gone: true,
        ..Pane::default()
    };

    const HOLD_LIMIT: i64 = 10;
    for attempt in 0..HOLD_LIMIT {
        let result =
            flock.deliver_message("m-exhausted", 100.0 + attempt as f64, &mut pane, || None);
        assert_eq!(
            if attempt + 1 == HOLD_LIMIT {
                DeliveryOutcome::Undeliverable
            } else {
                DeliveryOutcome::Held
            },
            result
        );
    }

    // Python removes the row now, and removes the empty pane only on the next worker step.
    assert!(flock.outbox_snapshot()[P].is_empty());
    assert_eq!(
        outbox_step(
            &flock,
            &mut pane,
            P,
            201.0,
            &std::sync::atomic::AtomicBool::new(false),
            &|_| false,
            &mut || None
        ),
        OutboxStep::Empty
    );
    assert!(flock.outbox_snapshot().is_empty());
    assert!(waiter.is_set());
    assert_eq!(
        Some("undeliverable"),
        flock
            .decided
            .lock()
            .unwrap()
            .by_id
            .get("m-exhausted")
            .map(|row| row.status.as_str())
    );
    assert_eq!(
        DeliveryOutcome::AlreadyDecided,
        flock.deliver_message("m-exhausted", 200.0, &mut pane, || None)
    );
}

#[test]
fn deliver_message_terminal_action_failure_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m-failed", P, "pane", "hello", "test");
    message.hold_while_typing = false;
    let waiter = message.done.clone();
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane {
        receipt_ok: false,
        ..Pane::default()
    };
    pane.panes = vec![serde_json::json!({
        "pane_id": P,
        "pane_pid": std::process::id(),
    })];
    pane.attempt_generation_script.borrow_mut().extend([
        Some("pid:123:start:captured".into()),
        Some("pid:456:start:replacement".into()),
    ]);

    assert_eq!(
        DeliveryOutcome::Failed,
        flock.deliver_message("m-failed", 100.0, &mut pane, || None)
    );
    assert_eq!(1, pane.typed.len());
    assert_eq!(
        Some("pid:123:start:captured".into()),
        pane.typed_attempts[0].3
    );
    assert_eq!(
        Some("pid:456:start:replacement".into()),
        pane.attempt_generation_script
            .borrow()
            .front()
            .cloned()
            .flatten(),
        "submit_text must not reread the generation after delivery captured it"
    );
    assert!(waiter.is_set());
    // Python removes the row now, and removes the empty pane only on the next worker step.
    assert!(flock.outbox_snapshot()[P].is_empty());
    assert_eq!(
        outbox_step(
            &flock,
            &mut pane,
            P,
            201.0,
            &std::sync::atomic::AtomicBool::new(false),
            &|_| false,
            &mut || None
        ),
        OutboxStep::Empty
    );
    assert!(flock.outbox_snapshot().is_empty());
    assert_eq!(
        DeliveryOutcome::AlreadyDecided,
        flock.deliver_message("m-failed", 200.0, &mut pane, || None)
    );
    assert_eq!(1, pane.typed.len());
}

#[test]
fn outbox_step_respects_due_time_and_releases_queue_lock_before_reads() {
    use std::sync::atomic::AtomicBool;
    use std::sync::TryLockError;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m-due", P, "pane", "send me", "test");
    message.next_attempt_at = Some(105.0);
    message.hold_signature_kind = Some("box".into());
    message.hold_signature = Some("human draft".into());
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane::default();
    pane.screens
        .insert(P.into(), submit_pending_screen("human draft"));
    let stopped = AtomicBool::new(false);
    let waited = AtomicBool::new(false);

    let result = outbox_step(
        &flock,
        &mut pane,
        P,
        104.0,
        &stopped,
        &|_| {
            waited.store(true, std::sync::atomic::Ordering::SeqCst);
            false
        },
        &mut || None,
    );

    assert_eq!(OutboxStep::Due, result);
    assert!(!waited.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        flock.outbox.try_lock(),
        Ok(_) | Err(TryLockError::Poisoned(_))
    ));
    assert_eq!(0, flock.outbox_snapshot()[P][0].holds);
}

#[test]
fn outbox_step_clears_backoff_when_the_box_signature_changes() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m-changed", P, "pane", "send me", "test");
    message.next_attempt_at = Some(500.0);
    message.hold_signature_kind = Some("box".into());
    message.hold_signature = Some("old draft".into());
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane::default();
    pane.screens
        .insert(P.into(), submit_pending_screen("new draft"));
    let stopped = AtomicBool::new(false);
    let waits = std::cell::Cell::new(0);

    let result = outbox_step(
        &flock,
        &mut pane,
        P,
        100.0,
        &stopped,
        &|_| {
            waits.set(waits.get() + 1);
            false
        },
        &mut || None,
    );

    assert_eq!(OutboxStep::Held, result);
    assert_eq!(1, waits.get()); // optimistic settle runs before the guarded draft recheck
    let held = &flock.outbox_snapshot()[P][0];
    assert_eq!(1, held.holds);
    assert_eq!(Some(105.0), held.next_attempt_at);
}

#[test]
fn outbox_step_defers_a_busy_non_queueing_harness_without_typing_or_exhausting() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.outbox.lock().unwrap().insert(
        P.into(),
        vec![Message::new(
            "m-busy-deferred",
            P,
            "pane",
            "send me",
            "test",
        )],
    );
    let mut pane = busy_codex();
    pane.harness.insert(P.into(), "unknown".into());
    let stopped = AtomicBool::new(false);
    let mut now = 100.0;

    for _ in 0..12 {
        assert_eq!(
            OutboxStep::Held,
            outbox_step(&flock, &mut pane, P, now, &stopped, &|_| false, &mut || {
                None
            },)
        );
        let queue = flock.outbox_snapshot();
        assert_eq!(1, queue[P].len());
        assert_eq!("queued", queue[P][0].status);
        now = queue[P][0].next_attempt_at.unwrap();
    }

    assert_eq!(12, flock.outbox_snapshot()[P][0].holds);
    assert!(pane.typed.is_empty());
    assert!(pane.keys.is_empty());
}

#[test]
fn outbox_step_only_attempts_the_fifo_head_and_never_waits_with_queue_locked() {
    use std::sync::atomic::AtomicBool;
    use std::sync::TryLockError;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let first = Message::new("m-first", P, "pane", "first", "test");
    let second = Message::new("m-second", P, "pane", "second", "test");
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![first, second]);
    let mut pane = Pane::default();
    pane.statuses.insert(P.into(), "idle".into());
    pane.screens.insert(P.into(), hint_screen());
    let stopped = AtomicBool::new(false);
    let wait_saw_unlocked_queue = std::cell::Cell::new(false);

    let result = outbox_step(
        &flock,
        &mut pane,
        P,
        100.0,
        &stopped,
        &|_| {
            wait_saw_unlocked_queue.set(matches!(
                flock.outbox.try_lock(),
                Ok(_) | Err(TryLockError::Poisoned(_))
            ));
            false
        },
        &mut || None,
    );

    assert_eq!(OutboxStep::Delivered, result);
    assert!(wait_saw_unlocked_queue.get());
    let queue = flock.outbox_snapshot();
    assert_eq!(1, queue[P].len());
    assert_eq!("m-second", queue[P][0].id);
    assert_eq!(0, queue[P][0].holds);
    assert_eq!("m-first", flock.message_history.lock().unwrap()[0].id);
}

#[test]
fn record_delivery_outcome_ignores_receipt_errors_and_still_settles_waiter() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::{Outgoing, Store};
    use rusqlite::Connection;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let inbox_id = inbox
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("notice"),
            ..Outgoing::default()
        })
        .unwrap();
    let observer = Connection::open(&inbox_path).unwrap();
    observer
        .execute_batch(
            "CREATE TRIGGER reject_notify BEFORE UPDATE OF notify_state ON messages
             BEGIN SELECT RAISE(ABORT, 'store unavailable'); END;",
        )
        .unwrap();

    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    let mut message = Message::new("m1", "%1", "writer", "body", "direct");
    message.notify_for.push(inbox_id);

    flock.record_delivery_outcome(&message, false, Some("closed"), "undeliverable", None);
    message.done.wait();

    let actual_state: Option<String> = observer
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [&message.notify_for[0]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(None, actual_state);
}

#[test]
fn remember_bounds_history_and_decided_tombstones_and_keeps_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);

    for index in 0..501 {
        let mut message = Message::new(
            &format!("m{index}"),
            &format!("%{index}"),
            "writer",
            "body",
            "direct",
        );
        message.status = "sent".into();
        message.press_enter = false;
        message.created_at = index as f64;
        flock.remember(&message);
    }

    let history = flock.message_history.lock().unwrap();
    assert_eq!(50, history.len());
    assert_eq!("m451", history.front().unwrap().id);
    assert_eq!("m500", history.back().unwrap().id);
    drop(history);

    let decided = flock.decided.lock().unwrap();
    assert_eq!(500, decided.by_id.len());
    assert!(!decided.by_id.contains_key("m0"));
    assert_eq!(Some(500.0), decided.by_id["m500"].created_at);
    drop(decided);

    let candidates = flock.stuck_candidates.lock().unwrap();
    assert_eq!(501, candidates.len());
    assert!(candidates.values().all(|candidate| !candidate.eligible));
}

#[test]
fn remember_refreshes_created_at_for_a_reused_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut first = Message::new("m1", "%1", "writer", "first", "direct");
    first.status = "sent".into();
    first.created_at = 10.0;
    flock.remember(&first);

    let mut reused = Message::new("m1", "%1", "writer", "reused", "direct");
    reused.status = "failed".into();
    reused.created_at = 20.0;
    flock.remember(&reused);

    let decided = flock.decided.lock().unwrap();
    assert_eq!(1, decided.by_id.len());
    assert_eq!("failed", decided.by_id["m1"].status);
    assert_eq!(Some(20.0), decided.by_id["m1"].created_at);
}

#[test]
fn persist_typed_attempt_saves_candidate_before_submit_and_restores_message_status() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), true);
    flock
        .attach_status_state(super::status_runtime::StatusState::at_start(
            "test",
            dir.path().join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    let mut message = Message::new("m1", "%1", "worker", "typed text", "send_text");
    message.created_at = 7.0;
    message.attempt_generation = Some("pid:123:start:456".into());

    let saved = flock
        .persist_typed_attempt(
            &mut message,
            "%1",
            "typed text",
            true,
            &serde_json::json!({}),
        )
        .unwrap();

    assert!(saved);
    assert_eq!(
        "queued", message.status,
        "the caller keeps its prior status"
    );
    let candidate = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!("typed", candidate.status);
    assert!(candidate.typed_at.is_some());
    assert!(
        candidate.native_binding.is_none(),
        "native lookup failure keeps the strict candidate"
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!("typed", snapshot["stuck_candidates"]["%1"]["status"]);
    assert!(snapshot["stuck_candidates"]["%1"]["typed_at"].is_number());
}

#[test]
fn persist_typed_attempt_refuses_enter_when_snapshot_save_fails_and_restores_status() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"file").unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), true);
    flock
        .attach_status_state(super::status_runtime::StatusState::at_start(
            "test",
            blocker.join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    let mut message = Message::new("m2", "%2", "worker", "must not submit", "send_text");
    message.attempt_generation = Some("pid:123:start:456".into());

    assert!(flock
        .persist_typed_attempt(
            &mut message,
            "%2",
            "must not submit",
            true,
            &serde_json::json!({})
        )
        .is_err());

    assert_eq!(
        "queued", message.status,
        "a failed receipt save refuses Enter"
    );
    let candidate = flock.stuck_candidates.lock().unwrap()["%2"].clone();
    assert_eq!("typed", candidate.status);
    assert!(candidate.typed_at.is_some());
}

#[test]
fn persist_typed_attempt_without_matching_message_context_does_not_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m3", "%3", "worker", "owned text", "send_text");

    assert!(flock
        .persist_typed_attempt(
            &mut message,
            "%3",
            "different text",
            true,
            &serde_json::json!({})
        )
        .unwrap());

    assert_eq!("queued", message.status);
    assert!(message.typed_at.is_none());
    assert!(flock.stuck_candidates.lock().unwrap().is_empty());
}

#[test]
fn native_binding_requires_the_original_candidate_and_attempt_generation() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m4", "%4", "worker", "native", "send_text");
    message.status = "typed".into();
    message.created_at = 4.0;
    message.attempt_generation = Some("pid:123:start:456".into());
    flock.note_stuck_candidate(&message);
    let binding = crate::recovery_native::PromptSessionBinding {
        pid: 123,
        start: "456".into(),
        session_id: "session-id".into(),
        rollout_path: "/unused/rollout.jsonl".into(),
        cwd: "/unused".into(),
    };

    assert!(!flock.bind_native_candidate(
        "%4",
        "m4",
        4.0,
        Some("pid:123:start:replacement"),
        &binding,
    ));
    assert!(!flock.bind_native_candidate("%4", "m4", 5.0, Some("pid:123:start:456"), &binding));
    assert!(flock.bind_native_candidate("%4", "m4", 4.0, Some("pid:123:start:456"), &binding));

    assert_eq!(
        Some((123, "456".into(), "session-id".into())),
        flock.stuck_candidates.lock().unwrap()["%4"].native_binding
    );
}

fn recovery_candidate(flock: &Flock, id: &str, created_at: f64, notify_for: Vec<String>) {
    let mut message = Message::new(id, "%1", "writer", "stuck body", "send_text");
    message.created_at = created_at;
    message.status = "failed".into();
    message.attempt_generation = Some("pid:45:start:6".into());
    message.notify_for = notify_for;
    flock.note_stuck_candidate(&message);
}

fn persistent_recovery_flock(dir: &std::path::Path, state_path: &std::path::Path) -> Flock {
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), true);
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            state_path,
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    flock
}

#[test]
fn unwedge_reserves_before_one_key_and_does_not_claim_unchanged_text() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state);
    {
        let mut config = flock.stuck_config.lock().unwrap();
        config.threshold_s = 1.0;
        config.observation_interval_s = 0.1;
        config.max_attempts = 1;
    }
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id": "%1", "pane_pid": std::process::id()}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.created_at = 1.0;
    message.status = "failed".into();
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);

    assert!(!flock.unwedge(
        &mut env,
        "%1",
        "stuck body",
        Some(98.0),
        Some(&message),
        100.0,
        None,
        true
    ));
    assert_eq!(vec![("%1".into(), "Enter".into())], env.keys);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    assert!(saved.to_string().contains("spent"));
    assert!(!flock.unwedge(
        &mut env,
        "%1",
        "stuck body",
        None,
        Some(&message),
        102.0,
        None,
        true
    ));
    assert_eq!(1, env.keys.len());
}

#[test]
fn unwedge_verifies_a_newly_cleared_composer_after_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state);
    {
        let mut config = flock.stuck_config.lock().unwrap();
        config.threshold_s = 1.0;
        config.observation_interval_s = 0.1;
    }
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id": "%1", "pane_pid": std::process::id()}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    env.after_key_screen = Some(frame("\x1b[2mAsk Codex to do anything\x1b[0m", &[]));
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.created_at = 1.0;
    message.status = "failed".into();
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);
    assert!(flock.unwedge(
        &mut env,
        "%1",
        "stuck body",
        Some(98.0),
        Some(&message),
        100.0,
        None,
        true
    ));
    assert_eq!(1, env.keys.len());
    assert_eq!(
        flock.stuck_candidates.lock().unwrap()["%1"]
            .recovery_outcome
            .as_deref(),
        Some("consumed")
    );
}

#[test]
fn unwedge_refuses_disabled_persistence_and_edited_text() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id": "%1", "pane_pid": std::process::id()}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens
        .insert("%1".into(), frame("stuck body plus edit", &[]));
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.status = "failed".into();
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);
    assert!(!flock.unwedge(
        &mut env,
        "%1",
        "stuck body plus edit",
        Some(1.0),
        Some(&message),
        200.0,
        None,
        true
    ));
    assert!(env.keys.is_empty());
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    assert!(!flock.unwedge(
        &mut env,
        "%1",
        "stuck body",
        Some(1.0),
        Some(&message),
        400.0,
        None,
        true
    ));
    assert!(env.keys.is_empty());
}

#[test]
fn owned_recovery_evidence_preserves_malformed_binding_as_a_refusal() {
    let row = serde_json::json!({
        "message_id": "m1", "created_at": 1.0, "text": "stuck body",
        "generation": "pid:1:start:2", "eligible": true,
        "native_binding": [1, "2", "session", "extra"]
    });
    let candidate = candidate_from_snapshot(&row).unwrap();
    assert!(candidate.native_binding.is_none());
    assert_eq!(
        candidate.native_binding_invalid,
        Some(row["native_binding"].clone())
    );
    let round_trip = candidate_snapshot("%1", &candidate);
    assert_eq!(round_trip["native_binding"], row["native_binding"]);
    assert!(round_trip.get("typed_at").is_none());
}

#[test]
fn stuck_status_never_resolves_busy_dead_modal_or_unreadable_to_ready() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let mut env = Pane::default();
    assert_eq!(flock.stuck_status(&env, "%1"), "unknown");
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    assert_eq!(flock.stuck_status(&env, "%1"), "unknown");
    env.statuses.insert("%1".into(), "working".into());
    assert_eq!(flock.stuck_status(&env, "%1"), "working");
    env.statuses.insert("%1".into(), "idle".into());
    env.gone = true;
    assert_eq!(flock.stuck_status(&env, "%1"), "dead");
    env.gone = false;
    env.screens.insert(
        "%1".into(),
        "Press Enter to confirm or Esc to cancel\n".into(),
    );
    assert_eq!(flock.stuck_status(&env, "%1"), "blocked");
}

#[test]
fn stuck_text_is_ours_requires_complete_matching_text_and_readable_screen() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let mut env = Pane::default();
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    assert!(!flock.stuck_text_is_ours(&env, "%1", Some("stuck body"), "stuck body"));
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    assert!(flock.stuck_text_is_ours(&env, "%1", Some("stuck body"), "stuck body"));
    assert!(!flock.stuck_text_is_ours(&env, "%1", Some("stuck body"), "stuck body plus edit"));
}

#[test]
fn press_stuck_enter_refuses_replaced_pane_before_sending_key() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id": "%1", "pane_pid": std::process::id()}));
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    let (pressed, _) = flock.press_stuck_enter(
        &mut env,
        "%1",
        &crate::stuck::fingerprint("stuck body"),
        "pid:wrong:start:wrong",
        "m1",
        Some("stuck body"),
        None,
        false,
        None,
    );
    assert!(!pressed);
    assert!(env.keys.is_empty());
}

#[test]
fn press_stuck_enter_honors_backend_refusal_and_busy_status() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id":"%1", "pane_pid":std::process::id()}));
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    env.statuses.insert("%1".into(), "working".into());
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    let generation = pane_generation(&env, "%1").unwrap();
    let fingerprint = crate::stuck::fingerprint("stuck body");
    let (pressed, _) = flock.press_stuck_enter(
        &mut env,
        "%1",
        &fingerprint,
        &generation,
        "m1",
        Some("stuck body"),
        None,
        false,
        None,
    );
    assert!(!pressed);
    assert!(env.events.is_empty());
    env.statuses.insert("%1".into(), "idle".into());
    env.key_refusal = Some("backend refused".into());
    let (pressed, detail) = flock.press_stuck_enter(
        &mut env,
        "%1",
        &fingerprint,
        &generation,
        "m1",
        Some("stuck body"),
        None,
        false,
        None,
    );
    assert!(!pressed);
    assert_eq!(detail, "backend refused");
    assert_eq!(env.events, vec!["key"]);
    assert!(env.keys.is_empty());
}

#[test]
fn press_stuck_enter_refuses_intent_superseded_during_final_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id":"%1", "pane_pid":std::process::id()}));
    env.screens.insert("%1".into(), frame("stuck body", &[]));
    env.statuses.insert("%1".into(), "idle".into());
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec!["stuck body".into()]);
    let generation = pane_generation(&env, "%1").unwrap();
    let mut original = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    original.created_at = 1.0;
    original.status = "failed".into();
    original.attempt_generation = Some(generation.clone());
    flock.note_stuck_candidate(&original);
    let mut replacement = Message::new("m2", "%1", "writer", "new intent", "send_text");
    replacement.created_at = 2.0;
    replacement.status = "cancelled".into();
    replacement.press_enter = false;
    let owner = std::sync::Arc::clone(&flock);
    let mut reads = 0;
    *env.on_read.borrow_mut() = Some(Box::new(move || {
        reads += 1;
        if reads == 2 {
            owner.note_stuck_candidate(&replacement);
        }
    }));
    let (pressed, _) = flock.press_stuck_enter(
        &mut env,
        "%1",
        &crate::stuck::fingerprint("stuck body"),
        &generation,
        "m1",
        Some("stuck body"),
        Some(&original),
        true,
        Some(1.0),
    );
    assert!(!pressed);
    assert_eq!(
        flock.stuck_candidates.lock().unwrap()["%1"].message_id,
        "m2"
    );
    assert!(env.keys.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn scheduled_recovery_requires_current_schedule_for_styled_and_real_native_evidence() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hooks = dir.path().join("hooks");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let binary = dir.path().join("codex");
    std::fs::copy("/bin/sleep", &binary).unwrap();
    let child = std::process::Command::new(binary)
        .arg("300")
        .env("CODEX_HOME", &home)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", &hooks)
        .spawn()
        .unwrap();
    let child = ChildGuard(child);
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    let session = "01a173ee-beed-7461-a560-e5f6c1f666a7";
    let rollout = home.join(format!("rollout-{session}.jsonl"));
    std::fs::write(
        &rollout,
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta", "payload":{"session_id":session,"cwd":cwd}})
        ),
    )
    .unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute(
        "create table threads (id text primary key, rollout_path text, cwd text)",
        [],
    )
    .unwrap();
    db.execute(
        "insert into threads values (?,?,?)",
        (session, rollout.to_str().unwrap(), cwd.to_str().unwrap()),
    )
    .unwrap();
    std::fs::write(hooks.join("bound.json"), serde_json::json!({"session_id":session,"pane_id":"%1","root_pid":pid,"root_start":start,"cwd":cwd}).to_string()).unwrap();

    for native in [false, true] {
        for (present, running, template, target, now, expected) in [
            (true, true, "instruction", "%1", 101.0, true),
            (true, false, "instruction", "%1", 101.0, false),
            (true, true, "changed", "%1", 101.0, false),
            (true, true, "instruction", "%1", 1000.0, false),
            (true, true, "instruction", "%1", 99.0, false),
            (false, true, "instruction", "%1", 101.0, false),
            (true, true, "instruction", "%2", 101.0, false),
        ] {
            let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
            let rows = if present {
                serde_json::json!({"dunkers":[{"id":"d1","target_pane_id":target,"text":template,"running":running}]})
            } else {
                serde_json::json!({"dunkers":[]})
            };
            flock.dunks.load(rows.as_object().unwrap());
            let mut env = Pane::default();
            env.panes
                .push(serde_json::json!({"pane_id":"%1","pane_pid":pid,"agent":"codex","cwd":cwd}));
            env.statuses.insert("%1".into(), "idle".into());
            flock
                .sent_text_by_pane
                .lock()
                .unwrap()
                .insert("%1".into(), vec!["previous".into(), "stuck body".into()]);
            let screen = if native {
                "› stuck body\n\n  model · /tmp/test\n".to_string()
            } else {
                "\x1b[1;2m›\x1b[0m previous\n\x1b[1m›\x1b[0m stuck body\n\n\n  model · /tmp/test\n"
                    .to_string()
            };
            let mut message = Message::new("m1", "%1", "writer", "stuck body", "d1");
            message.status = "sent".into();
            message.scheduled = true;
            message.created_at = 100.0;
            message.schedule_template = Some("instruction".into());
            message.attempt_generation = pane_generation(&env, "%1");
            message.typed_at = native.then_some(100.0);
            flock.note_stuck_candidate(&message);
            let entry = flock.stuck_candidates.lock().unwrap()["%1"].clone();
            if native {
                assert!(
                    flock.owned_recovery_evidence(&env, "%1", &screen).is_some(),
                    "native fixture must authenticate"
                );
            }
            assert_eq!(
                flock.sent_schedule_pending(&env, "%1", &entry, &screen, now),
                expected,
                "native={native} running={running} template={template} now={now}"
            );
            if native && expected {
                env.screens.insert("%1".into(), screen.clone());
                let generation = message.attempt_generation.as_deref().unwrap();
                for (old_id, old_hash, expected_spent) in [
                    ("m1", "dbe71a6e223b4ec1", u32::MAX),
                    ("m1@100", "dbe71a6e223b4ec1", u32::MAX),
                    ("m1@100", "51d09990b82bd9c5", u32::MAX),
                    ("m1@100", "b2d4ddd55ad59b5e", 1),
                ] {
                    *flock.stuck_tracker.lock().unwrap() = crate::stuck::Tracker::default();
                    flock.stuck_last_check.lock().unwrap().clear();
                    let old_key =
                        crate::stuck::ledger_key("%1", old_id, Some(generation), old_hash);
                    flock.stuck_tracker.lock().unwrap().spent.insert(old_key, 1);
                    assert!(!flock.unwedge(
                        &mut env,
                        "%1",
                        "stuck body",
                        Some(1.0),
                        Some(&message),
                        101.0,
                        None,
                        true
                    ));
                    let canonical = crate::stuck::fingerprint("stuck body");
                    let migrated =
                        crate::stuck::ledger_key("%1", "m1@100.0", Some(generation), &canonical);
                    assert_eq!(
                        flock.stuck_tracker.lock().unwrap().spent[&migrated],
                        expected_spent
                    );
                    assert!(env.keys.is_empty());
                }
                assert!(flock
                    .owned_recovery_evidence(&env, "%1", "› human edit\n\n  model · /tmp/test\n")
                    .is_none());
                flock
                    .stuck_candidates
                    .lock()
                    .unwrap()
                    .get_mut("%1")
                    .unwrap()
                    .native_binding = Some((pid, "wrong-start".into(), session.into()));
                assert!(flock.owned_recovery_evidence(&env, "%1", &screen).is_none());
            }
        }
    }
}

#[test]
fn reconcile_stuck_requires_the_live_message_and_created_at_identity() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    recovery_candidate(&flock, "m1", 1.0, Vec::new());
    let mut newer = Message::new("m2", "%1", "writer", "cancelled", "send_text");
    newer.created_at = 2.0;
    newer.status = "cancelled".into();
    newer.press_enter = false;
    flock.note_stuck_candidate(&newer);

    flock.reconcile_stuck("%1", "m1", "failed", "old failure", Some(1.0));
    flock.reconcile_stuck("%1", "m2", "consumed", "missing stamp", None);
    flock.reconcile_stuck("%1", "m2", "consumed", "wrong stamp", Some(1.0));

    let candidate = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!("m2", candidate.message_id);
    assert_eq!(2.0, candidate.created_at);
    assert_eq!("cancelled", candidate.status);
    assert_eq!(None, candidate.recovery_outcome);
    assert!(!candidate.eligible);
}

#[test]
fn reconcile_stuck_retires_consumed_and_exhausted_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    recovery_candidate(&flock, "m4", 4.0, Vec::new());

    flock.reconcile_stuck("%1", "m4", "exhausted", "attempts spent", Some(4.0));
    let exhausted = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!(Some("exhausted"), exhausted.recovery_outcome.as_deref());
    assert_eq!(Some("attempts spent"), exhausted.recovery_detail.as_deref());
    assert!(exhausted.recovered_at.is_some());
    assert!(!exhausted.eligible);
    assert_eq!("failed", exhausted.status);

    flock.reconcile_stuck("%1", "m4", "consumed", "composer cleared", Some(4.0));
    let consumed = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!(Some("consumed"), consumed.recovery_outcome.as_deref());
    assert_eq!("consumed", consumed.status);
    assert!(!consumed.eligible);
}

#[test]
fn consumed_receipt_is_not_published_when_candidate_save_fails() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::{Outgoing, Store};
    use rusqlite::Connection;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let inbox_id = inbox
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("notice"),
            ..Outgoing::default()
        })
        .unwrap();
    let observer = Connection::open(&inbox_path).unwrap();
    observer
        .execute_batch(
            "CREATE TABLE notify_updates (count INTEGER NOT NULL);
             INSERT INTO notify_updates VALUES (0);
             CREATE TRIGGER count_notify_updates AFTER UPDATE OF notify_state ON messages
             BEGIN UPDATE notify_updates SET count = count + 1; END;",
        )
        .unwrap();

    let state_path = dir.path().join("state.json");
    std::fs::create_dir(&state_path).unwrap();
    let flock = persistent_recovery_flock(dir.path(), &state_path);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    recovery_candidate(&flock, "m5", 5.0, vec![inbox_id.clone()]);

    flock.reconcile_stuck("%1", "m5", "consumed", "composer cleared", Some(5.0));

    let status: Option<String> = observer
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [&inbox_id],
            |row| row.get(0),
        )
        .unwrap();
    let update_count: i64 = observer
        .query_row("SELECT count FROM notify_updates", [], |row| row.get(0))
        .unwrap();
    let candidate = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!(None, status);
    assert_eq!(0, update_count);
    assert_eq!(Some("consumed"), candidate.recovery_outcome.as_deref());
    assert_eq!(None, candidate.receipts_reconciled);
}

#[test]
fn missing_recovery_detail_leaves_receipt_pending() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::{Outgoing, Store};
    use rusqlite::Connection;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let inbox_id = inbox
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("notice"),
            ..Outgoing::default()
        })
        .unwrap();
    let observer = Connection::open(&inbox_path).unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    recovery_candidate(&flock, "m7", 7.0, vec![inbox_id.clone()]);
    {
        let mut candidates = flock.stuck_candidates.lock().unwrap();
        let candidate = candidates.get_mut("%1").unwrap();
        candidate.recovery_outcome = Some("consumed".into());
        candidate.recovery_detail = None;
        candidate.status = "consumed".into();
        candidate.eligible = false;
    }
    let entry = flock.stuck_candidates.lock().unwrap()["%1"].clone();

    flock.publish_stuck_receipt("%1", &entry);

    let state: Option<String> = observer
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [&inbox_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(None, state);
    assert_eq!(
        None,
        flock.stuck_candidates.lock().unwrap()["%1"].receipts_reconciled
    );
}

#[test]
fn partial_receipt_failure_stays_pending_and_retry_completes_without_new_recovery() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::{Outgoing, Store};
    use rusqlite::Connection;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let inbox_ids = (0..2)
        .map(|index| {
            inbox
                .send(&Outgoing {
                    mailbox: "writer",
                    body: Some(&format!("notice {index}")),
                    ..Outgoing::default()
                })
                .unwrap()
        })
        .collect::<Vec<_>>();
    let observer = Connection::open(&inbox_path).unwrap();
    observer
        .execute_batch(&format!(
            "CREATE TRIGGER reject_second_notify BEFORE UPDATE OF notify_state ON messages
             WHEN OLD.id = '{}'
             BEGIN SELECT RAISE(ABORT, 'store unavailable'); END;",
            inbox_ids[1]
        ))
        .unwrap();

    let state_path = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state_path);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    recovery_candidate(&flock, "m6", 6.0, inbox_ids.clone());

    flock.reconcile_stuck("%1", "m6", "consumed", "composer cleared", Some(6.0));
    let first_state: Option<String> = observer
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [&inbox_ids[0]],
            |row| row.get(0),
        )
        .unwrap();
    let second_state: Option<String> = observer
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [&inbox_ids[1]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(Some("sent".into()), first_state);
    assert_eq!(None, second_state);
    assert_eq!(
        None,
        flock.stuck_candidates.lock().unwrap()["%1"].receipts_reconciled
    );

    observer
        .execute_batch("DROP TRIGGER reject_second_notify;")
        .unwrap();
    let entry = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    flock.publish_stuck_receipt("%1", &entry);

    for inbox_id in &inbox_ids {
        let (state, reason): (Option<String>, Option<String>) = observer
            .query_row(
                "SELECT notify_state, notify_reason FROM messages WHERE id = ?1",
                [inbox_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(Some("sent".into()), state);
        assert_eq!(Some("recovered: composer cleared".into()), reason);
    }
    assert_eq!(
        Some(true),
        flock.stuck_candidates.lock().unwrap()["%1"].receipts_reconciled
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
    assert_eq!(
        true,
        payload["stuck_candidates"]["%1"]["receipts_reconciled"]
    );
}

#[test]
fn cancelled_inflight_non_io_save_failure_is_not_a_successful_hold() {
    use std::sync::Arc;
    struct CancelSave;
    impl SnapshotStore for CancelSave {
        fn save(&self, _: &Flock) -> Result<(), DunkError> {
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    // Missing startup state is a non-I/O persistence failure, not a filesystem OSError.
    let flock = Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        true,
    ));
    let mut message = Message::new("cancel-save", P, "worker", "body", "test");
    message.hold_while_typing = false;
    message.press_enter = false;
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![message.clone()]);
    let callback_flock = Arc::clone(&flock);
    let mut pane = Pane {
        send_refusal: Some(format!("{BOX_HOLD} (controlled-refusal)")),
        ..Default::default()
    };
    pane.on_send_text.replace(Some(Box::new(move || {
        callback_flock
            .cancel_message(&serde_json::json!("cancel-save"), &CancelSave)
            .unwrap();
    })));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        flock.deliver_message("cancel-save", 100.0, &mut pane, || None)
    }))
    .expect_err("non-I/O save failure must terminate the worker operation");
    let error = failure
        .downcast_ref::<DunkError>()
        .expect("preserve the typed persistence error");
    assert!(error.to_string().contains("status startup state"));
    assert!(message.done.is_set());
    assert!(flock.outbox_snapshot()[P].is_empty());
    let history = flock.message_history.lock().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "cancelled");
    assert_eq!(history[0].holds, 1);
    assert_eq!(
        history[0].detail.as_deref(),
        Some(format!("{BOX_HOLD} (controlled-refusal)").as_str())
    );
}

#[test]
fn completed_delivery_preserves_pane_slot_until_empty_step_or_reenqueue() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    let mut first = Message::new("first", P, "first", "body", "test");
    first.hold_while_typing = false;
    first.press_enter = false;
    flock.outbox.lock().unwrap().insert(P.into(), vec![first]);
    flock.outbox.lock().unwrap().insert(
        "a-later".into(),
        vec![Message::new("later", "a-later", "later", "body", "test")],
    );
    let mut pane = Pane {
        receipt_ok: true,
        ..Default::default()
    };
    assert_eq!(
        flock.deliver_message("first", 1.0, &mut pane, || None),
        DeliveryOutcome::Sent
    );
    assert!(flock.outbox_snapshot()[P].is_empty());
    assert_eq!(
        flock
            .outbox_snapshot()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [P, "a-later"]
    );
    flock
        .enqueue(
            Message::new("next", P, "next", "body", "test"),
            &NoopOutboxWake,
            &NoopSnapshotStore,
        )
        .unwrap();
    assert_eq!(
        flock
            .outbox_snapshot()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [P, "a-later"]
    );
    assert_eq!(flock.outbox_snapshot()[P][0].id, "next");
}

#[test]
fn cancelled_captured_message_survives_history_eviction_without_reinsertion() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    let mut original = Message::new("evicted", P, "original", "body", "test");
    original.hold_while_typing = false;
    original.press_enter = false;
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![original.clone()]);
    let callback_flock = Arc::clone(&flock);
    let mut pane = Pane {
        send_refusal: Some(format!("{BOX_HOLD} (controlled-refusal)")),
        ..Default::default()
    };
    pane.on_send_text.replace(Some(Box::new(move || {
        callback_flock
            .cancel_message(&serde_json::json!("evicted"), &NoopSnapshotStore)
            .unwrap();
        for index in 0..50 {
            let mut newer = Message::new(
                &format!("new-{index}"),
                "other",
                "other",
                "new body",
                "other",
            );
            newer.status = "sent".into();
            callback_flock.remember(&newer);
        }
    })));
    assert_eq!(
        flock.deliver_message("evicted", 100.0, &mut pane, || None),
        DeliveryOutcome::Held
    );
    assert!(original.done.is_set());
    assert!(flock.outbox_snapshot()[P].is_empty());
    let history = flock.message_history.lock().unwrap();
    assert_eq!(history.len(), 50);
    assert!(history.iter().all(|message| message.id != "evicted"));
}

/// Python `test_queued_messages_keep_their_order_per_pane`: three messages wait behind a human
/// draft and go out in the order they were queued. Each `outbox_step` touches only the queue head.
#[test]
fn test_queued_messages_keep_their_order_per_pane() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut queued = Vec::new();
    for text in ["first", "second", "third"] {
        let mut message = Message::new(&format!("m-{text}"), P, "pane", text, "test");
        message.press_enter = false;
        queued.push(message);
    }
    flock.outbox.lock().unwrap().insert(P.into(), queued);
    let mut pane = Pane {
        receipt_ok: true,
        ..Pane::default()
    };
    pane.statuses.insert(P.into(), "idle".into());
    pane.panes = vec![serde_json::json!({
        "pane_id": P,
        "pane_pid": std::process::id(),
    })];
    pane.screens
        .insert(P.into(), submit_pending_screen("human draft"));
    let stopped = AtomicBool::new(false);
    let step = |pane: &mut Pane, now: f64| {
        outbox_step(&flock, pane, P, now, &stopped, &|_| false, &mut || None)
    };
    let order = |flock: &Flock| -> Vec<String> {
        flock.outbox_snapshot()[P]
            .iter()
            .map(|message| message.id.clone())
            .collect()
    };

    assert_eq!(OutboxStep::Held, step(&mut pane, 100.0));
    assert!(pane.typed.is_empty(), "a human draft is never typed over");
    assert_eq!(vec!["m-first", "m-second", "m-third"], order(&flock));

    pane.screens.insert(P.into(), hint_screen());
    for _ in 0..3 {
        assert_eq!(OutboxStep::Delivered, step(&mut pane, 100.0));
    }
    assert_eq!(OutboxStep::Empty, step(&mut pane, 100.0));
    let sent: Vec<String> = flock
        .message_history
        .lock()
        .unwrap()
        .iter()
        .map(|message| message.id.clone())
        .collect();
    assert_eq!(vec!["m-first", "m-second", "m-third"], sent);
    let typed: Vec<String> = pane.typed.iter().map(|(_, text)| text.clone()).collect();
    assert_eq!(vec!["first", "second", "third"], typed);
}

/// Python `test_a_failed_send_is_reported_not_retried`: a send that fails is reported as failed,
/// is not left queued, and is never attempted again.
#[test]
fn test_a_failed_send_is_reported_not_retried() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut message = Message::new("m-doomed", P, "pane", "doomed", "test");
    message.hold_while_typing = false;
    message.press_enter = false;
    let waiter = message.done.clone();
    flock.outbox.lock().unwrap().insert(P.into(), vec![message]);
    let mut pane = Pane {
        receipt_ok: false,
        ..Pane::default()
    };
    pane.panes = vec![serde_json::json!({
        "pane_id": P,
        "pane_pid": std::process::id(),
    })];

    assert_eq!(
        DeliveryOutcome::Failed,
        flock.deliver_message("m-doomed", 100.0, &mut pane, || None)
    );
    assert!(
        waiter.is_set(),
        "the waiting sender is released with the failure"
    );
    {
        let history = flock.message_history.lock().unwrap();
        let last = history.back().expect("the failed send is recorded");
        assert_eq!(
            ("m-doomed", "failed"),
            (last.id.as_str(), last.status.as_str())
        );
    }
    let queued = flock
        .outbox_snapshot()
        .get(P)
        .map_or(0, |queue| queue.len());
    assert_eq!(0, queued, "a failed send is not left queued");
    assert_eq!(1, pane.typed.len(), "the send was attempted once");

    let stopped = AtomicBool::new(false);
    assert_eq!(
        OutboxStep::Empty,
        outbox_step(
            &flock,
            &mut pane,
            P,
            101.0,
            &stopped,
            &|_| false,
            &mut || None
        )
    );
    assert_eq!(
        DeliveryOutcome::AlreadyDecided,
        flock.deliver_message("m-doomed", 102.0, &mut pane, || None)
    );
    assert_eq!(1, pane.typed.len(), "a failed send is never retried");
}

/// Python `test_a_refused_message_keeps_its_place_and_is_delivered_later`: a refused head stays
/// queued and is not typed over, and the message behind it keeps its place. Both go out later.
#[test]
fn test_a_refused_message_keeps_its_place_and_is_delivered_later() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
    let mut refused = Message::new("m-hello", P, "pane", "hello", "test");
    refused.press_enter = false;
    let mut behind = Message::new("m-after", P, "pane", "after", "test");
    behind.press_enter = false;
    flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![refused, behind]);
    let mut pane = Pane {
        receipt_ok: true,
        ..Pane::default()
    };
    pane.statuses.insert(P.into(), "idle".into());
    pane.panes = vec![serde_json::json!({
        "pane_id": P,
        "pane_pid": std::process::id(),
    })];
    pane.screens
        .insert(P.into(), submit_pending_screen("human draft"));
    let stopped = AtomicBool::new(false);
    let step = |pane: &mut Pane, now: f64| {
        outbox_step(&flock, pane, P, now, &stopped, &|_| false, &mut || None)
    };

    assert_eq!(OutboxStep::Held, step(&mut pane, 100.0));
    assert!(pane.typed.is_empty(), "the refusal types nothing");
    let queue = flock.outbox_snapshot();
    assert_eq!(
        vec![("m-hello", "queued", 1), ("m-after", "queued", 0)],
        queue[P]
            .iter()
            .map(|message| (message.id.as_str(), message.status.as_str(), message.holds))
            .collect::<Vec<_>>()
    );
    // The same draft, still there: the held head waits for its backoff and is not retried.
    assert_eq!(OutboxStep::Due, step(&mut pane, 100.0));
    assert!(pane.typed.is_empty());

    // The draft is gone. Both messages go out, in the order they were queued.
    pane.screens.insert(P.into(), hint_screen());
    assert_eq!(OutboxStep::Delivered, step(&mut pane, 106.0));
    assert_eq!(OutboxStep::Delivered, step(&mut pane, 106.0));
    assert_eq!(OutboxStep::Empty, step(&mut pane, 106.0));
    let typed: Vec<String> = pane.typed.iter().map(|(_, text)| text.clone()).collect();
    assert_eq!(vec!["hello", "after"], typed);
    let sent: Vec<String> = flock
        .message_history
        .lock()
        .unwrap()
        .iter()
        .map(|message| message.id.clone())
        .collect();
    assert_eq!(vec!["m-hello", "m-after"], sent);
}

// T55 rows 16 and 65 (t3344 proof C), test-only. Python oracle: tests/test_dunk_stuck_recovery.py
// at 79bc92fa. Row 16 is ReviewBlockerTests.test_a_dunk_notice_with_no_message_row_is_still_tracked_and_recovered
// and row 65 is AttemptGenerationTests.test_the_attempt_identity_is_captured_before_delivery_not_after.
// No production code is changed by these tests.
mod t55_rows_16_65 {
    use super::*;
    use crate::flock::panes::Stores;
    use crate::metrics::MetricsStore;
    use std::sync::Arc;

    const PANE: &str = "%1";

    #[test]
    fn a_dunk_notice_with_no_message_row_is_tracked_and_its_consumed_recovery_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        let flock = persistent_recovery_flock(dir.path(), &state);
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        flock.attach_stores(Stores {
            metrics: Some(metrics.clone()),
            ..Stores::default()
        });
        {
            let mut config = flock.stuck_config.lock().unwrap();
            config.threshold_s = 1.0;
            config.observation_interval_s = 0.1;
            config.max_attempts = 1;
        }
        let mut env = Pane::default();
        env.panes
            .push(serde_json::json!({"pane_id": PANE, "pane_pid": std::process::id()}));
        env.statuses.insert(PANE.into(), "idle".into());
        env.screens.insert(PANE.into(), frame("stuck body", &[]));
        env.after_key_screen = Some(frame("\x1b[2mAsk Codex to do anything\x1b[0m", &[]));
        flock
            .sent_text_by_pane
            .lock()
            .unwrap()
            .insert(PANE.into(), vec!["stuck body".into()]);
        // The dunk notice was sent, and its Message row has left the outbox.
        let mut notice = Message::new("d1-notice", PANE, "dunk", "stuck body", "d1");
        notice.created_at = 1.0;
        notice.status = "failed".into();
        notice.attempt_generation = pane_generation(&env, PANE);
        flock.note_stuck_candidate(&notice);

        // No Message row, so the head is None and the identity is the text fingerprint.
        assert!(!flock.unwedge(
            &mut env,
            PANE,
            "stuck body",
            Some(99.5),
            None,
            100.0,
            None,
            false
        ));
        assert!(
            env.keys.is_empty(),
            "the clock has not reached the threshold"
        );
        assert!(
            flock
                .stuck_tracker
                .lock()
                .unwrap()
                .records
                .contains_key(PANE),
            "a dunk notice with no message row is still tracked"
        );
        assert!(flock.unwedge(
            &mut env,
            PANE,
            "stuck body",
            Some(98.0),
            None,
            200.0,
            None,
            false
        ));
        assert_eq!(1, env.keys.len());
        let outcomes: Vec<String> = metrics
            .events_between(0.0, f64::MAX)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "stuck.recover")
            .filter_map(|event| event.outcome)
            .collect();
        let consumed = outcomes
            .iter()
            .filter(|outcome| *outcome == "consumed")
            .count();
        assert_eq!(
            1, consumed,
            "one consumed stuck.recover event, got {outcomes:?}"
        );
    }

    #[test]
    fn the_attempt_identity_and_the_registry_generation_are_captured_before_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(persistent_receipt_flock(dir.path()));
        // These flags are the ones the Sent path is pinned by, so the delivery reaches the
        // typed-attempt receipt (see fold_during_success_and_failure_settles_the_exact_receipt_frame).
        let mut message = Message::new("m-attempt", PANE, "pane", "hello", "test");
        message.hold_while_typing = false;
        message.press_enter = false;
        flock
            .outbox
            .lock()
            .unwrap()
            .insert(PANE.into(), vec![message]);
        let mut pane = Pane {
            receipt_forward: Some(real_receipt_forward(
                Arc::clone(&flock),
                serde_json::json!({"pane_id": PANE}),
            )),
            ..Pane::default()
        };
        // The pane holds the pre-delivery generation. The terminal action (send_text) replaces the
        // pane, so every read after it returns the new generation.
        *pane.generation.borrow_mut() = Some("pid:1:start:before".to_string());
        *pane.generation_after_send.borrow_mut() = Some("pid:2:start:after".to_string());
        let before = "pid:1:start:before".to_string();
        let after = "pid:2:start:after".to_string();
        assert_eq!(
            Some(before.clone()),
            pane.attempt_generation(PANE),
            "premise: the pane is read as the pre-delivery generation"
        );
        assert_eq!(
            DeliveryOutcome::Sent,
            flock.deliver_message("m-attempt", 100.0, &mut pane, || None),
            "the delivery reaches the typed-attempt receipt and is sent"
        );
        assert_eq!(
            Some(after.clone()),
            pane.attempt_generation(PANE),
            "premise: the terminal action replaced the pane"
        );
        assert_ne!(
            before, after,
            "premise: the generation changed during delivery"
        );
        let delivered = flock
            .message_history
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == "m-attempt")
            .map(|m| m.attempt_generation.clone());
        let held = flock.outbox_snapshot()[PANE]
            .iter()
            .find(|m| m.id == "m-attempt")
            .map(|m| m.attempt_generation.clone());
        assert_eq!(
            Some(Some(before.clone())),
            delivered.or(held),
            "the attempt identity must be the pane the text went into, not the one after the action"
        );
        let registry = flock
            .stuck_candidates
            .lock()
            .unwrap()
            .get(PANE)
            .and_then(|candidate| candidate.generation.clone());
        assert_eq!(
            Some(before),
            registry,
            "the registry must carry the ATTEMPT generation, not the pane's generation after delivery"
        );
    }
}
