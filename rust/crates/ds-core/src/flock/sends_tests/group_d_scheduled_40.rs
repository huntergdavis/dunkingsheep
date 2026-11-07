//! t3485 (T55 group D scheduled row 40, t820 proof D): the Rust counterpart of Python
//! test_styled_schedule_survives_restart_with_provenance_for_echo (tests/test_dunk_scheduled_recovery.py
//! at 79bc92fa, line 182). A child of `sends_tests`, so the parent's `Pane`, `persistent_recovery_flock`
//! and `pane_generation` are shared.
//!
//! The order follows Python: the sent echo is remembered first, the scheduled reminder fails to
//! submit and is remembered as failed, the screen shows the styled echo above the bright schedule,
//! and the flock restarts from its state file. The saved state carries the candidates and the
//! verdicts only, as Python's `_save` does. `sent_text_by_pane` and the message history are not
//! restored, so the echo's provenance has to come from the candidate's transcript texts.
//!
//! The flock is persisted. A non-persisted flock refuses every press ("reservation not durable"),
//! so its threshold tick can only return 0. Timing matches Python: the tick at 1000 starts the
//! clock, and the tick at 1120 presses once, with one attempt as the Python fixture sets.

use super::*;
use crate::flock::loops_stuck::strict_stuck_observer_scan;

const TEXT: &str = "Scheduled check d7: read your inbox and finish the assigned work.";
const ECHO: &str = "[inbox] new message for Codex. Read it with: dunkingsheep inbox read";
const FOOTER: &str = "  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n";

/// The Python fixture's stuck policy: 120 s threshold, one attempt, 20 s observation, 60 s cooldown.
fn oracle_policy() -> crate::stuck::Config {
    crate::stuck::Config {
        enabled: true,
        threshold_s: 120.0,
        max_attempts: 1,
        cooldown_s: 60.0,
        observation_interval_s: 20.0,
    }
}

/// An idle pane showing `screen`. The pane record carries this process's pid, so the attempt
/// generation is real. Idle is the status the observer's stuck status accepts.
fn idle_pane(screen: &str) -> Pane {
    Pane {
        screens: std::collections::BTreeMap::from([(P.to_string(), screen.to_string())]),
        panes: vec![serde_json::json!({
            "pane_id": P,
            "tab_label": "worker",
            "pane_pid": std::process::id(),
        })],
        statuses: std::collections::BTreeMap::from([(P.to_string(), "idle".to_string())]),
        ..Pane::default()
    }
}

/// Python's screen: the styled echo above the bright schedule, two blank rows, then the footer.
fn echo_screen() -> String {
    format!("\x1b[1;2m\u{203a}\x1b[0m {ECHO}\n\n\x1b[1m\u{203a}\x1b[0m {TEXT}\n\n\n{FOOTER}")
}

/// The same schedule with no echo line, for the control.
fn schedule_only_screen() -> String {
    format!("\x1b[1m\u{203a}\x1b[0m {TEXT}\n\n\n{FOOTER}")
}

/// Python `_new_message` plus `_remember` for the echo this flock sent before the schedule.
fn remember_sent_echo(flock: &Flock, env: &Pane, created: f64) {
    let mut echo = Message::new("m-echo", P, "Codex", ECHO, "send_text");
    echo.created_at = created;
    echo.press_enter = true;
    echo.status = "sent".into();
    echo.attempt_generation = pane_generation(env, P);
    flock.remember(&echo);
}

/// Python `failed_schedule`: the reminder is typed, its submit is swallowed, and the message is
/// remembered as failed. The dunk row carries the same text, so the schedule is current.
fn remember_failed_schedule(flock: &Flock, env: &Pane, created: f64) {
    flock.dunks.load(
        serde_json::json!({"dunkers": [{
            "id": "d7",
            "target_pane_id": P,
            "text": TEXT,
            "running": true,
        }]})
        .as_object()
        .unwrap(),
    );
    flock.remember_typed(P, TEXT);
    let mut message = Message::new("m7", P, "CoS", TEXT, "d7");
    message.created_at = created;
    message.press_enter = true;
    message.status = "failed".into();
    message.scheduled = true;
    message.schedule_template = Some(TEXT.into());
    message.attempt_generation = pane_generation(env, P);
    message.typed_at = Some(created);
    flock.remember(&message);
}

/// Save the flock, then restart from the state file: only the saved snapshot comes back.
fn restart_from_state(flock: Flock, dir: &std::path::Path, state: &std::path::Path) -> Flock {
    flock.save().unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    drop(flock);
    let restored = persistent_recovery_flock(dir, state);
    restored.restore_snapshot(&saved);
    restored.declare_stuck_config(oracle_policy());
    restored
}

/// The restored candidate's fields that decide the press, for a failure message.
fn candidate_summary(flock: &Flock) -> String {
    match flock.stuck_candidates.lock().unwrap().get(P) {
        Some(entry) => format!(
            "status={} eligible={} generation={:?} transcript={:?} text_is_schedule={}",
            entry.status,
            entry.eligible,
            entry.generation,
            entry.transcript_texts,
            entry.text == TEXT,
        ),
        None => "no candidate".to_string(),
    }
}

/// Oracle test_styled_schedule_survives_restart_with_provenance_for_echo (scheduled row 40): after a
/// restart, the styled echo above the bright schedule does not stop the one recovery press. The
/// press comes at the second tick, 120 s after the first, and not at the first.
#[test]
fn scheduled_40_restart_with_styled_echo_presses_once_like_python() {
    let base = crate::clock::now_s();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state);
    flock.declare_stuck_config(oracle_policy());
    let setup = idle_pane("");
    remember_sent_echo(&flock, &setup, base - 2.0);
    remember_failed_schedule(&flock, &setup, base);
    let restored = restart_from_state(flock, dir.path(), &state);

    let mut observer = idle_pane(&echo_screen());
    assert_eq!(
        0,
        strict_stuck_observer_scan(&restored, &mut observer, base + 1000.0).unwrap(),
        "no press at the first sighting"
    );
    assert!(observer.keys.is_empty(), "no key before 120 s");
    assert_eq!(
        1,
        strict_stuck_observer_scan(&restored, &mut observer, base + 1120.0).unwrap(),
        "one press 120 s after the first sighting; candidate: {}",
        candidate_summary(&restored)
    );
    assert_eq!(vec![(P.to_string(), "Enter".to_string())], observer.keys);
}

/// Control, not an oracle case: the same restart with no echo line presses once at the same ticks.
/// If this passes while the oracle-shaped test fails, the divergence is the echo's provenance.
#[test]
fn scheduled_40_control_restart_without_echo_presses_once() {
    let base = crate::clock::now_s();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state);
    flock.declare_stuck_config(oracle_policy());
    remember_failed_schedule(&flock, &idle_pane(""), base);
    let restored = restart_from_state(flock, dir.path(), &state);

    let mut observer = idle_pane(&schedule_only_screen());
    assert_eq!(
        0,
        strict_stuck_observer_scan(&restored, &mut observer, base + 1000.0).unwrap()
    );
    assert_eq!(
        1,
        strict_stuck_observer_scan(&restored, &mut observer, base + 1120.0).unwrap(),
        "candidate: {}",
        candidate_summary(&restored)
    );
    assert_eq!(1, observer.keys.len(), "exactly one key");
}
