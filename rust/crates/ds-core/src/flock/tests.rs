//! Ported from tests/test_dunk_lane_auto_declare.py (InferenceTests, CorruptRowTests) plus the
//! launch-memory and caller-state behaviour of dunk_core.Flock. The Python log assertion in
//! `test_a_corrupt_declared_row_fails_closed` is NOT ported: the Rust port logs with
//! `eprintln!` and there is no capture hook.

use super::*;
use serde_json::json;

const NUDGE_TITLE: &str = "OC | OpenCode inbox message from %15";

fn flock(dir: &tempfile::TempDir, persist: bool) -> Flock {
    Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        persist,
    )
}

#[test]
fn test_the_exact_title_from_the_incident_is_not_a_name() {
    assert!(!Flock::plain_lane_name(NUDGE_TITLE));
}

#[test]
fn test_notice_text_and_pipes_are_never_names() {
    for title in [
        NUDGE_TITLE,
        "[inbox] new message",
        "a | b",
        "read it with: x",
    ] {
        assert!(!Flock::plain_lane_name(title), "{title}");
    }
}

#[test]
fn test_a_plain_lane_name_is_still_accepted() {
    for name in ["opencode-ds41-b", "dunky_pi.2", "%28", "w8:p8"] {
        assert!(Flock::plain_lane_name(name), "{name}");
    }
}

#[test]
fn test_declared_names_may_contain_spaces_but_not_notice_text() {
    for name in ["dunky pi", "PM (par-a)", "claude poolrad"] {
        assert!(Flock::usable_declared_name(name), "{name}");
    }
    for name in [NUDGE_TITLE, "[inbox] new message"] {
        assert!(!Flock::usable_declared_name(name), "{name}");
    }
}

#[test]
fn test_a_corrupt_declared_row_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, false);
    let mut data = Map::new();
    data.insert(
        "w1:p1".into(),
        json!({"mailbox": NUDGE_TITLE, "declared": true, "alive": true}),
    );
    assert!(flock.write_lane_registry(&data));
    let registry = flock.lane_registry(false).unwrap();
    let (name, declared) = Flock::row_identity(registry.get("w1:p1"));
    assert_eq!(name, None);
    assert!(!declared);
}

#[test]
fn test_remember_lane_declared_beats_inferred() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, false);
    assert!(flock.remember_lane("%1", "guess", false));
    assert!(!flock.remember_lane("%1", "other-guess", false));
    assert!(flock.remember_lane("%1", "PM (par-a)", true));
    assert!(!flock.remember_lane("%1", "claude title", false));
    let registry = flock.lane_registry(false).unwrap();
    let (name, declared) = Flock::row_identity(registry.get("%1"));
    assert_eq!(name.as_deref(), Some("PM (par-a)"));
    assert!(declared);
    assert_eq!(registry["%1"]["alive"], json!(true));
}

#[test]
fn test_lane_registry_strict_refuses_a_non_object() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, false);
    std::fs::write(&flock.lane_registry_path, "[1]").unwrap();
    assert!(flock.lane_registry(true).is_err());
    assert!(flock.lane_registry(false).unwrap().is_empty());
}

#[test]
fn test_record_launch_merges_non_null_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, true);
    flock.record_launch("%3", &[("agent", json!("claude")), ("cwd", Value::Null)]);
    flock.record_launch("%3", &[("session", json!("s1"))]);
    let entry = flock.launch("%3").unwrap();
    assert_eq!(entry["agent"], json!("claude"));
    assert_eq!(entry["session"], json!("s1"));
    assert!(!entry.contains_key("cwd"));
    assert!(entry.contains_key("recorded_at"));
    let again = Flock::new(&flock.launches_file, &flock.lane_registry_path, true);
    assert_eq!(again.launch("%3").unwrap()["session"], json!("s1"));
    let text = std::fs::read_to_string(&flock.launches_file).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap()["version"],
        json!(1)
    );
}

#[test]
fn test_record_launch_does_nothing_when_not_persisting_or_without_a_pane() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, false);
    flock.record_launch("%3", &[("agent", json!("claude"))]);
    assert!(!flock.launches_file.exists());
    flock.record_launch("", &[("agent", json!("x"))]);
    assert!(flock.launch("").is_none());
}

#[test]
fn test_snapshot_launches_records_agent_sessions_once() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, true);
    let panes = vec![
        json!({"pane_id": "%1", "agent": "claude", "cwd": "/w",
               "agent_session": {"value": "abc", "kind": "uuid"}}),
        json!({"pane_id": "%2", "agent": "claude"}),
    ];
    flock.snapshot_launches(&panes);
    let entry = flock.launch("%1").unwrap();
    assert_eq!(entry["session"], json!("abc"));
    assert_eq!(entry["source"], json!("herdr"));
    assert!(flock.launch("%2").is_none());
    let before = std::fs::metadata(&flock.launches_file)
        .unwrap()
        .modified()
        .unwrap();
    flock.snapshot_launches(&panes);
    let after = std::fs::metadata(&flock.launches_file)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn test_a_corrupt_launches_file_is_an_empty_memory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("launches.json"), "{nope").unwrap();
    let flock = flock(&dir, true);
    assert!(flock.launch("%1").is_none());
}

#[test]
fn test_caller_state_set_current_clear_and_socket_peer() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(&dir, false);
    assert_eq!(flock.current_caller(), None);
    flock.set_caller(Some(json!({"pane": "%9"})));
    assert_eq!(flock.current_caller(), Some(json!({"pane": "%9"})));
    flock.set_socket_peer(Some(false));
    flock.clear_caller();
    assert_eq!(flock.current_caller(), None);
    assert_eq!(flock.socket_peer_ok(), Some(false));
    flock.clear_socket_peer();
    assert_eq!(flock.socket_peer_ok(), None);
}
