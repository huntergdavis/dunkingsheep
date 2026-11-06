//! T36c slice 2 tests: the dialog observer step on an explicit clock, and the loop shell. The
//! Python has no dedicated test file for `_dialog_observer_step`; these pin the behaviour the
//! source states (30 s scan period, stable `since`, fresh interval on a replaced/non-modal pane,
//! nothing typed) and the loop's tick order.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::ready;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::commands_inbox::{InboxEnv, Nudge, RouteEntry};
use super::loops_stuck::{
    federated_recovery_step, run_stuck_observer, strict_stuck_observer_scan, DialogObserver,
    StalePanelReporter, StuckEnv,
};
use super::mailbox::MailboxEnv;
use super::Flock;
use crate::db::ReadPool;
use crate::error::DunkError;
use crate::flock::loop_env::LoopFuture;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::{Message, SendEnv};
use crate::inbox_store::Store;

const MODAL: &str = "\u{1b}[1mApprove this?\u{1b}[0m\nEsc to cancel   Enter to confirm\n";

struct RecoveryProbe {
    screen: Mutex<String>,
    after_enter: String,
    keys: Mutex<Vec<String>>,
    generation: String,
    federated: bool,
}

impl SendEnv for RecoveryProbe {
    fn read_pane(&self, _: &str, _: i64, _: bool) -> Option<String> {
        Some(self.screen.lock().unwrap().clone())
    }
    fn pane_harness(&self, _: &str) -> Option<String> {
        Some("codex".into())
    }
    fn session_process_gone(&self, _: &str) -> bool {
        false
    }
    fn read_status(&self, _: &str) -> Option<String> {
        Some("idle".into())
    }
    fn send_key(&mut self, _: &str, key: &str) -> (bool, String) {
        self.keys.lock().unwrap().push(key.into());
        if key == "Enter" {
            *self.screen.lock().unwrap() = self.after_enter.clone();
        }
        (true, "sent".into())
    }
    fn federated_input(&self) -> bool {
        self.federated
    }
}

impl PaneInfoEnv for RecoveryProbe {
    fn list_panes_grouped(&self) -> Vec<Value> {
        vec![self.get_pane("%1").unwrap()]
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        (pane_id == "%1").then(|| {
            json!({
                "pane_id": "%1", "pane_pid": std::process::id(), "agent": "codex"
            })
        })
    }
}

fn persisted_flock(dir: &tempfile::TempDir) -> Flock {
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        true,
    );
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            dir.path().join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    flock
}

fn federated_probe() -> RecoveryProbe {
    let frame = "  >_ OpenAI Codex (v0.165.0)\n\n\x1b[1m\u{203a} pending text\x1b[0m\n\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  ? for shortcuts\n";
    RecoveryProbe {
        screen: Mutex::new(frame.into()),
        after_enter: frame.replace("pending text", "Ask Codex to do anything"),
        keys: Mutex::default(),
        generation: format!(
            "pid:{}:start:{}",
            std::process::id(),
            crate::proc::process_start(i64::from(std::process::id())).unwrap()
        ),
        federated: true,
    }
}

fn failed_federated_candidate(probe: &RecoveryProbe) -> Message {
    let mut message = Message::new("m1", "%1", "lane", "pending text", "send_text");
    message.status = "failed".into();
    message.attempt_generation = Some(probe.generation.clone());
    message
}

struct Fake {
    panes: Mutex<Result<Vec<Value>, String>>,
    screens: Mutex<BTreeMap<String, String>>,
    generations: Mutex<BTreeMap<String, String>>,
    notes: Mutex<Vec<(String, String, f64)>>,
    log: Mutex<Vec<&'static str>>,
    observed: Mutex<Vec<&'static str>>,
    enabled: bool,
}

impl Fake {
    fn with_panes(panes: Value) -> Fake {
        Fake {
            panes: Mutex::new(Ok(panes.as_array().unwrap().clone())),
            screens: Mutex::default(),
            generations: Mutex::default(),
            notes: Mutex::default(),
            log: Mutex::default(),
            observed: Mutex::default(),
            enabled: false,
        }
    }
    fn screen(&self, pane: &str, text: &str) {
        self.screens
            .lock()
            .unwrap()
            .insert(pane.into(), text.into());
    }
}

impl StuckEnv for Fake {
    fn list_panes(&self) -> LoopFuture<'_, Result<Vec<Value>, DunkError>> {
        self.observed.lock().unwrap().push("list");
        let result = self
            .panes
            .lock()
            .unwrap()
            .clone()
            .map_err(DunkError::Refused);
        Box::pin(ready(result))
    }
    fn read_screen<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>> {
        self.observed.lock().unwrap().push("screen");
        let screen = self.screens.lock().unwrap().get(pane_id).cloned();
        Box::pin(ready(screen))
    }
    fn pane_generation<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>> {
        self.observed.lock().unwrap().push("generation");
        let generation = self.generations.lock().unwrap().get(pane_id).cloned();
        Box::pin(ready(generation))
    }
    fn note_stale_panel<'a>(
        &'a self,
        pane_id: &'a str,
        text: &'a str,
        since: f64,
    ) -> LoopFuture<'a, ()> {
        self.observed.lock().unwrap().push("note");
        let pane_id = pane_id.to_owned();
        let text = text.to_owned();
        Box::pin(async move {
            self.notes.lock().unwrap().push((pane_id, text, since));
        })
    }
    fn stuck_recovery_enabled(&self) -> bool {
        self.enabled
    }
    fn stuck_observer_step(&self) -> LoopFuture<'_, usize> {
        Box::pin(async move {
            self.log.lock().unwrap().push("step");
            1
        })
    }
}

fn agent(id: &str) -> Value {
    json!({"pane_id": id, "agent": "claude"})
}

struct AlertInbox {
    pool: ReadPool,
    store: Arc<Store>,
    _dir: tempfile::TempDir,
    nudges: Mutex<Vec<String>>,
    panes: Vec<Value>,
}

impl AlertInbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let pool = ReadPool::open(store.path(), 1).unwrap();
        Self {
            pool,
            store,
            _dir: dir,
            nudges: Mutex::new(Vec::new()),
            panes: vec![json!({
                "pane_id":"w9:pH", "terminal_id":"term_vp", "label":"VP",
                "tab_label":"VP", "workspace_label":"fixture", "agent":"codex"
            })],
        }
    }
}

impl MailboxEnv for AlertInbox {
    fn list_panes(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl InboxEnv for AlertInbox {
    fn caller_role(&self, _: &str) -> Result<String, DunkError> {
        Ok("owner".into())
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("fixture".into())
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
    fn pm_routing(&self) -> Vec<RouteEntry> {
        Vec::new()
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn send_now(&self, _: &str, text: &str, _: &str) -> Result<Nudge, DunkError> {
        self.nudges.lock().unwrap().push(text.to_string());
        Ok(Nudge {
            queued: false,
            message: Some("fixture submitted".into()),
        })
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, _: &str) -> Result<Value, DunkError> {
        Ok(json!([]))
    }
}

fn alert_flock(dir: &tempfile::TempDir) -> Flock {
    let flock = Flock::new(
        dir.path().join("state.json"),
        dir.path().join("lanes.json"),
        false,
    );
    flock.wire_observability(dir.path(), None).unwrap();
    flock
}

#[test]
fn cloned_reporters_share_warning_and_retry_only_failed_sinks() {
    let inbox = AlertInbox::new();
    let state = tempfile::tempdir().unwrap();
    let alert_db = state.path().join("alerts.db");
    let flock = alert_flock(&state);
    // Model the observer, an outbox worker, and that worker's restart.
    let mut loop_reporter = StalePanelReporter::new();
    let mut outbox_reporter = loop_reporter.clone();
    let mut restarted_outbox_reporter = outbox_reporter.clone();
    let owner_calls = Cell::new(0);
    let vp_calls = Cell::new(0);
    let mut owner = |_: &str| {
        let attempt = owner_calls.get() + 1;
        owner_calls.set(attempt);
        if attempt == 1 {
            Err("offline".into())
        } else {
            Ok(crate::dialog_alerts::SinkDelivery::Delivered)
        }
    };
    let mut vp = |body: &str| {
        vp_calls.set(vp_calls.get() + 1);
        crate::dialog_alerts::post_vp(&flock, &inbox, body, Some("w9:pH"))
    };

    loop_reporter.note_with_sinks(
        &flock,
        "%2",
        "modal",
        Some("pid:1:start:2"),
        Some("Allow command?"),
        0.0,
        29.9,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    loop_reporter.note_with_sinks(
        &flock,
        "%2",
        "box",
        Some("pid:1:start:2"),
        Some("Allow command?"),
        0.0,
        30.0,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    loop_reporter.note_with_sinks(
        &flock,
        "%3",
        "modal",
        None,
        Some("Allow command?"),
        0.0,
        30.0,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    loop_reporter.note_with_sinks(
        &flock,
        "%4",
        "modal",
        Some("pid:1:start:2"),
        None,
        0.0,
        30.0,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    assert_eq!(owner_calls.get(), 0);
    assert_eq!(vp_calls.get(), 0);

    loop_reporter.note_with_sinks(
        &flock,
        "%2",
        "modal",
        Some("pid:1:start:2"),
        Some("Allow command?"),
        0.0,
        30.0,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    assert_eq!(owner_calls.get(), 1);
    assert_eq!(vp_calls.get(), 1);
    let (mailbox, rows) = flock
        .inbox_list(&inbox, Some("VP"), true, 10, None)
        .unwrap();
    assert_eq!(mailbox, "VP");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sender.as_deref(), Some("watchdog"));
    assert_eq!(
        rows[0].subject.as_deref(),
        Some("Dialog blocks queued delivery")
    );
    assert_eq!(inbox.nudges.lock().unwrap().len(), 1);

    outbox_reporter.note_with_sinks(
        &flock,
        "%2",
        "modal",
        Some("pid:1:start:2"),
        Some("Allow command?"),
        0.0,
        89.9,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    assert_eq!(
        owner_calls.get(),
        1,
        "failed owner sink waits for the 60s retry"
    );
    assert_eq!(vp_calls.get(), 1, "successful VP sink is not repeated");
    restarted_outbox_reporter.note_with_sinks(
        &flock,
        "%2",
        "modal",
        Some("pid:1:start:2"),
        Some("Allow command?"),
        0.0,
        90.0,
        Some(&alert_db),
        &mut owner,
        &mut vp,
    );
    assert_eq!(owner_calls.get(), 2, "failed owner sink retries at 60s");
    assert_eq!(vp_calls.get(), 1, "successful VP sink remains deduplicated");

    let metrics = flock.stores().metrics.unwrap();
    let events = metrics.events_between(0.0, 200.0).unwrap();
    let pane_events: Vec<_> = events
        .iter()
        .filter(|event| event.kind == "panel.blocking" && event.pane_id.as_deref() == Some("%2"))
        .collect();
    assert_eq!(pane_events.len(), 1);
    assert_eq!(pane_events[0].outcome.as_deref(), Some("held"));
    assert_eq!(pane_events[0].reason.as_deref(), Some("dialog"));
}

#[tokio::test]
async fn loop_stuck_observer_awaits_worker_local_screen_reads() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    assert_eq!(observer.step(&fake, 100.0).await, 1);
    assert_eq!(
        *fake.observed.lock().unwrap(),
        vec!["list", "screen", "generation", "note"]
    );
}

#[tokio::test]
async fn a_modal_pane_is_reported_with_its_collapsed_screen_and_since_now() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    assert_eq!(observer.step(&fake, 100.0).await, 1);
    let notes = fake.notes.lock().unwrap();
    assert_eq!(
        notes[0],
        (
            "%1".to_string(),
            "Approve this? Esc to cancel Enter to confirm".to_string(),
            100.0
        )
    );
}

#[tokio::test]
async fn the_scan_runs_at_most_every_thirty_seconds() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    observer.step(&fake, 129.9).await;
    assert_eq!(
        fake.notes.lock().unwrap().len(),
        1,
        "inside the period: no scan"
    );
    observer.step(&fake, 130.0).await;
    assert_eq!(fake.notes.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn since_is_kept_while_the_generation_and_screen_are_unchanged() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g1".into());
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    observer.step(&fake, 140.0).await;
    assert_eq!(
        fake.notes.lock().unwrap()[1].2,
        100.0,
        "stable: the interval keeps running"
    );
}

#[tokio::test]
async fn a_replaced_pane_or_a_changed_dialog_starts_a_fresh_interval() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g1".into());
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g2".into());
    observer.step(&fake, 140.0).await;
    assert_eq!(
        fake.notes.lock().unwrap()[1].2,
        140.0,
        "new generation: since resets"
    );
    fake.screen(
        "%1",
        "Approve something else?\nEsc to cancel   Enter to confirm\n",
    );
    observer.step(&fake, 180.0).await;
    assert_eq!(
        fake.notes.lock().unwrap()[2].2,
        180.0,
        "new dialog text: since resets"
    );
}

#[tokio::test]
async fn a_pane_that_stops_showing_a_modal_forgets_its_interval() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    fake.screen("%1", "plain output\n");
    assert_eq!(observer.step(&fake, 140.0).await, 0);
    fake.screen("%1", MODAL);
    observer.step(&fake, 180.0).await;
    assert_eq!(
        fake.notes.lock().unwrap().last().unwrap().2,
        180.0,
        "a fresh stability interval"
    );
}

#[tokio::test]
async fn panes_without_an_agent_or_a_readable_screen_are_skipped() {
    let fake = Fake::with_panes(
        json!([{"pane_id": "%1"}, {"pane_id": "%2", "agent": ""}, agent("%3"), {"agent": "x"}]),
    );
    fake.screen("%1", MODAL);
    fake.screen("%2", MODAL);
    // %3 has an agent but no readable screen.
    let mut observer = DialogObserver::new();
    assert_eq!(observer.step(&fake, 100.0).await, 0);
    assert!(fake.notes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_pane_listing_keeps_the_observations_and_still_spends_the_scan() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    *fake.panes.lock().unwrap() = Err("backend down".into());
    assert_eq!(observer.step(&fake, 140.0).await, 0);
    *fake.panes.lock().unwrap() = Ok(vec![agent("%1")]);
    observer.step(&fake, 145.0).await;
    assert_eq!(
        fake.notes.lock().unwrap().len(),
        1,
        "the failed scan consumed the 30 s window"
    );
    observer.step(&fake, 170.0).await;
    assert_eq!(
        fake.notes.lock().unwrap().last().unwrap().2,
        100.0,
        "observations survived the failure"
    );
}

#[tokio::test]
async fn the_signature_is_the_last_4000_characters() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    let long = format!("{}\nEsc to cancel   Enter to confirm\n", "é".repeat(5000));
    fake.screen("%1", &long);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0).await;
    let text = fake.notes.lock().unwrap()[0].1.clone();
    assert_eq!(text.chars().count(), 4000);
    assert!(text.ends_with("Esc to cancel Enter to confirm"));
}

#[tokio::test]
async fn the_loop_invokes_non_noop_stuck_step_when_enabled() {
    for (enabled, want_steps) in [(false, false), (true, true)] {
        let mut fake = Fake::with_panes(json!([agent("%1")]));
        fake.enabled = enabled;
        fake.screen("%1", MODAL);
        let fake = Arc::new(fake);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_stuck_observer(
            fake.clone(),
            Duration::from_millis(10),
            stop_rx,
        ));
        tokio::time::sleep(Duration::from_millis(120)).await;
        stop_tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("stops")
            .unwrap();
        assert!(
            !fake.notes.lock().unwrap().is_empty(),
            "the dialog scan runs regardless of recovery"
        );
        assert_eq!(
            fake.notes.lock().unwrap().len(),
            1,
            "30 s period: one scan inside this test"
        );
        assert_eq!(
            !fake.log.lock().unwrap().is_empty(),
            want_steps,
            "enabled={enabled}"
        );
    }
}

#[test]
fn federated_recovery_waits_then_submits_once_without_replaying_text() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&failed_federated_candidate(&probe));
    let waits = Mutex::new(Vec::new());
    let wait = |seconds| waits.lock().unwrap().push(seconds);

    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &wait).unwrap()
    );
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "threshold has not elapsed"
    );
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, 221.0, &wait).unwrap()
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
    assert_eq!(
        vec![crate::flock::sends::SUBMIT_POLL_S[0]],
        *waits.lock().unwrap()
    );
    assert_eq!(
        Some(String::new()),
        crate::terminal::composer::federated_composer(true, &probe.screen.lock().unwrap())
    );
}

#[test]
fn strict_stuck_scan_leaves_queued_pane_to_its_outbox_worker() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    let candidate = failed_federated_candidate(&probe);
    flock.note_stuck_candidate(&candidate);
    let mut queued = Message::new("m2", "%1", "lane", "next", "send_text");
    queued.status = "queued".into();
    flock
        .outbox
        .lock()
        .unwrap()
        .insert("%1".into(), vec![queued]);

    assert_eq!(
        0,
        strict_stuck_observer_scan(&flock, &mut probe, 221.0).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());
}

#[test]
fn strict_stuck_scan_repairs_consumed_receipt_even_when_pane_is_queued() {
    use crate::flock::panes::Stores;
    use crate::inbox_store::Outgoing;
    use rusqlite::Connection;

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
    let flock = persisted_flock(&dir);
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });

    let mut consumed = Message::new("m1", "%1", "lane", "earlier", "send_text");
    consumed.status = "sent".into();
    consumed.attempt_generation = Some("old-generation".into());
    consumed.notify_for.push(inbox_id.clone());
    flock.note_stuck_candidate(&consumed);
    {
        let mut candidates = flock.stuck_candidates.lock().unwrap();
        let candidate = candidates.get_mut("%1").unwrap();
        candidate.status = "consumed".into();
        candidate.eligible = false;
        candidate.recovery_outcome = Some("consumed".into());
        candidate.recovery_detail = Some("composer cleared".into());
        candidate.receipts_reconciled = Some(false);
    }

    let mut queued = Message::new("m2", "%1", "lane", "later", "send_text");
    queued.status = "queued".into();
    flock
        .outbox
        .lock()
        .unwrap()
        .insert("%1".into(), vec![queued]);

    let mut probe = federated_probe();
    probe.federated = false;
    assert_eq!(
        0,
        strict_stuck_observer_scan(&flock, &mut probe, 221.0).unwrap()
    );

    let (notify_state, notify_reason): (Option<String>, Option<String>) = observer
        .query_row(
            "SELECT notify_state, notify_reason FROM messages WHERE id = ?1",
            [&inbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(Some("sent".into()), notify_state);
    assert_eq!(Some("recovered: composer cleared".into()), notify_reason);
    assert_eq!(
        Some(true),
        flock.stuck_candidates.lock().unwrap()["%1"].receipts_reconciled
    );
    assert_eq!("queued", flock.outbox.lock().unwrap()["%1"][0].status);
    assert!(probe.keys.lock().unwrap().is_empty());
}

#[test]
fn strict_stuck_scan_records_consumed_at_when_composer_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    probe.federated = false;
    let empty_frame = format!(
        "  >_ OpenAI Codex (v0.165.0)\n     ~/workspace/fixture\n  permissions: YOLO mode\n{}\u{203a} Ask Codex to do anything\n\n  GPT-6-Astra high \u{b7} ~/workspace/fixture\n  ? for shortcuts                                      \u{26a0} 1 warning \u{b7} f2 to view",
        "\n".repeat(16)
    );
    assert_eq!(
        Some(String::new()),
        crate::terminal::screen::typed_text(&empty_frame)
    );
    *probe.screen.lock().unwrap() = empty_frame;

    let mut candidate = failed_federated_candidate(&probe);
    candidate.created_at = 10.0;
    flock.note_stuck_candidate(&candidate);

    assert_eq!(
        0,
        strict_stuck_observer_scan(&flock, &mut probe, 221.0).unwrap()
    );

    let candidates = flock.stuck_candidates.lock().unwrap();
    let consumed = &candidates["%1"];
    assert_eq!("consumed", consumed.status);
    assert!(!consumed.eligible);
    assert_eq!(Some(221.0), consumed.consumed_at);
    assert_eq!(None, consumed.retired_at);
    drop(candidates);

    let snapshot = flock.snapshot();
    assert_eq!(
        221.0,
        snapshot["stuck_candidates"]["%1"]["consumed_at"]
            .as_f64()
            .unwrap()
    );
    assert!(snapshot["stuck_candidates"]["%1"]["retired_at"].is_null());
}

#[test]
fn federated_recovery_refuses_no_enter_intent_and_queued_panes() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut probe = federated_probe();
    let mut no_enter = failed_federated_candidate(&probe);
    no_enter.press_enter = false;
    flock.note_stuck_candidate(&no_enter);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&failed_federated_candidate(&probe));
    let mut queued = Message::new("m2", "%1", "lane", "next", "send_text");
    queued.status = "queued".into();
    flock
        .outbox
        .lock()
        .unwrap()
        .insert("%1".into(), vec![queued]);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());
}

/// A failed-or-otherwise candidate for `%1` with the probe's attempt generation.
fn observed_candidate(
    probe: &RecoveryProbe,
    id: &str,
    status: &str,
    press_enter: bool,
    created_at: f64,
) -> Message {
    let mut message = failed_federated_candidate(probe);
    message.id = id.into();
    message.status = status.into();
    message.press_enter = press_enter;
    message.created_at = created_at;
    message
}

/// Runs the federated observer at each time and asserts that none of them acts.
fn federated_steps_without_acting(flock: &Flock, probe: &mut RecoveryProbe, times: &[f64]) {
    for now in times {
        assert_eq!(
            0,
            federated_recovery_step(flock, probe, *now, &|_| {}).unwrap(),
            "no action at t={now}"
        );
    }
}

// StandaloneObserverTests.test_exactly_one_action_at_the_threshold_and_no_second_message (row 38)
#[test]
fn federated_recovery_at_the_threshold_acts_once_and_creates_no_second_message() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&failed_federated_candidate(&probe));
    federated_steps_without_acting(&flock, &mut probe, &[100.0]);
    let messages_before = flock.message_history.lock().unwrap().len();
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
    assert_eq!(
        messages_before,
        flock.message_history.lock().unwrap().len(),
        "recovery must not create or send a second message"
    );
}

// ObserverBoundaryTests.test_a_newer_sent_delivery_also_supersedes (row 46)
#[test]
fn a_newer_sent_delivery_with_enter_supersedes_an_older_failed_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    flock.note_stuck_candidate(&observed_candidate(&probe, "m2", "sent", true, 11.0));
    federated_steps_without_acting(&flock, &mut probe, &[100.0, 221.0, 821.0]);
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "the pane's latest intent is a clean send"
    );
}

// IntentOrderingTests.test_a_late_older_failure_cannot_undo_a_newer_clean_send (row 54)
#[test]
fn a_late_older_failure_cannot_undo_a_newer_clean_send() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&observed_candidate(&probe, "m2", "sent", true, 20.0));
    flock.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    federated_steps_without_acting(&flock, &mut probe, &[100.0, 221.0]);
    assert!(probe.keys.lock().unwrap().is_empty());
}

// IntentOrderingTests.test_a_late_older_failure_cannot_undo_a_newer_cancellation (row 55)
#[test]
fn a_late_older_failure_cannot_undo_a_newer_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&observed_candidate(&probe, "m2", "cancelled", true, 20.0));
    flock.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    federated_steps_without_acting(&flock, &mut probe, &[100.0, 221.0]);
    assert!(probe.keys.lock().unwrap().is_empty());
}

// SameTimestampOrderTests.test_same_timestamp_uses_message_sequence_not_lexical_order (row 59)
#[test]
fn same_timestamp_uses_message_sequence_not_lexical_order() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    // m10 is the newer message by sequence; the older-by-string m9 FAILED arrives later.
    flock.note_stuck_candidate(&observed_candidate(&probe, "m10", "sent", false, 50.0));
    flock.note_stuck_candidate(&observed_candidate(&probe, "m9", "failed", true, 50.0));
    federated_steps_without_acting(&flock, &mut probe, &[100.0, 221.0, 821.0]);
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "'m10' must outrank 'm9' by sequence, not by string order"
    );
}

// SameTimestampOrderTests.test_consumption_keeps_the_watermark (row 60)
#[test]
fn consumption_keeps_the_watermark_against_a_late_older_failure() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    probe.federated = false;
    let empty_frame = format!(
        "  >_ OpenAI Codex (v0.165.0)\n     ~/workspace/fixture\n  permissions: YOLO mode\n{}\u{203a} Ask Codex to do anything\n\n  GPT-6-Astra high \u{b7} ~/workspace/fixture\n  ? for shortcuts                                      \u{26a0} 1 warning \u{b7} f2 to view",
        "\n".repeat(16)
    );
    *probe.screen.lock().unwrap() = empty_frame;
    flock.note_stuck_candidate(&observed_candidate(&probe, "m2", "failed", true, 10.0));
    assert_eq!(
        0,
        strict_stuck_observer_scan(&flock, &mut probe, 221.0).unwrap()
    );
    assert_eq!(
        "consumed",
        flock.stuck_candidates.lock().unwrap()["%1"].status
    );
    // A late, older FAILED decision must not resurrect the work.
    flock.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 9.0));
    *probe.screen.lock().unwrap() = federated_probe().screen.lock().unwrap().clone();
    for now in [231.0, 341.0, 701.0] {
        assert_eq!(
            0,
            strict_stuck_observer_scan(&flock, &mut probe, now).unwrap(),
            "no action at t={now}"
        );
    }
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "the watermark must survive consumption"
    );
    let candidates = flock.stuck_candidates.lock().unwrap();
    assert_eq!("m2", candidates["%1"].message_id);
    assert_eq!("consumed", candidates["%1"].status);
}

/// A flock with persistence off: a restart is modelled by restoring a snapshot.
fn in_memory_flock(dir: &tempfile::TempDir) -> Flock {
    Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    )
}

// ObserverBoundaryTests.test_the_candidate_survives_a_restart (row 49)
#[test]
fn the_candidate_survives_a_restart_with_its_enter_and_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    let after = in_memory_flock(&dir);
    after.restore_snapshot(&before.snapshot());
    let candidates = after.stuck_candidates.lock().unwrap();
    assert_eq!("m1", candidates["%1"].message_id);
    assert!(
        candidates["%1"].press_enter,
        "the restored candidate keeps its Enter intent"
    );
}

// RestartRecoveryTests.test_the_restarted_worker_actually_recovers (row 51)
#[test]
fn the_restarted_worker_recovers_the_persisted_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    let after = in_memory_flock(&dir);
    after.restore_snapshot(&before.snapshot());
    federated_steps_without_acting(&after, &mut probe, &[100.0]);
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "the first step after a restart is only evidence"
    );
    assert_eq!(
        1,
        federated_recovery_step(&after, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
}

// GenerationBindingTests.test_the_same_generation_still_recovers_with_the_real_matcher (row 62)
#[test]
fn the_same_generation_after_a_restart_still_presses_once() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    let after = in_memory_flock(&dir);
    after.restore_snapshot(&before.snapshot());
    assert_eq!(
        Some(probe.generation.clone()),
        after.stuck_candidates.lock().unwrap()["%1"].generation,
        "the persisted attempt generation survives the restart"
    );
    federated_steps_without_acting(&after, &mut probe, &[100.0]);
    assert_eq!(
        1,
        federated_recovery_step(&after, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert_eq!(1, probe.keys.lock().unwrap().len());
}

// GenerationBindingTests.test_a_replacement_generation_is_retired_not_adopted (row 63)
#[test]
fn a_replacement_generation_after_a_restart_is_retired_not_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    probe.federated = false;
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    let after = in_memory_flock(&dir);
    after.restore_snapshot(&before.snapshot());
    // The pane is now a different live process at the same pane id.
    probe.generation = "pid:1:start:replaced".into();
    for now in [100.0, 221.0, 441.0] {
        assert_eq!(
            0,
            strict_stuck_observer_scan(&after, &mut probe, now).unwrap(),
            "no action at t={now}"
        );
    }
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "a replacement pane must never inherit the old authority"
    );
    let candidates = after.stuck_candidates.lock().unwrap();
    assert_eq!("generation-changed", candidates["%1"].status);
    assert!(!candidates["%1"].eligible);
}

// DaemonStateFileTests.test_a_declared_config_wins_over_the_saved_one (row 24)
// DIVERGENCE: Python lets the declared config win over the saved state file; Rust restore
// overwrites the declared config with the saved one (flock/mod.rs restore_snapshot).
#[test]
#[ignore = "t3354 row 24: restore overwrites a declared stuck config with the saved one; owner ruling pending"]
fn a_declared_config_wins_over_the_saved_one() {
    let dir = tempfile::tempdir().unwrap();
    let saved = in_memory_flock(&dir);
    let payload = saved.snapshot();
    let declared = in_memory_flock(&dir);
    declared.stuck_config.lock().unwrap().enabled = false;
    declared.restore_snapshot(&payload);
    assert!(
        !declared.stuck_config.lock().unwrap().enabled,
        "a declared config must not be overwritten by the state file"
    );
}
