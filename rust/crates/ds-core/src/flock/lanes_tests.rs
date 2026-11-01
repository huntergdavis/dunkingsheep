//! The Pane -> JSON adapter and lane_activity/resolve_lanes, from dunk_terminal._parse_panes,
//! TerminalBackend.list_panes_grouped and dunk_core.Flock.lane_activity/resolve_lanes. Not a
//! 1:1 port: the Python tests reach these through the roster/dispatch (T38/T54).

use super::lanes::*;
use super::mailbox::MailboxEnv;
use super::Flock;
use crate::activity::Turn;
use crate::terminal::fakes::FakeTerminal;
use crate::terminal::Terminal;
use serde_json::json;

fn flock() -> crate::testsupport::Scratch<Flock> {
    // t1490: a private directory, not files in the shared temp dir that every run reused.
    let dir = crate::testsupport::dir("lanes");
    let flock = Flock::new(
        dir.path().join("lanes-launches.json"),
        dir.path().join("lanes-lanes.json"),
        false,
    );
    crate::testsupport::Scratch::new(flock, dir)
}

fn none(_: &str) -> (bool, bool, Option<String>) {
    (false, false, None)
}

#[test]
fn the_adapter_carries_the_tmux_dict_keys() {
    let term = FakeTerminal::new();
    let pane = term
        .list_panes()
        .into_iter()
        .find(|p| p.pane_id == "w2:p2")
        .unwrap();
    let value = pane_to_json(&pane, None);
    assert_eq!(value["agent"], json!("claude"));
    assert_eq!(value["terminal_id"], json!("w2:p2"));
    assert_eq!(value["tab_id"], json!("w2"));
    assert_eq!(value["workspace_id"], json!("s2"));
    assert_eq!(value["tab_label"], json!("2"));
    assert_eq!(value["workspace_label"], json!("work"));
    assert_eq!(value["agent_status"], json!("unknown"));
    assert_eq!(value["terminal_title_stripped"], value["label"]);
    assert_eq!(
        pane_to_json(&pane, Some("idle"))["agent_status"],
        json!("idle")
    );
    // A shell has no agent: the empty string, as `_agent_kind` returns.
    let shell = term
        .list_panes()
        .into_iter()
        .find(|p| p.pane_id == "w1:p1")
        .unwrap();
    assert_eq!(pane_to_json(&shell, None)["agent"], json!(""));
}

#[test]
fn the_grouped_listing_is_sorted_and_labels_fall_back_to_ids() {
    let mut term = FakeTerminal::new();
    term.panes[0].tab = String::new(); // a tab with no label
    let env = TerminalEnv {
        terminal: &term,
        registered: &none,
    };
    let grouped = env.list_panes_grouped();
    let keys: Vec<(i64, i64)> = grouped
        .iter()
        .map(|p| {
            (
                p["workspace_number"].as_i64().unwrap(),
                p["tab_number"].as_i64().unwrap(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let first = grouped
        .iter()
        .find(|p| p["pane_id"] == json!("w1:p1"))
        .unwrap();
    assert_eq!(first["tab_label"], first["tab_id"]);
}

#[test]
fn the_terminal_env_drives_resolve_mailbox_end_to_end() {
    let term = FakeTerminal::new();
    let env = TerminalEnv {
        terminal: &term,
        registered: &none,
    };
    let f = flock();
    let (key, label, pane) = f.resolve_mailbox(&env, "w2:p2", None, true, "x").unwrap();
    assert_eq!(pane.as_deref(), Some("w2:p2"));
    assert!(label.ends_with("(w2:p2)") && !key.is_empty());
    assert!(f.resolve_mailbox(&env, "w9:p9", None, true, "x").is_err());
}

#[test]
fn lane_activity_prefers_the_backend_then_declared_turns() {
    let f = flock();
    let working = json!({"pane_id": "%1", "agent": "claude", "agent_status": " Working "});
    let a = f.lane_activity(&working, 100.0);
    assert_eq!(
        (
            a["state"].clone(),
            a["source"].clone(),
            a["inferred"].clone()
        ),
        (json!("working"), json!("terminal backend"), json!(false))
    );
    let no_id = f.lane_activity(&json!({"agent_status": "unknown"}), 100.0);
    assert_eq!(no_id["state"], json!(null));
    assert_eq!(
        no_id["reason"],
        json!("no activity source: pane identity missing")
    );
    // No turn ever recorded: state None (not "unknown"), source names the harness.
    let silent = json!({"pane_id": "%2", "agent": "codex", "agent_status": "unknown"});
    let a = f.lane_activity(&silent, 100.0);
    assert_eq!(a["state"], json!(null));
    assert_eq!(a["source"], json!("codex declared turns"));
    // A live turn is working; an ended one is idle.
    f.set_turn(
        "%2",
        Turn {
            started_at: Some(90.0),
            expires_at: Some(500.0),
            ended_at: None,
        },
    );
    assert_eq!(f.lane_status(&silent, 100.0), json!("working"));
    f.set_turn(
        "%2",
        Turn {
            started_at: Some(90.0),
            expires_at: Some(95.0),
            ended_at: Some(95.0),
        },
    );
    assert_eq!(f.lane_status(&silent, 100.0), json!("idle"));
}

#[test]
fn resolve_lanes_lists_every_pane_with_its_derived_status() {
    let mut term = FakeTerminal::new();
    term.statuses.insert("w2:p2".into(), "working".into());
    let env = TerminalEnv {
        terminal: &term,
        registered: &none,
    };
    let lanes = flock().resolve_lanes(&env, 100.0).unwrap();
    assert_eq!(lanes.len(), term.panes.len());
    let claude = lanes
        .iter()
        .find(|l| l["pane_id"] == json!("w2:p2"))
        .unwrap();
    assert_eq!(claude["agent_status"], json!("working"));
    assert_eq!(claude["agent_name"], json!("2")); // tab_label beats label
    assert_eq!(claude["pane_pid"], json!(2202));
    let shell = lanes
        .iter()
        .find(|l| l["pane_id"] == json!("w1:p1"))
        .unwrap();
    assert_eq!(shell["agent_status"], json!(null));
}
