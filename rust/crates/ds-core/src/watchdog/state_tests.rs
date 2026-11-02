//! State fixtures stay under tempfile and exercise the Python watchdog's named state cases.

use std::fs;

use serde_json::json;

use super::{empty_state, load_state, save_state};

#[test]
fn missing_state_suppresses_owner_pages_and_tells_vp_naming_the_state_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watchdog-state.json");
    let (state, problem) = load_state(&path);

    assert_eq!(empty_state(), state);
    let problem = problem.unwrap();
    assert!(problem.contains(path.to_str().unwrap()));
    assert!(problem.contains("missing"));
}

#[test]
fn corrupt_state_suppresses_owner_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watchdog-state.json");
    fs::write(&path, "{ truncated").unwrap();

    let (state, problem) = load_state(&path);
    assert_eq!(empty_state(), state);
    assert!(problem.unwrap().contains("corrupt"));
}

#[test]
fn state_round_trip_preserves_payload_and_fills_the_empty_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/watchdog-state.json");
    let state = json!({
        "alerts": {"disk|/": {"last_sent": 42}},
        "owner_pages": {"disk|/": {"brief_id": 17}},
        "custom_future_key": ["kept"],
    });

    save_state(&path, &state).unwrap();
    let (loaded, problem) = load_state(&path);

    assert_eq!(None, problem);
    assert_eq!(state["alerts"], loaded["alerts"]);
    assert_eq!(state["owner_pages"], loaded["owner_pages"]);
    assert_eq!(json!(["kept"]), loaded["custom_future_key"]);
    assert_eq!(json!({}), loaded["streaks"]);
    assert_eq!(json!([]), loaded["owner_page_times"]);
    assert_eq!(0o600, crate::atomic_json::mode(&path).unwrap());
}

#[test]
fn non_object_state_is_corrupt_and_returns_the_empty_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watchdog-state.json");
    fs::write(&path, "[]").unwrap();

    let (state, problem) = load_state(&path);
    assert_eq!(empty_state(), state);
    assert!(problem.unwrap().contains("corrupt (not an object)"));
}
