//! T16 slice: the activity rule, ported from tests/test_dunk_activity.py.
//!
//! The rule this module exists for: an ABSENT signal is UNKNOWN, never idle.

use std::collections::BTreeMap;

use ds_core::activity::{self, Activity, ClaudeProbe, Turn, IDLE_CLAIM_S, STATES};

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
        expires_at: Some(NOW + 60.0),
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
        expires_at: Some(NOW - 500.0),
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
        expires_at: Some(NOW - 89000.0),
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
        expires_at: Some(NOW - 600.0),
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
    let answer = activity::activity("%2", "pi", ClaudeProbe::NoClient, None, NOW, IDLE_CLAIM_S);
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
            expires_at: Some(NOW + 10.0),
            ended_at: None,
        },
    );
    let answer = activity::activity(
        "%2",
        "pi",
        ClaudeProbe::NoClient,
        Some(&turns),
        NOW,
        IDLE_CLAIM_S,
    );
    assert_eq!("working", answer.state);
    assert_eq!("pi turn events", answer.source);
}

#[test]
fn claude_answers_from_its_own_cli_mapping() {
    assert_eq!(
        "working",
        activity::claude_activity(ClaudeProbe::Session(Some("busy")), None).state
    );
    assert_eq!(
        "waiting",
        activity::claude_activity(ClaudeProbe::Session(Some("waiting")), None).state
    );
    assert_eq!(
        "idle",
        activity::claude_activity(ClaudeProbe::Session(Some("idle")), None).state
    );
    let waiting =
        activity::claude_activity(ClaudeProbe::Session(Some("waiting")), Some("permission"));
    assert_eq!(Some("permission".to_string()), waiting.waiting_for);
}

#[test]
fn claude_with_no_session_is_unknown_not_idle() {
    let answer = activity::claude_activity(ClaudeProbe::Session(None), None);
    assert_eq!("unknown", answer.state);
    assert!(answer.reason.unwrap().contains("no claude session matches"));
    let odd = activity::claude_activity(ClaudeProbe::Session(Some("rebooting")), None);
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
            expires_at: Some(NOW + 60.0),
            ended_at: None,
        },
    );
    let answer = activity::activity(
        "%3",
        "codex",
        ClaudeProbe::NoClient,
        Some(&turns),
        NOW,
        IDLE_CLAIM_S,
    );
    assert_eq!("working", answer.state);
    assert_eq!(Some(true), answer.inferred);
    assert_eq!("codex declared turns", answer.source);
}

#[test]
fn an_unknown_harness_says_so_rather_than_guessing() {
    let answer = activity::activity("%9", "vim", ClaudeProbe::NoClient, None, NOW, IDLE_CLAIM_S);
    assert_eq!("unknown", answer.state);
    assert_eq!("vim", answer.source);
    assert!(answer
        .reason
        .unwrap()
        .contains("no activity source for harness"));
    assert_eq!(
        "none",
        activity::activity("%9", "", ClaudeProbe::NoClient, None, NOW, IDLE_CLAIM_S).source
    );
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
        activity::say(&unknown, NOW)
    );
    // t984 finding 1: the record is BUILT BY activity(), not hand-assembled with a
    // rendered field. The first version of this test set the hours itself, so it could
    // not fail whatever the module did - and a lane working for two hours printed
    // "since 0h ago".
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    turns.insert(
        "%2".to_string(),
        Turn {
            started_at: Some(NOW - 7200.0),
            expires_at: Some(NOW + 60.0),
            ended_at: None,
        },
    );
    let working = activity::activity(
        "%2",
        "pi",
        ClaudeProbe::NoClient,
        Some(&turns),
        NOW,
        IDLE_CLAIM_S,
    );
    assert_eq!("working since 2h ago", activity::say(&working, NOW));
}

#[test]
fn hours_ago_is_never_negative_and_zero_without_a_time() {
    assert_eq!(0.0, activity::now_hours(None, NOW));
    assert_eq!(0.0, activity::now_hours(Some(0.0), NOW));
    assert_eq!(2.0, activity::now_hours(Some(NOW - 7200.0), NOW));
    assert_eq!(0.0, activity::now_hours(Some(NOW + 500.0), NOW));
}

/// t984 finding 2: the PRODUCERS write `expires_at`, and the first version of `Turn`
/// called the field `ends_at` with no Deserialize at all - so the obvious derive would
/// have silently ignored `expires_at` and sent every live turn down the
/// "expired without an end" branch: a working lane read as unknown.
#[test]
fn a_real_producer_record_parses_as_a_live_turn() {
    // The shape `dunk_core.turn_start` and both integrations actually write.
    let record = r#"{"started_at": 999940.0, "expires_at": 1000060.0, "ended_at": null}"#;
    let turn: Turn = serde_json::from_str(record).expect("a producer record must parse");
    assert_eq!(
        Some(1000060.0),
        turn.expires_at,
        "expires_at is the live field"
    );
    let (state, since, reason) = activity::turn_state(Some(&turn), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(
        Some("working"),
        state,
        "a live turn is WORKING, not unknown"
    );
    assert_eq!(Some(999940.0), since);
    assert_eq!(None, reason);
    // The older fixture spelling still parses (the alias).
    let older: Turn = serde_json::from_str(r#"{"ends_at": 1000060.0}"#).expect("alias");
    assert_eq!(Some(1000060.0), older.expires_at);
    // And an empty record is "no activity source", as the Python's {} is falsy there.
    let empty: Turn = serde_json::from_str("{}").expect("empty");
    let (state, _since, reason) = activity::turn_state(Some(&empty), NOW, IDLE_CLAIM_S, "pi");
    assert_eq!(None, state);
    assert!(reason.unwrap().contains("no activity source"));
}

/// t984 finding 3: the claude half keeps the Python's THREE failure reasons.
#[test]
fn the_claude_probe_names_the_cause_it_actually_had() {
    let no_client = activity::claude_activity(ClaudeProbe::NoClient, None);
    assert_eq!("unknown", no_client.state);
    assert!(no_client
        .reason
        .unwrap()
        .contains("no claude status client available"));

    let failed = activity::claude_activity(ClaudeProbe::Failed("boom".to_string()), None);
    assert_eq!("unknown", failed.state);
    let reason = failed.reason.unwrap();
    assert!(
        reason.contains("claude agents --json failed: boom"),
        "{reason}"
    );

    let no_session = activity::claude_activity(ClaudeProbe::Session(None), None);
    assert!(no_session
        .reason
        .unwrap()
        .contains("no claude session matches"));
}

/// t984 finding 4: the only env knob in this module is honoured again.
#[test]
fn the_idle_claim_window_comes_from_the_environment() {
    let default = activity::idle_claim_s_from(&ds_core::config::Env::from_pairs(&[]));
    assert_eq!(IDLE_CLAIM_S, default);
    let tuned = activity::idle_claim_s_from(&ds_core::config::Env::from_pairs(&[(
        "DUNKINGSHEEP_IDLE_CLAIM_S",
        "60",
    )]));
    assert_eq!(
        60.0, tuned,
        "DUNKINGSHEEP_IDLE_CLAIM_S is read, not ignored"
    );
}

/// t984 note: the Python's dict ALWAYS carries reason and since, so a consumer doing
/// `record["reason"]` must not KeyError on a real answer.
#[test]
fn a_serialised_answer_always_carries_since_and_reason() {
    let answer = activity::activity("%2", "pi", ClaudeProbe::NoClient, None, NOW, IDLE_CLAIM_S);
    let value = serde_json::to_value(&answer).expect("serialises");
    let object = value.as_object().expect("an object");
    assert!(
        object.contains_key("since"),
        "since must be present, even when null"
    );
    assert!(
        object.contains_key("reason"),
        "reason must be present, even when null"
    );
    assert!(object.contains_key("state") && object.contains_key("source"));
}
