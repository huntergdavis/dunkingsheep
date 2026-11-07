//! t3485 (T55 group D scheduled row 40, t820 proof D): the Rust counterpart of Python
//! test_styled_schedule_survives_restart_with_provenance_for_echo (tests/test_dunk_scheduled_recovery.py
//! at 79bc92fa, line 182), with cases that change one factor at a time. A child of `sends_tests`, so
//! the parent's `Pane`, `persistent_recovery_flock` and `pane_generation` are shared.
//!
//! The flock is persisted. A non-persisted flock refuses every press ("reservation not durable"),
//! so its threshold tick can only return 0. Timing matches Python: the tick at 1000 starts the
//! clock, and the tick at 1120 presses once, with one attempt as the Python fixture sets.
//!
//! Matrix (A-D, the Python frame and the contiguous frame, restart on and off): A is the oracle, B is
//! Python's own restart frame, C is the contiguous frame, D is Python's frame without a restart.
//! Variants V1-V3 start from the row-38 fixture shape and add one factor each: V1 is that fixture
//! (note, no typed_at, full dunk row), V2 adds typed_at, and V3 records through `remember`.
//! A failure message carries the gate values after the first tick and the candidate's recovery
//! outcome after the second.

use super::*;
use crate::flock::loops_stuck::strict_stuck_observer_scan;

const TEXT: &str = "Scheduled check d7: read your inbox and finish the assigned work.";
const ECHO: &str = "[inbox] new message for Codex. Read it with: dunkingsheep inbox read";
const FOOTER: &str = "  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n";

/// The factors one case changes.
#[derive(Clone, Copy)]
struct Setup {
    /// Remember the sent echo first (Python `_remember(previous)`).
    echo: bool,
    /// Record the failed schedule through `remember` (history and verdict), not `note` alone.
    remember: bool,
    /// Set `typed_at` on the failed schedule, as the typed transport does.
    typed_at: bool,
    /// Carry the target agent and cwd on the dunk row, as the row-38 fixture does.
    full_dunk: bool,
    /// Restart from the saved state before the observer ticks.
    restart: bool,
}

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

/// Python's oracle screen: the styled echo above the bright schedule, two blank rows, the footer.
fn echo_screen() -> String {
    format!("\x1b[1;2m\u{203a}\x1b[0m {ECHO}\n\n\x1b[1m\u{203a}\x1b[0m {TEXT}\n\n\n{FOOTER}")
}

/// Python's FRAME from the scheduled fixture: plain text, two blank rows before the footer.
fn python_frame() -> String {
    format!("Previous response.\n\n\u{203a} {TEXT}\n\n\n{FOOTER}")
}

/// The contiguous shape the row-38 fixture uses: no blank row between the composer and the footer.
fn contiguous_frame() -> String {
    format!("Previous response.\n\n\u{203a} {TEXT}\n{FOOTER}")
}

/// The composer after the Enter clears: the placeholder row with one blank row before the footer,
/// as Python's fake shows it after a submit. Without this the post-press read is "unchanged" and
/// the observer counts no recovery, even though the key was sent.
fn cleared_frame() -> String {
    format!("Previous response.\n\n\u{203a} \n\n{FOOTER}")
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
/// recorded as failed. The dunk row carries the same text, so the schedule is current.
fn remember_failed_schedule(flock: &Flock, env: &Pane, created: f64, setup: Setup) {
    let mut dunk = serde_json::json!({
        "id": "d7",
        "target_pane_id": P,
        "text": TEXT,
        "running": true,
    });
    if setup.full_dunk {
        dunk["target_agent"] = serde_json::json!("codex");
        dunk["target_cwd"] = serde_json::json!("/work");
    }
    flock
        .dunks
        .load(serde_json::json!({"dunkers": [dunk]}).as_object().unwrap());
    flock.remember_typed(P, TEXT);
    let mut message = Message::new("m7", P, "CoS", TEXT, "d7");
    message.created_at = created;
    message.press_enter = true;
    message.status = "failed".into();
    message.scheduled = true;
    message.schedule_template = Some(TEXT.into());
    message.attempt_generation = pane_generation(env, P);
    if setup.typed_at {
        message.typed_at = Some(created);
    }
    if setup.remember {
        flock.remember(&message);
    } else {
        flock.note_stuck_candidate(&message);
    }
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

/// The gate values the strict observer walks for this pane and screen.
fn gate_report(flock: &Flock, observer: &Pane, screen: &str) -> String {
    let content = flock.recovery_box_text(observer, P, screen);
    let status = flock.stuck_status(observer, P);
    let region = crate::terminal::composer::composer_region_text(screen);
    let (box_value, box_detail) = crate::terminal::screen::box_text(screen);
    let candidate_generation = flock
        .stuck_candidates
        .lock()
        .unwrap()
        .get(P)
        .map(|entry| entry.generation.clone());
    let message = flock.stuck_candidate_message(P, observer).is_some();
    let known = flock.sent_text_by_pane.lock().unwrap().get(P).cloned();
    format!(
        "gates: recovery_box_text={content:?} stuck_status={status} composer_region={region:?} \
         box_text={box_value:?} box_detail={box_detail:?} candidate_message={message} \
         known={known:?} candidate_generation={candidate_generation:?} pane_generation={:?}",
        pane_generation(observer, P)
    )
}

/// The candidate's state after the observer has run, including any recorded refusal.
fn candidate_outcome(flock: &Flock) -> String {
    match flock.stuck_candidates.lock().unwrap().get(P) {
        Some(entry) => format!(
            "candidate: status={} eligible={} typed_at={:?} outcome={:?} detail={:?}",
            entry.status,
            entry.eligible,
            entry.typed_at,
            entry.recovery_outcome,
            entry.recovery_detail
        ),
        None => "candidate: none".to_string(),
    }
}

/// One scenario: the setup, an optional restart, then the observer ticks at 1000 and 1120. The
/// composer clears after the Enter, so a recorded press is counted as a recovery, as in Python.
/// Returns both tick results, the keys sent, and a report for a failure message.
fn scenario(setup: Setup, screen: &str) -> (usize, usize, usize, String) {
    let base = crate::clock::now_s();
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    let flock = persistent_recovery_flock(dir.path(), &state);
    flock.declare_stuck_config(oracle_policy());
    let setup_pane = idle_pane("");
    if setup.echo {
        remember_sent_echo(&flock, &setup_pane, base - 2.0);
    }
    remember_failed_schedule(&flock, &setup_pane, base, setup);
    let flock = if setup.restart {
        restart_from_state(flock, dir.path(), &state)
    } else {
        flock
    };
    let mut observer = idle_pane(screen);
    let first = strict_stuck_observer_scan(&flock, &mut observer, base + 1000.0).unwrap();
    let gates = gate_report(&flock, &observer, screen);
    observer.after_key_screen = Some(cleared_frame());
    let second = strict_stuck_observer_scan(&flock, &mut observer, base + 1120.0).unwrap();
    (
        first,
        second,
        observer.keys.len(),
        format!("{gates} | after 1120: {}", candidate_outcome(&flock)),
    )
}

/// A: Python's oracle, test_styled_schedule_survives_restart_with_provenance_for_echo.
#[test]
fn scheduled_40_a_oracle_styled_echo_restart_presses_once() {
    let setup = Setup {
        echo: true,
        remember: true,
        typed_at: true,
        full_dunk: false,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &echo_screen());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// B: Python's restart frame (test_restart_uses_saved_schedule_and_budget_without_new_send), no echo.
#[test]
fn scheduled_40_b_python_frame_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: true,
        typed_at: true,
        full_dunk: false,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &python_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// C: the contiguous frame, restart on, no echo.
#[test]
fn scheduled_40_c_contiguous_frame_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: true,
        typed_at: true,
        full_dunk: false,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &contiguous_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// D: Python's frame with no restart.
#[test]
fn scheduled_40_d_python_frame_without_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: true,
        typed_at: true,
        full_dunk: false,
        restart: false,
    };
    let (first, second, keys, report) = scenario(setup, &python_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// V1: the row-38 fixture shape, which presses in CI: `note` without typed_at, a full dunk row,
/// the contiguous frame, restart on. Expected to press once.
#[test]
fn scheduled_40_v1_row38_fixture_shape_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: false,
        typed_at: false,
        full_dunk: true,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &contiguous_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// V2: V1 plus typed_at on the failed schedule, which the real typed transport sets.
#[test]
fn scheduled_40_v2_row38_shape_with_typed_at_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: false,
        typed_at: true,
        full_dunk: true,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &contiguous_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}

/// V3: V1 but recorded through `remember` (history and verdict), without typed_at.
#[test]
fn scheduled_40_v3_row38_shape_with_remember_restart_presses_once() {
    let setup = Setup {
        echo: false,
        remember: true,
        typed_at: false,
        full_dunk: true,
        restart: true,
    };
    let (first, second, keys, report) = scenario(setup, &contiguous_frame());
    assert_eq!((0, 1, 1), (first, second, keys), "{report}");
}
