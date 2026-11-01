//! Tests for `sends.rs`, ported by name from tests/test_busy_native_queue.py,
//! tests/test_codex_ambiguous_composer.py (the `_box_holds` / `send_blocker` halves) and
//! tests/test_dunk_core.py (the `_clear_to_send` and `_press_until_submitted` tests). The scripted
//! pane below is the `FakeHerdr` surface those tests drive. NOT ported (later slices):
//! `submit_text`/`send_guarded`/`delivery_state`, and the thread-timing assertions of the
//! clear-to-send tests (the settle wait is injected, so wall-clock is not asserted).

use super::sends::*;
use super::Flock;
use crate::flock::panes::PaneInfoEnv;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
struct Pane {
    screens: BTreeMap<String, String>,
    harness: BTreeMap<String, String>,
    statuses: BTreeMap<String, String>,
    gone: bool,
    federated: bool,
    keys: Vec<(String, String)>,
    reads: RefCell<Vec<(String, bool)>>,
    screen_script: RefCell<VecDeque<Option<String>>>,
    status_script: RefCell<VecDeque<Option<String>>>,
    now: f64,
    sleeps: Vec<f64>,
    remembered: BTreeMap<String, String>,
    submit_metrics: Vec<(String, String, usize, usize)>,
    panes: Vec<serde_json::Value>,
    hint: String,
}

impl SendEnv for Pane {
    fn read_pane(&self, pane_id: &str, _lines: i64, ansi: bool) -> Option<String> {
        self.reads.borrow_mut().push((pane_id.to_string(), ansi));
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
        self.keys.push((pane_id.to_string(), key.to_string()));
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

const P: &str = "w2:p1";

#[test]
fn enqueue_appends_starts_worker_then_saves_the_queued_message() {
    use std::sync::{Arc, Mutex, TryLockError};

    #[derive(Default)]
    struct Events {
        order: Mutex<Vec<&'static str>>,
        saved: Mutex<Vec<BTreeMap<String, Vec<Message>>>>,
    }

    impl OutboxWake for Events {
        fn ensure_worker(
            &self,
            flock: &Flock,
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
    let flock = Flock::new(dir.path().join("launches.json"), dir.path(), false);
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
    use std::sync::{Arc, TryLockError};

    struct Wake;
    impl OutboxWake for Wake {
        fn ensure_worker(&self, flock: &Flock, _: &str) -> Result<(), crate::error::DunkError> {
            assert!(matches!(
                flock.outbox.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            Ok(())
        }
    }

    struct Store(AtomicUsize);
    impl SnapshotStore for Store {
        fn save(&self, flock: &Flock) -> Result<(), crate::error::DunkError> {
            assert!(flock.outbox.try_lock().is_ok());
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
    let wake = Arc::new(Wake);
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
        let mut pane = Pane::default();
        pane.panes = vec![serde_json::json!({
            "pane_id": P,
            "tab_label": "Sheep",
            "label": "Sheep shell",
            "agent": "claude"
        })];
        pane.hint = "no backend panes".into();
        pane
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
        assert!(result["detail"].as_str().unwrap().contains("composer"));
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
