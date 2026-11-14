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
    fn send_recovery_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.send_key(pane_id, key)
    }
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
        crate::dialog_alerts::post_lane(&flock, &inbox, body, "w9:pH", false)
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

fn scheduled_federated_fixture(flock: &Flock, probe: &RecoveryProbe, created: f64) {
    flock.dunks.load(serde_json::json!({"dunkers":[{"id":"d7","target_pane_id":"%1","text":"pending text","running":true}]}).as_object().unwrap());
    let mut message = failed_federated_candidate(probe);
    message.status = "sent".into();
    message.scheduled = true;
    message.source = "d7".into();
    message.schedule_template = Some("pending text".into());
    message.created_at = created;
    flock.note_stuck_candidate(&message);
}

#[test]
fn scheduled_federated_exact_draft_recovers_once_after_five_seconds() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    let now = crate::clock::now_s();
    scheduled_federated_fixture(&flock, &probe, now);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now, &|_| {}).unwrap()
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now + 4.0, &|_| {}).unwrap()
    );
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, now + 5.0, &|_| {}).unwrap()
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now + 10.0, &|_| {}).unwrap()
    );
    assert_eq!(1, probe.keys.lock().unwrap().len());
}

#[test]
fn scheduled_federated_rejects_edited_draft_and_changed_generation() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    let now = crate::clock::now_s();
    scheduled_federated_fixture(&flock, &probe, now);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now, &|_| {}).unwrap()
    );
    {
        let mut screen = probe.screen.lock().unwrap();
        *screen = screen.replace("pending text", "pending text human addition");
    }
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now + 6.0, &|_| {}).unwrap()
    );
    {
        let mut screen = probe.screen.lock().unwrap();
        *screen = screen.replace("pending text human addition", "pending text");
    }
    probe.generation.push_str(":changed");
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, now + 12.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());
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
    // t3501 federated row 8: the no-enter case used a non-persistent flock, so its no-key result was
    // vacuous. It now runs on a persisted flock, asserts the candidate was recorded, and has a positive
    // control (the same candidate with Enter presses once at the threshold).
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    let mut no_enter = failed_federated_candidate(&probe);
    no_enter.press_enter = false;
    flock.note_stuck_candidate(&no_enter);
    assert!(
        flock.stuck_candidates.lock().unwrap().contains_key("%1"),
        "precondition: the no-enter candidate is recorded"
    );
    federated_steps_without_acting(&flock, &mut probe, &[100.0]);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let mut probe = federated_probe();
    flock.note_stuck_candidate(&failed_federated_candidate(&probe));
    federated_steps_without_acting(&flock, &mut probe, &[100.0]);
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap(),
        "positive control: the same candidate with Enter presses once"
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());

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

// Federated analogue, accepted parity and NOT row 51 coverage. The Python row 51 case is strict
// and runs through the real matcher; its Rust counterpart is sends_tests.rs stuck_gap_tests
// (ignored until the rehydration PR lands). This test only shows the federated press after restart.
#[test]
fn the_restarted_worker_recovers_the_persisted_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    // The federated press is durable: it needs persistence on, as the daemon has.
    let after = persisted_flock(&dir);
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

// Federated analogue, accepted parity and NOT row 62 coverage. The Python row 62 case is strict
// and runs through the real matcher; its Rust counterpart is sends_tests.rs stuck_gap_tests
// (ignored until the rehydration PR lands). This test only shows the federated press after restart.
#[test]
fn the_same_generation_after_a_restart_still_presses_once() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    // The federated press is durable: it needs persistence on, as the daemon has.
    let after = persisted_flock(&dir);
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

// DaemonStateFileTests.test_a_declared_config_wins_over_the_saved_one (row 24)
// Python keeps a declared stuck config (argument or env) on restart and adopts the state file
// only when nothing was declared (dunk_core `_stuck_config_declared`). restore_snapshot now
// honours the same flag (flock/mod.rs).
#[test]
fn a_declared_config_wins_over_the_saved_one() {
    let dir = tempfile::tempdir().unwrap();
    let saved = in_memory_flock(&dir);
    let payload = saved.snapshot();
    let declared = in_memory_flock(&dir);
    declared.declare_stuck_config(crate::stuck::Config {
        enabled: false,
        ..crate::stuck::Config::default()
    });
    declared.restore_snapshot(&payload);
    assert!(
        !declared.stuck_config.lock().unwrap().enabled,
        "a declared config must not be overwritten by the state file"
    );
}

// t3371 (t820 proof D, row 44): test_long_false_sent_composer_stays_observable (oracle 79bc92fa).
// This covers the box condition only: an 18-line composer this flock sent keeps its full collapsed
// text in the box read. The Python case also asserts that the status stays `sent`, that one recovery
// key follows over ticks, and that no consumed outcome is recorded. Those are not covered here.
//
// The Python reads the box with `_box_text`, which is `_recovery_box_text`: a composer whose whole
// region is text this flock sent is read as that region (`_composer_ownership`). The module-level
// `box_text` returns only the sigil line, in Python and in Rust alike, so this drives
// `recovery_box_text`, the Rust `_box_text`.
#[test]
fn long_false_sent_composer_keeps_its_full_collapsed_text_in_the_box() {
    let text = (0..18)
        .map(|i| format!("scheduled line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let frame = format!(
        "\u{1b}[1m\u{203a}\u{1b}[0m {text}\n  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n"
    );
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    // What Python's `_do_send` records for this pane: the text that was sent.
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec![text.clone().into()]);
    let probe = RecoveryProbe {
        screen: Mutex::new(frame.clone()),
        after_enter: frame.clone(),
        keys: Mutex::default(),
        generation: String::new(),
        federated: false,
    };
    let shown = flock.recovery_box_text(&probe, "%1", &frame);
    assert_eq!(
        Some(crate::terminal::screen::collapse(&text)),
        shown.map(|read| crate::terminal::screen::collapse(&read))
    );
}
// t3355 (t820 proof D) federated gap and partial rows: test_dunk_federated_recovery.py,
// oracle commit 79bc92fa. ScriptedProbe replays a script: read_pane call n returns
// screens[min(n, last)] and get_pane call n reports pids[min(n, last)]. A federated step reads
// the screen for its decision, reads again for dead_session, and then re-reads the composer at
// key time. A script therefore changes exactly the read it names. Each refusal is paired with a
// control that presses once, so a refusal cannot pass only because the candidate was never read.

struct ScriptedProbe {
    screens: Vec<String>,
    pids: Vec<u32>,
    reads: Mutex<usize>,
    lookups: Mutex<usize>,
    keys: Mutex<Vec<String>>,
}

impl SendEnv for ScriptedProbe {
    fn send_recovery_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.send_key(pane_id, key)
    }
    fn read_pane(&self, _: &str, _: i64, _: bool) -> Option<String> {
        let mut reads = self.reads.lock().unwrap();
        let index = (*reads).min(self.screens.len() - 1);
        *reads += 1;
        Some(self.screens[index].clone())
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
        (true, "sent".into())
    }
    fn federated_input(&self) -> bool {
        true
    }
}

impl PaneInfoEnv for ScriptedProbe {
    fn list_panes_grouped(&self) -> Vec<Value> {
        vec![self.get_pane("%1").unwrap()]
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        let mut lookups = self.lookups.lock().unwrap();
        let index = (*lookups).min(self.pids.len() - 1);
        *lookups += 1;
        (pane_id == "%1")
            .then(|| json!({"pane_id": "%1", "pane_pid": self.pids[index], "agent": "codex"}))
    }
}

fn scripted_probe(screens: Vec<String>, pids: Vec<u32>) -> ScriptedProbe {
    ScriptedProbe {
        screens,
        pids,
        reads: Mutex::default(),
        lookups: Mutex::default(),
        keys: Mutex::default(),
    }
}

fn scripted_frame(pending: &str) -> String {
    format!(
        "  >_ OpenAI Codex (v0.165.0)\n\n\x1b[1m\u{203a} {pending}\x1b[0m\n\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  ? for shortcuts\n"
    )
}

/// A persisted flock holding the failed federated candidate for the scripted pane.
fn scripted_candidate(dir: &tempfile::TempDir) -> Flock {
    let flock = persisted_flock(dir);
    flock.note_stuck_candidate(&failed_federated_candidate(&federated_probe()));
    flock
}

#[test]
fn federated_edit_between_steps_resets_the_stable_timer() {
    let frame = scripted_frame("pending text");
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    // Reads 0 and 1 belong to the step at 100; read 2 is the decision read of the step at 221.
    let mut probe = scripted_probe(
        vec![
            frame.clone(),
            frame.clone(),
            scripted_frame("pending text edited"),
        ],
        vec![std::process::id()],
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap()
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap(),
        "an edited composer restarts the stable timer"
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame], vec![std::process::id()]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );
    assert_eq!(vec!["Enter".to_string()], *control.keys.lock().unwrap());
}

#[test]
fn federated_changed_content_at_key_time_refuses() {
    let frame = scripted_frame("pending text");
    let changed = scripted_frame("pending text, changed by a human");
    assert_ne!(
        crate::terminal::composer::federated_composer(true, &frame),
        crate::terminal::composer::federated_composer(true, &changed),
        "precondition: the changed screen must hold different composer text"
    );
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    // Reads 0-1 are the step at 100 and reads 2-3 the decision of the step at 221; read 4 is
    // the composer re-read at key time.
    let mut probe = scripted_probe(
        vec![
            frame.clone(),
            frame.clone(),
            frame.clone(),
            frame.clone(),
            changed,
        ],
        vec![std::process::id()],
    );
    federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap();
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame], vec![std::process::id()]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );
}

#[test]
fn federated_changed_generation_at_key_time_refuses() {
    let frame = scripted_frame("pending text");
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    // Pane lookups 0 and 1 are the decision generations of the steps at 100 and 221. Lookup 2
    // is the generation re-read at key time, reported as pid 1, which no longer matches.
    let me = std::process::id();
    let mut probe = scripted_probe(vec![frame.clone()], vec![me, me, 1]);
    federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap();
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame], vec![me]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );
}

#[test]
fn federated_new_dialog_at_key_time_refuses() {
    let frame = scripted_frame("pending text");
    let dialog = format!("{frame}{MODAL}");
    assert!(
        !crate::terminal::screen::modal_open(&frame),
        "precondition: the composer frame is not a dialog"
    );
    assert!(
        crate::terminal::screen::modal_open(&dialog),
        "precondition: the dialog frame is a dialog"
    );
    // A modal on screen is never a federated frame (composer_tests), so the key-time composer
    // re-read refuses it. The modal branch of the key-time check is therefore not isolated here.
    assert_eq!(
        None,
        crate::terminal::composer::federated_composer(true, &dialog),
        "precondition: a dialog is never a federated composer"
    );
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    // The decision reads (2-3) see the composer; the key-time composer re-read (4) sees the dialog.
    let mut probe = scripted_probe(
        vec![
            frame.clone(),
            frame.clone(),
            frame.clone(),
            frame.clone(),
            dialog,
        ],
        vec![std::process::id()],
    );
    federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap();
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());

    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame], vec![std::process::id()]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );
}

/// t3501 federated row 13, positive control for the dialog test below: the same candidate with no dialog
/// presses once at the threshold, so the no-key result under the dialog is shown not to be vacuous.
#[test]
fn federated_dialog_control_without_the_dialog_presses_once() {
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    let mut probe = scripted_probe(
        vec![scripted_frame("pending text")],
        vec![std::process::id()],
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap()
    );
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap(),
        "the candidate without a dialog presses once at the threshold"
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
}

#[test]
fn federated_dialog_held_across_steps_never_authorizes_keys() {
    let dialog = format!("{}{MODAL}", scripted_frame("pending text"));
    assert!(
        crate::terminal::screen::modal_open(&dialog),
        "precondition: the dialog frame is a dialog"
    );
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    let mut probe = scripted_probe(vec![dialog], vec![std::process::id()]);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap()
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert!(probe.keys.lock().unwrap().is_empty());
}

#[test]
fn federated_failed_durable_reservation_never_presses() {
    let frame = scripted_frame("pending text");
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    let mut probe = scripted_probe(vec![frame.clone()], vec![std::process::id()]);
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap()
    );
    // A directory where the state file belongs makes the durable reservation save fail. (Removing
    // the directory does not: the save recreates it.)
    let state_file = dir.path().join("state.json");
    let _ = std::fs::remove_file(&state_file);
    std::fs::create_dir(&state_file).unwrap();
    assert!(
        flock.save().is_err(),
        "precondition: the reservation save must fail"
    );
    // The step decides not to press, then its closing save reports the failure as an error.
    let error = federated_recovery_step(&flock, &mut probe, 221.0, &|_| {})
        .expect_err("the failed reservation save is reported, not hidden");
    assert!(error.to_string().contains("state.json"), "{error}");
    assert!(probe.keys.lock().unwrap().is_empty());

    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame], vec![std::process::id()]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );
}

#[test]
fn federated_cancelled_or_replaced_candidates_never_press() {
    let frame = scripted_frame("pending text");
    // Control: the unchanged candidate presses once, so the refusals below test the change.
    let control_dir = tempfile::tempdir().unwrap();
    let control_flock = scripted_candidate(&control_dir);
    let mut control = scripted_probe(vec![frame.clone()], vec![std::process::id()]);
    federated_recovery_step(&control_flock, &mut control, 100.0, &|_| {}).unwrap();
    assert_eq!(
        1,
        federated_recovery_step(&control_flock, &mut control, 221.0, &|_| {}).unwrap()
    );

    for change in ["cancelled", "replaced"] {
        let dir = tempfile::tempdir().unwrap();
        let flock = persisted_flock(&dir);
        let mut candidate = failed_federated_candidate(&federated_probe());
        if change == "cancelled" {
            candidate.status = "cancelled".into();
        } else {
            candidate.attempt_generation = Some("replacement-generation".into());
        }
        flock.note_stuck_candidate(&candidate);
        let mut probe = scripted_probe(vec![frame.clone()], vec![std::process::id()]);
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap();
        assert_eq!(
            0,
            federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap(),
            "{change} candidate"
        );
        assert!(
            probe.keys.lock().unwrap().is_empty(),
            "{change} candidate pressed"
        );
    }
}

// t3355 (t820 proof D) scheduled row 29: test_dunk_scheduled_recovery.py::
// test_scheduled_failure_is_persisted_with_exact_expanded_text_and_generation (oracle 79bc92fa).
// A scheduled failure is an eligible candidate that holds the expanded text and the pane
// generation, and the state file holds the same entry as the in-memory candidate.
#[test]
fn scheduled_failure_is_persisted_with_expanded_text_and_generation() {
    let dir = tempfile::tempdir().unwrap();
    let flock = persisted_flock(&dir);
    let generation = federated_probe().generation;
    // t3501 scheduled row 29: the expansion is computed by the template path, not supplied. {id}
    // expands to the dunk id at send time, and the persisted text must be that expansion.
    let template = "Scheduled check {id}: read your inbox and finish the assigned work.";
    let expanded =
        crate::flock::dunks::expand_template(template, &crate::flock::dunks::Dunker::new("d7"));
    assert_eq!(
        "Scheduled check d7: read your inbox and finish the assigned work.", expanded,
        "{{id}} expands to the dunk id"
    );
    let mut message = Message::new("m1", "%1", "lane", &expanded, "send_text");
    message.scheduled = true;
    message.status = "sent".into();
    message.attempt_generation = Some(generation.clone());
    message.schedule_template = Some(template.into());
    flock.note_stuck_candidate(&message);
    let entry = flock
        .stuck_candidates
        .lock()
        .unwrap()
        .get("%1")
        .cloned()
        .expect("a scheduled failure is a stuck candidate");
    let snapshot = crate::flock::sends::candidate_snapshot("%1", &entry);
    assert_eq!(json!(true), snapshot["eligible"]);
    assert_eq!(json!(expanded), snapshot["text"]);
    assert_eq!(json!(generation), snapshot["generation"]);
    flock.save().unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap())
            .unwrap();
    assert_eq!(snapshot, saved["stuck_candidates"]["%1"]);
}

// t3355 (t820 proof D) wiring row 14, candidate half: test_dunk_recovery_wiring.py::
// test_failure_candidate_and_spent_budget_survive_restart (oracle 79bc92fa). The spent budget
// half is stuck.rs's spent_ledger_survives_restart_and_a_consumed_message_does_not_regain_budget.
// This test covers the failed candidate: after a save and a restore into a fresh flock, the
// candidate holds the same persisted fields. The comparison is on candidate_snapshot, the form
// the state file stores.
#[test]
fn failed_candidate_survives_a_restart_through_the_state_file() {
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    flock.save().unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap())
            .unwrap();
    let restarted_dir = tempfile::tempdir().unwrap();
    let restarted = persisted_flock(&restarted_dir);
    restarted.restore_snapshot(&saved);
    let before = flock.stuck_candidates.lock().unwrap().clone();
    let after = restarted.stuck_candidates.lock().unwrap().clone();
    assert_eq!(1, before.len());
    assert!(
        after.contains_key("%1"),
        "the candidate survives the restart"
    );
    assert_eq!(
        crate::flock::sends::candidate_snapshot("%1", &before["%1"]),
        crate::flock::sends::candidate_snapshot("%1", &after["%1"])
    );
    assert!(
        after["%1"].eligible,
        "the restored candidate stays eligible"
    );
}

// t3355 (t820 proof D) federated row 6, key level: test_dunk_federated_recovery.py::
// test_watchdog_waits_then_presses_once_without_replaying_text (oracle 79bc92fa). The Python test
// asserts one Enter after the threshold and no text sent. SendEnv has no text channel, and the
// federated step's only input call is send_key(Enter) (loops_stuck.rs federated_recovery_step).
// This test pins the key sequence across later steps, and that the watchdog never queues the
// candidate text into the outbox. The no-text property is structural in the Rust seam.
#[test]
fn federated_watchdog_presses_once_and_never_queues_the_candidate_text() {
    let dir = tempfile::tempdir().unwrap();
    let flock = scripted_candidate(&dir);
    *flock.stuck_config.lock().unwrap() = crate::stuck::Config {
        max_attempts: 1,
        cooldown_s: 1.0,
        ..crate::stuck::Config::default()
    };
    let mut probe = scripted_probe(
        vec![scripted_frame("pending text")],
        vec![std::process::id()],
    );
    assert_eq!(
        0,
        federated_recovery_step(&flock, &mut probe, 100.0, &|_| {}).unwrap()
    );
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "the threshold has not elapsed"
    );
    assert_eq!(
        1,
        federated_recovery_step(&flock, &mut probe, 221.0, &|_| {}).unwrap()
    );
    // The composer still shows the pending text, but the spent budget keeps later steps silent.
    for now in [240.0, 300.0] {
        assert_eq!(
            0,
            federated_recovery_step(&flock, &mut probe, now, &|_| {}).unwrap(),
            "at {now}"
        );
    }
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
    assert!(
        flock.outbox.lock().unwrap().is_empty(),
        "the watchdog must not queue the candidate text"
    );
}

// RestartRecoveryTests.test_a_human_continuation_is_refused_after_a_restart_by_the_real_matcher (row 52)
// Python's case 52 is the legacy real-matcher refusal, a different path. This test covers the
// federated native frame: Python's federated step submits the composer once and replays nothing,
// and the owner ruled that stays (i45030). The test pins that observed behaviour.
#[test]
fn a_restarted_federated_composer_is_submitted_once_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let before = in_memory_flock(&dir);
    let mut probe = federated_probe();
    before.note_stuck_candidate(&observed_candidate(&probe, "m1", "failed", true, 10.0));
    let after = persisted_flock(&dir);
    after.restore_snapshot(&before.snapshot());
    // The restarted worker runs the Python test's explicit config: one attempt, 120 s threshold.
    after.declare_stuck_config(crate::stuck::Config {
        enabled: true,
        threshold_s: 120.0,
        max_attempts: 1,
        cooldown_s: 60.0,
        observation_interval_s: 5.0,
    });
    // After the restart the composer holds our text and a line the person typed below it.
    // Enter leaves that composer in place (Python: "receipt unverified"), so the attempt budget
    // alone must stop a second press.
    let frame = "  >_ OpenAI Codex (v0.165.0)\n\n\x1b[1m\u{203a} pending text\x1b[0m\n  and one more thing I was typing\n\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  ? for shortcuts\n".to_string();
    *probe.screen.lock().unwrap() = frame.clone();
    probe.after_enter = frame;
    assert_eq!(
        0,
        federated_recovery_step(&after, &mut probe, 100.0, &|_| {}).unwrap()
    );
    assert!(
        probe.keys.lock().unwrap().is_empty(),
        "no key before the threshold"
    );
    assert_eq!(
        1,
        federated_recovery_step(&after, &mut probe, 221.0, &|_| {}).unwrap()
    );
    assert_eq!(vec!["Enter".to_string()], *probe.keys.lock().unwrap());
    assert_eq!(
        0,
        federated_recovery_step(&after, &mut probe, 341.0, &|_| {}).unwrap()
    );
    assert_eq!(
        vec!["Enter".to_string()],
        *probe.keys.lock().unwrap(),
        "one Enter and no replay: SendEnv has no text method, so a replay would show as a key"
    );
}

// RestartRecoveryTests.test_a_human_continuation_is_refused_after_a_restart_by_the_real_matcher (row 52)
// Python's case 52 is the non-federated real-matcher refusal, which the legacy strict scan reaches.
// The fixture binds a real native codex session as the daemon does. After the restart the exact
// text is pressed once, which is the positive control that makes the refusal meaningful. A
// composer holding a human continuation below our text gets no key at all.
#[cfg(target_os = "linux")]
#[test]
fn a_human_continuation_is_refused_after_a_restart() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct NativeCodexPane {
        pane_pid: i64,
        cwd: String,
        screen: Mutex<String>,
        after_enter: String,
        keys: Mutex<Vec<String>>,
    }
    impl SendEnv for NativeCodexPane {
        fn send_recovery_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
            self.send_key(pane_id, key)
        }
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
            false
        }
    }
    impl PaneInfoEnv for NativeCodexPane {
        fn list_panes_grouped(&self) -> Vec<Value> {
            vec![self.get_pane("%1").unwrap()]
        }
        fn get_pane(&self, pane_id: &str) -> Option<Value> {
            (pane_id == "%1").then(|| {
                json!({"pane_id": "%1", "pane_pid": self.pane_pid, "agent": "codex", "cwd": self.cwd})
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hooks = dir.path().join("hooks");
    let cwd = dir.path().join("cwd");
    for path in [&home, &hooks, &cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    let binary = dir.path().join("codex");
    // A copied multi-call sleep can exit when renamed; a shell blocked on its
    // retained stdin pipe preserves a live native process named `codex`.
    std::fs::copy("/bin/sh", &binary).unwrap();
    let mut child = ChildGuard(
        crate::testsupport::exec_retrying_busy(|| {
            std::process::Command::new(&binary)
                .args(["-c", "read line"])
                .stdin(std::process::Stdio::piped())
                .env("CODEX_HOME", &home)
                .env("DUNKINGSHEEP_CODEX_STATE_DIR", &hooks)
                .spawn()
        })
        .unwrap(),
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "fixture exited early"
    );
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    let session = "01a173ee-beed-7461-a560-e5f6c1f666a7";
    let rollout = home.join(format!("rollout-{session}.jsonl"));
    std::fs::write(
        &rollout,
        format!(
            "{}\n",
            json!({"type": "session_meta", "payload": {"session_id": session, "cwd": cwd}})
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
    std::fs::write(
        hooks.join("bound.json"),
        json!({"session_id": session, "pane_id": "%1", "root_pid": pid, "root_start": start, "cwd": cwd})
            .to_string(),
    )
    .unwrap();

    let pane_cwd = cwd.display().to_string();
    let generation_probe = NativeCodexPane {
        pane_pid: pid,
        cwd: pane_cwd.clone(),
        screen: Mutex::default(),
        after_enter: String::new(),
        keys: Mutex::default(),
    };
    let mut message = Message::new("m1", "%1", "lane", "pending text", "send_text");
    message.status = "failed".into();
    message.press_enter = true;
    message.created_at = 10.0;
    message.typed_at = Some(100.0);
    message.attempt_generation = crate::flock::sends::pane_generation(&generation_probe, "%1");
    let before = in_memory_flock(&dir);
    before.note_stuck_candidate(&message);

    // Each screen gets its own restart, all from the same persisted snapshot.
    let restart_and_scan = |screen: &str| -> Vec<String> {
        let after = persisted_flock(&dir);
        after.restore_snapshot(&before.snapshot());
        // The restarted worker runs the Python test's explicit config: one attempt, 120 s threshold.
        after.declare_stuck_config(crate::stuck::Config {
            enabled: true,
            threshold_s: 120.0,
            max_attempts: 1,
            cooldown_s: 60.0,
            observation_interval_s: 5.0,
        });
        // Enter leaves the composer as it was (Python: "receipt unverified"), so only the
        // attempt budget can stop a second press.
        let mut env = NativeCodexPane {
            pane_pid: pid,
            cwd: pane_cwd.clone(),
            screen: Mutex::new(screen.into()),
            after_enter: screen.into(),
            keys: Mutex::default(),
        };
        for now in [100.0, 221.0, 341.0] {
            strict_stuck_observer_scan(&after, &mut env, now).unwrap();
        }
        env.keys.into_inner().unwrap()
    };
    assert_eq!(
        vec!["Enter".to_string()],
        restart_and_scan("› pending text\n\n  model · /tmp/test\n"),
        "the exact text is recovered once after a restart"
    );
    assert!(
        restart_and_scan(
            "› pending text\n  and one more thing I was typing\n\n  model · /tmp/test\n"
        )
        .is_empty(),
        "the real matcher refuses a human continuation after a restart, with no key"
    );
}
