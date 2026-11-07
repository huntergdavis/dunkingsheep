//! Parity matrices for the push sweep's two pure decisions, pinned to the Python oracle.
//!
//! Expected values are the output of origin/main `dunk_core.py::_unread_mail` (t1053, PR #209)
//! and `dunk_core.py::_lane_agent_alive` (t1029, PR #127), run with the Python source unchanged
//! and `time.time()` fixed at NOW = 1_000_000.0 and grace = 600 s. Each row is one oracle case.

use std::cell::Cell;

use super::*;

const NOW: f64 = 1_000_000.0;
const GRACE: f64 = 600.0;

fn m(subject: &str, body: &str, created_at: Option<f64>) -> Mail {
    Mail {
        subject: Some(subject.to_string()),
        body: Some(body.to_string()),
        created_at,
    }
}

#[test]
fn parity_unread_gate_matches_python_unread_mail() {
    // (name, unread count, peeked messages, Python result)
    let rows: Vec<(&str, i64, Vec<Mail>, bool)> = vec![
        ("no_unread", 0, vec![m("hi", "", Some(NOW))], false),
        (
            "task_subject_fresh",
            1,
            vec![m("TASK t1 [high] x", "", Some(NOW))],
            false,
        ),
        (
            "task_body_fresh",
            1,
            vec![m("", "TASK t1 [high] x", Some(NOW))],
            false,
        ),
        (
            "mentions_task_later",
            1,
            vec![m("re: TASK t1", "", Some(NOW))],
            true,
        ),
        ("fresh_plain", 1, vec![m("note", "", Some(NOW - 1.0))], true),
        (
            "age_equal_grace",
            1,
            vec![m("note", "", Some(NOW - 600.0))],
            true,
        ),
        (
            "age_past_grace",
            1,
            vec![m("note", "", Some(NOW - 601.0))],
            false,
        ),
        ("created_none", 1, vec![m("note", "", None)], true),
        ("created_zero", 1, vec![m("note", "", Some(0.0))], true),
        (
            "task_skipped_then_fresh",
            2,
            vec![m("TASK t1", "", Some(NOW)), m("note", "", Some(NOW))],
            true,
        ),
        ("count_says_one_read_empty", 1, vec![], false),
        (
            "stale_only",
            2,
            vec![m("a", "", Some(NOW - 900.0)), m("b", "TASK x", Some(NOW))],
            false,
        ),
    ];
    for (name, unread, messages, python) in rows {
        assert_eq!(
            unread_mail(unread, &messages, NOW, GRACE),
            python,
            "unread gate row `{name}` diverges from _unread_mail"
        );
    }
}

fn proc(comm: &str, state: &str) -> ChildProc {
    ChildProc {
        comm: comm.to_string(),
        state: state.to_string(),
    }
}

#[test]
fn parity_liveness_matches_python_lane_agent_alive() {
    // (name, direct children (None = table unknown), walk answer (None = walk failed),
    //  whether Python consults the walk, Python result)
    type Case = (
        &'static str,
        Option<Vec<ChildProc>>,
        Option<bool>,
        bool,
        Option<bool>,
    );
    let rows: Vec<Case> = vec![
        ("table_unknown", None, Some(true), false, None),
        (
            "direct_agent",
            Some(vec![proc("claude", "S")]),
            Some(false),
            false,
            Some(true),
        ),
        (
            "dead_agent_walk_finds",
            Some(vec![proc("claude", "Z")]),
            Some(true),
            true,
            Some(true),
        ),
        (
            "dead_agent_walk_empty",
            Some(vec![proc("claude", "Z")]),
            Some(false),
            true,
            Some(false),
        ),
        (
            "bare_shell_walk_finds_deep",
            Some(vec![proc("bash", "S")]),
            Some(true),
            true,
            Some(true),
        ),
        (
            "bare_shell_walk_empty",
            Some(vec![proc("bash", "S")]),
            Some(false),
            true,
            Some(false),
        ),
        (
            "bare_shell_walk_raises",
            Some(vec![proc("bash", "S")]),
            None,
            true,
            None,
        ),
        (
            "empty_table_walk_empty",
            Some(vec![]),
            Some(false),
            true,
            Some(false),
        ),
        ("empty_table_walk_raises", Some(vec![]), None, true, None),
        (
            "dead_state_x_walk_finds",
            Some(vec![proc("pi", "X")]),
            Some(true),
            true,
            Some(true),
        ),
        (
            "direct_agent_walk_raises",
            Some(vec![proc("claude", "S")]),
            None,
            false,
            Some(true),
        ),
    ];
    for (name, entries, walk, python_walks, python) in rows {
        let consulted = Cell::new(false);
        let got = lane_agent_alive(entries.as_deref(), || {
            consulted.set(true);
            walk
        });
        assert_eq!(
            got, python,
            "liveness row `{name}` diverges from _lane_agent_alive"
        );
        assert_eq!(
            consulted.get(),
            python_walks,
            "liveness row `{name}` consults the descendant walk differently from Python"
        );
    }
}
