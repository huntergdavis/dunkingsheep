//! t3484 (T55 group D wiring rows 20 and 21, t820 proof D). Each test names its row and the Python
//! case it mirrors at 79bc92fa in tests/test_dunk_recovery_wiring.py (row 20 starts at line 223,
//! row 21 at line 254). A child of `sends_tests`: the parent's `Pane` double and
//! `real_receipt_forward` are shared without editing the parent's tests.
//!
//! The message goes through the real `Flock::send_now`, the real typed-receipt persist, and the
//! real `strict_stuck_observer_scan`. Differences from the Python fixture, each noted where it
//! applies: the composer shows the pending text from the start (the double cannot change its
//! screen on a typed send), no native Codex binding is attached (the pane record carries only the
//! pid), and the crash is a panic at the Enter key rather than an exception from `send_keys`.
//! The composer frame follows the row-38 fixture (`scheduled_candidate_fixture`). The observer
//! panes report the pane idle: without an idle status the stuck status is "unknown" and the
//! press is refused before any key.

use super::*;
use crate::flock::loops_stuck::strict_stuck_observer_scan;

const TEXT: &str = "wiring row 20 and 21: typed text";

/// The Python fixture's stuck policy: 120 s threshold, two attempts, 20 s observation, 60 s cooldown.
fn oracle_policy() -> crate::stuck::Config {
    crate::stuck::Config {
        enabled: true,
        threshold_s: 120.0,
        max_attempts: 2,
        cooldown_s: 60.0,
        observation_interval_s: 20.0,
    }
}

/// The pane record. `pane_pid` binds the attempt generation to this test process.
fn pane_record() -> serde_json::Value {
    serde_json::json!({"pane_id": P, "tab_label": "worker", "pane_pid": std::process::id()})
}

/// A Codex composer holding `text`: the frame the strict observer reads as the owned box.
fn composer_frame(text: &str) -> String {
    format!(
        "Previous response.\n\n\u{203a} {text}\n  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n"
    )
}

/// The same composer after a successful key: the placeholder, so the box reads as empty.
fn submitted_frame() -> String {
    composer_frame("")
}

// Python row 20 (test_real_submit_persists_typed_receipt_before_enter_then_observer_recovers):
// every Enter sees the typed receipt in state.json, the failed delivery leaves a typed candidate,
// and the observer presses once at the threshold (tick 1000, then tick 1120).
#[test]
fn wiring_20_real_submit_persists_typed_receipt_before_enter_then_observer_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    flock.declare_stuck_config(oracle_policy());
    let state_path = dir.path().join("state.json");
    let statuses_at_key = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let mut pane = Pane {
        screens: std::collections::BTreeMap::from([(P.to_string(), composer_frame(TEXT))]),
        panes: vec![pane_record()],
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };
    let seen = statuses_at_key.clone();
    *pane.on_send_key.borrow_mut() = Some(Box::new(move || {
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        seen.borrow_mut().push(
            saved["stuck_candidates"][P]["status"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }));

    let delivered = flock
        .send_now(
            &serde_json::json!(P),
            TEXT,
            crate::flock::sends::DirectSendOptions {
                hold_while_typing: false,
                ..Default::default()
            },
            crate::flock::sends::DirectTextMetadata::default(),
            &mut pane,
            &NoopOutboxWake,
            &NoopSnapshotStore,
            || None,
        )
        .unwrap();
    assert_eq!("failed", delivered["status"]);

    let statuses = statuses_at_key.borrow().clone();
    assert!(
        !statuses.is_empty(),
        "the unconfirmed Enter was pressed at least once"
    );
    assert!(
        statuses.iter().all(|status| status.as_str() == "typed"),
        "every Enter saw the typed receipt, got {statuses:?}"
    );
    let receipt = pane.events.iter().position(|event| *event == "receipt");
    let first_key = pane.events.iter().position(|event| *event == "key");
    assert!(
        receipt.is_some() && receipt < first_key,
        "the receipt precedes the first key: {:?}",
        pane.events
    );

    // The observer's first tick only starts the clock; the second, 120 s later, presses Enter once.
    let before = pane.keys.len();
    pane.statuses.insert(P.to_string(), "idle".to_string());
    pane.after_key_screen = Some(submitted_frame());
    assert_eq!(
        0,
        strict_stuck_observer_scan(&flock, &mut pane, 1000.0).unwrap()
    );
    assert_eq!(
        1,
        strict_stuck_observer_scan(&flock, &mut pane, 1120.0).unwrap()
    );
    assert_eq!(
        vec![(P.to_string(), "Enter".to_string())],
        pane.keys[before..].to_vec()
    );
}

// Python row 21 (test_crash_after_typing_before_enter_restores_the_bound_attempt): a crash between
// the typed receipt and Enter leaves a typed candidate and no key; a restarted worker restores the
// attempt and presses once at the threshold.
#[test]
fn wiring_21_crash_after_typing_before_enter_restores_the_bound_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let flock = std::sync::Arc::new(persistent_receipt_flock(dir.path()));
    flock.declare_stuck_config(oracle_policy());
    let mut pane = Pane {
        screens: std::collections::BTreeMap::from([(P.to_string(), composer_frame(TEXT))]),
        panes: vec![pane_record()],
        receipt_forward: Some(real_receipt_forward(
            flock.clone(),
            serde_json::json!({"pane_id": P}),
        )),
        ..Pane::default()
    };
    let crash: Box<dyn FnMut()> = Box::new(|| panic!("crash between typing and Enter"));
    *pane.on_send_key.borrow_mut() = Some(crash);

    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        flock.send_now(
            &serde_json::json!(P),
            TEXT,
            crate::flock::sends::DirectSendOptions {
                hold_while_typing: false,
                ..Default::default()
            },
            crate::flock::sends::DirectTextMetadata::default(),
            &mut pane,
            &NoopOutboxWake,
            &NoopSnapshotStore,
            || None,
        )
    }));
    assert!(crashed.is_err(), "the crash reaches the caller");
    assert!(
        pane.keys.is_empty(),
        "no Enter reached the pane before the crash"
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
    assert_eq!("typed", saved["stuck_candidates"][P]["status"]);

    // The restart is a new flock restored from the saved snapshot, with a fresh observer pane.
    let restart_dir = tempfile::tempdir().unwrap();
    let restarted = persistent_receipt_flock(restart_dir.path());
    restarted.restore_snapshot(&saved);
    restarted.declare_stuck_config(oracle_policy());
    let mut observer = Pane {
        screens: std::collections::BTreeMap::from([(P.to_string(), composer_frame(TEXT))]),
        panes: vec![pane_record()],
        statuses: std::collections::BTreeMap::from([(P.to_string(), "idle".to_string())]),
        after_key_screen: Some(submitted_frame()),
        ..Pane::default()
    };
    assert_eq!(
        0,
        strict_stuck_observer_scan(&restarted, &mut observer, 1000.0).unwrap()
    );
    assert_eq!(
        1,
        strict_stuck_observer_scan(&restarted, &mut observer, 1120.0).unwrap()
    );
    assert_eq!(vec![(P.to_string(), "Enter".to_string())], observer.keys);
}
