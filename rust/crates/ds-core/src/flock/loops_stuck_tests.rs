//! T36c slice 2 tests: the dialog observer step on an explicit clock, and the loop shell. The
//! Python has no dedicated test file for `_dialog_observer_step`; these pin the behaviour the
//! source states (30 s scan period, stable `since`, fresh interval on a replaced/non-modal pane,
//! nothing typed) and the loop's tick order.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::loops_stuck::{run_stuck_observer, DialogObserver, StuckEnv};
use crate::error::DunkError;

const MODAL: &str = "\u{1b}[1mApprove this?\u{1b}[0m\nEsc to cancel   Enter to confirm\n";

struct Fake {
    panes: Mutex<Result<Vec<Value>, String>>,
    screens: Mutex<BTreeMap<String, String>>,
    generations: Mutex<BTreeMap<String, String>>,
    notes: Mutex<Vec<(String, String, f64)>>,
    log: Mutex<Vec<&'static str>>,
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
    fn list_panes(&self) -> Result<Vec<Value>, DunkError> {
        self.panes
            .lock()
            .unwrap()
            .clone()
            .map_err(DunkError::Refused)
    }
    fn read_screen(&self, pane_id: &str) -> Option<String> {
        self.screens.lock().unwrap().get(pane_id).cloned()
    }
    fn pane_generation(&self, pane_id: &str) -> Option<String> {
        self.generations.lock().unwrap().get(pane_id).cloned()
    }
    fn note_stale_panel(&self, pane_id: &str, text: &str, since: f64) {
        self.notes
            .lock()
            .unwrap()
            .push((pane_id.into(), text.into(), since));
    }
    fn stuck_recovery_enabled(&self) -> bool {
        self.enabled
    }
    fn stuck_observer_step(&self) -> usize {
        self.log.lock().unwrap().push("step");
        0
    }
}

fn agent(id: &str) -> Value {
    json!({"pane_id": id, "agent": "claude"})
}

#[test]
fn a_modal_pane_is_reported_with_its_collapsed_screen_and_since_now() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    assert_eq!(observer.step(&fake, 100.0), 1);
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

#[test]
fn the_scan_runs_at_most_every_thirty_seconds() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
    observer.step(&fake, 129.9);
    assert_eq!(
        fake.notes.lock().unwrap().len(),
        1,
        "inside the period: no scan"
    );
    observer.step(&fake, 130.0);
    assert_eq!(fake.notes.lock().unwrap().len(), 2);
}

#[test]
fn since_is_kept_while_the_generation_and_screen_are_unchanged() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g1".into());
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
    observer.step(&fake, 140.0);
    assert_eq!(
        fake.notes.lock().unwrap()[1].2,
        100.0,
        "stable: the interval keeps running"
    );
}

#[test]
fn a_replaced_pane_or_a_changed_dialog_starts_a_fresh_interval() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g1".into());
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
    fake.generations
        .lock()
        .unwrap()
        .insert("%1".into(), "g2".into());
    observer.step(&fake, 140.0);
    assert_eq!(
        fake.notes.lock().unwrap()[1].2,
        140.0,
        "new generation: since resets"
    );
    fake.screen(
        "%1",
        "Approve something else?\nEsc to cancel   Enter to confirm\n",
    );
    observer.step(&fake, 180.0);
    assert_eq!(
        fake.notes.lock().unwrap()[2].2,
        180.0,
        "new dialog text: since resets"
    );
}

#[test]
fn a_pane_that_stops_showing_a_modal_forgets_its_interval() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
    fake.screen("%1", "plain output\n");
    assert_eq!(observer.step(&fake, 140.0), 0);
    fake.screen("%1", MODAL);
    observer.step(&fake, 180.0);
    assert_eq!(
        fake.notes.lock().unwrap().last().unwrap().2,
        180.0,
        "a fresh stability interval"
    );
}

#[test]
fn panes_without_an_agent_or_a_readable_screen_are_skipped() {
    let fake = Fake::with_panes(
        json!([{"pane_id": "%1"}, {"pane_id": "%2", "agent": ""}, agent("%3"), {"agent": "x"}]),
    );
    fake.screen("%1", MODAL);
    fake.screen("%2", MODAL);
    // %3 has an agent but no readable screen.
    let mut observer = DialogObserver::new();
    assert_eq!(observer.step(&fake, 100.0), 0);
    assert!(fake.notes.lock().unwrap().is_empty());
}

#[test]
fn a_failed_pane_listing_keeps_the_observations_and_still_spends_the_scan() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    fake.screen("%1", MODAL);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
    *fake.panes.lock().unwrap() = Err("backend down".into());
    assert_eq!(observer.step(&fake, 140.0), 0);
    *fake.panes.lock().unwrap() = Ok(vec![agent("%1")]);
    observer.step(&fake, 145.0);
    assert_eq!(
        fake.notes.lock().unwrap().len(),
        1,
        "the failed scan consumed the 30 s window"
    );
    observer.step(&fake, 170.0);
    assert_eq!(
        fake.notes.lock().unwrap().last().unwrap().2,
        100.0,
        "observations survived the failure"
    );
}

#[test]
fn the_signature_is_the_last_4000_characters() {
    let fake = Fake::with_panes(json!([agent("%1")]));
    let long = format!("{}\nEsc to cancel   Enter to confirm\n", "é".repeat(5000));
    fake.screen("%1", &long);
    let mut observer = DialogObserver::new();
    observer.step(&fake, 100.0);
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
