//! T16 slice: the activity rule, ported from tests/test_dunk_activity.py.
//!
//! The rule this module exists for: an ABSENT signal is UNKNOWN, never idle.

use std::collections::BTreeMap;

use ds_core::activity::{self, Activity, Turn, IDLE_CLAIM_S, STATES};

const NOW: f64 = 1_000_000.0;

#[test]
fn the_states_and_the_idle_window_are_the_pythons() {
    assert_eq!(["working", "waiting", "idle", "unknown"], STATES);
    assert_eq!(6.0 * 3600.0, IDLE_CLAIM_S);
}

#[test]
fn a_live_turn_is_working_and_reports_when_it_started() {
    let turn = Turn {
        started_at: Some(NOW - 60.0),
        ends_at: Some(NOW + 60.0),
        ended_at: None,
    };
    let (state, since, reason) = activity::turn_state(Some(&turn), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(Some("working"), state);
    assert_eq!(Some(NOW - 60.0), since);
    assert_eq!(None, reason);
}

#[test]
fn a_recently_ended_turn_is_idle_since_it_ended() {
    let turn = Turn {
        started_at: Some(NOW - 600.0),
        ends_at: Some(NOW - 500.0),
        ended_at: Some(NOW - 300.0),
    };
    let (state, since, reason) = activity::turn_state(Some(&turn), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(Some("idle"), state);
    assert_eq!(
        Some(NOW - 300.0),
        since,
        "idle since the turn ENDED, not since it started"
    );
    assert_eq!(None, reason);
}

#[test]
fn a_turn_that_ended_too_long_ago_is_unknown_not_idle() {
    let turn = Turn {
        started_at: Some(NOW - 90000.0),
        ends_at: Some(NOW - 89000.0),
        ended_at: Some(NOW - 80000.0),
    };
    let (state, since, reason) = activity::turn_state(Some(&turn), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(None, state, "silence is not idle");
    assert_eq!(Some(NOW - 80000.0), since);
    assert!(reason.unwrap().contains("silent since"));
}

#[test]
fn a_turn_that_expired_without_an_end_is_neither_working_nor_idle() {
    let turn = Turn {
        started_at: Some(NOW - 900.0),
        ends_at: Some(NOW - 600.0),
        ended_at: None,
    };
    let (state, since, reason) = activity::turn_state(Some(&turn), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(None, state);
    assert_eq!(Some(NOW - 600.0), since);
    assert!(reason.unwrap().contains("expired without an end"));
}

#[test]
fn no_turn_at_all_names_the_absent_source() {
    let (state, since, reason) = activity::turn_state(None, NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(None, state);
    assert_eq!(None, since);
    assert!(reason.unwrap().contains("no activity source"));
}

#[test]
fn a_pi_lane_with_no_turn_map_is_unknown() {
    let answer = activity::activity("%2", "pi", None, None, NOW);
    assert_eq!("unknown", answer.state);
    assert_eq!("pi turn events", answer.source);
    assert!(answer.reason.unwrap().contains("no turn state available"));
}

#[test]
fn a_pi_lane_reads_its_own_turn() {
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    turns.insert(
        "%2".to_string(),
        Turn {
            started_at: Some(NOW - 10.0),
            ends_at: Some(NOW + 10.0),
            ended_at: None,
        },
    );
    let answer = activity::activity("%2", "pi", None, Some(&turns), NOW);
    assert_eq!("working", answer.state);
    assert_eq!("pi turn events", answer.source);
}

#[test]
fn claude_answers_from_its_own_cli_mapping() {
    assert_eq!(
        "working",
        activity::claude_activity(Some("busy"), None).state
    );
    assert_eq!(
        "waiting",
        activity::claude_activity(Some("waiting"), None).state
    );
    assert_eq!("idle", activity::claude_activity(Some("idle"), None).state);
    let waiting = activity::claude_activity(Some("waiting"), Some("permission"));
    assert_eq!(Some("permission".to_string()), waiting.waiting_for);
}

#[test]
fn claude_with_no_session_is_unknown_not_idle() {
    let answer = activity::claude_activity(None, None);
    assert_eq!("unknown", answer.state);
    assert!(answer.reason.unwrap().contains("no claude session matches"));
    let odd = activity::claude_activity(Some("rebooting"), None);
    assert_eq!("unknown", odd.state);
    assert!(odd.reason.unwrap().contains("reported status"));
}

#[test]
fn codex_and_opencode_are_marked_inferred() {
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    turns.insert(
        "%3".to_string(),
        Turn {
            started_at: Some(NOW),
            ends_at: Some(NOW + 60.0),
            ended_at: None,
        },
    );
    let answer = activity::activity("%3", "codex", None, Some(&turns), NOW);
    assert_eq!("working", answer.state);
    assert_eq!(Some(true), answer.inferred);
    assert_eq!("codex declared turns", answer.source);
}

#[test]
fn an_unknown_harness_says_so_rather_than_guessing() {
    let answer = activity::activity("%9", "vim", None, None, NOW);
    assert_eq!("unknown", answer.state);
    assert_eq!("vim", answer.source);
    assert!(answer
        .reason
        .unwrap()
        .contains("no activity source for harness"));
    assert_eq!("none", activity::activity("%9", "", None, None, NOW).source);
}

#[test]
fn a_line_for_a_log_names_the_reason_when_unknown() {
    let unknown = Activity {
        state: "unknown".to_string(),
        source: "pi turn events".to_string(),
        reason: Some("no turn state available".to_string()),
        ..Activity::default()
    };
    assert_eq!(
        "unknown (pi turn events: no turn state available)",
        activity::say(&unknown)
    );
    let working = Activity {
        state: "working".to_string(),
        source: "pi".to_string(),
        since: Some(NOW - 7200.0),
        since_hours: 2,
        ..Activity::default()
    };
    assert_eq!("working since 2h ago", activity::say(&working));
}

#[test]
fn hours_ago_is_never_negative_and_zero_without_a_time() {
    assert_eq!(0.0, activity::now_hours(None, NOW));
    assert_eq!(0.0, activity::now_hours(Some(0.0), NOW));
    assert_eq!(2.0, activity::now_hours(Some(NOW - 7200.0), NOW));
    assert_eq!(0.0, activity::now_hours(Some(NOW + 500.0), NOW));
}
