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
use super::loops_stuck::{run_stuck_observer, DialogObserver, StalePanelReporter, StuckEnv};
use super::mailbox::MailboxEnv;
use super::Flock;
use crate::db::ReadPool;
use crate::error::DunkError;
use crate::flock::loop_env::LoopFuture;
use crate::inbox_store::Store;

const MODAL: &str = "\u{1b}[1mApprove this?\u{1b}[0m\nEsc to cancel   Enter to confirm\n";

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
    fn read_screen(&self, pane_id: &str) -> LoopFuture<'_, Option<String>> {
        self.observed.lock().unwrap().push("screen");
        let screen = self.screens.lock().unwrap().get(pane_id).cloned();
        Box::pin(ready(screen))
    }
    fn pane_generation(&self, pane_id: &str) -> LoopFuture<'_, Option<String>> {
        self.observed.lock().unwrap().push("generation");
        let generation = self.generations.lock().unwrap().get(pane_id).cloned();
        Box::pin(ready(generation))
    }
    fn note_stale_panel(&self, pane_id: &str, text: &str, since: f64) -> LoopFuture<'_, ()> {
        self.observed.lock().unwrap().push("note");
        let pane_id = pane_id.to_owned();
        let text = text.to_owned();
        Box::pin(async move {
            self.notes
                .lock()
                .unwrap()
                .push((pane_id.into(), text.into(), since));
        })
    }
    fn stuck_recovery_enabled(&self) -> bool {
        self.enabled
    }
    fn stuck_observer_step(&self) -> LoopFuture<'_, usize> {
        Box::pin(async move {
            self.log.lock().unwrap().push("step");
            0
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
    fn caller_role(&self, _: &str) -> String {
        "owner".into()
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    fn backend_name(&self) -> String {
        "fixture".into()
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
fn note_stale_panel_waits_requires_identity_and_retries_only_failed_sinks() {
    let inbox = AlertInbox::new();
    let state = tempfile::tempdir().unwrap();
    let alert_db = state.path().join("alerts.db");
    let flock = alert_flock(&state);
    let mut reporter = StalePanelReporter::new();
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

    reporter.note_with_sinks(
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
    reporter.note_with_sinks(
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
    reporter.note_with_sinks(
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
    reporter.note_with_sinks(
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

    reporter.note_with_sinks(
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

    reporter.note_with_sinks(
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
    reporter.note_with_sinks(
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
async fn the_loop_scans_dialogs_every_tick_but_runs_the_stuck_step_only_when_enabled() {
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
