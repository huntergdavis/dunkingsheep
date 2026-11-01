//! Ported from tests/test_dunk_state_session.py `SessionReasonColumn` (the one test that needs
//! only the store). NOT in this slice: the session/proc/capture tests of test_dunk_state_session.py,
//! test_dunk_state_capture_cost.py (`session_of`, the /proc binding races, `capture`) and
//! test_dunk_styled_state.py (the styled-state table, a different module).

use super::*;

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn test_old_database_gains_the_column_without_losing_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    {
        let old = Connection::open(&path).expect("open");
        old.execute_batch(
            "CREATE TABLE agent_state (
                at REAL NOT NULL, pane_id TEXT NOT NULL, project TEXT, harness TEXT,
                cwd TEXT, argv TEXT, session_id TEXT, model TEXT, model_reason TEXT,
                thinking TEXT, thinking_reason TEXT, state TEXT, state_source TEXT,
                state_reason TEXT, task_id TEXT, source TEXT);
            INSERT INTO agent_state (at, pane_id, session_id) VALUES (1.0, '%9', NULL);",
        )
        .expect("old schema");
    }
    let store = StateStore::open(&path).expect("store");
    let columns: Vec<String> = store.with_connection(|c| {
        let mut statement = c.prepare("PRAGMA table_info(agent_state)").unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(1)).unwrap();
        rows.map(Result::unwrap).collect()
    });
    assert!(columns.iter().any(|c| c == "session_reason"));
    let panes: Vec<String> = store.with_connection(|c| {
        let mut statement = c.prepare("SELECT pane_id FROM agent_state").unwrap();
        let rows = statement.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(Result::unwrap).collect()
    });
    assert_eq!(panes, ["%9"]);
}

// -- beyond the Python tests ----------------------------------------------------------

#[test]
fn test_add_and_latest_round_trip_newest_first_with_decoded_argv() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("sub").join("agent-state.db")).expect("store");
    let row = json!({"pane_id": "%1", "project": "dunkingsheep", "harness": "pi", "cwd": "/home/x/dunkingsheep",
        "argv": ["pi", "--model", "m1"], "model": "m1", "state": "idle", "source": "periodic"});
    assert_eq!(store.add(std::slice::from_ref(&row), 10.0).unwrap(), 1);
    assert_eq!(
        store
            .add(&[json!({"pane_id": "%2", "project": "other"})], 20.0)
            .unwrap(),
        1
    );
    let all = store.latest(None, None, 200).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["pane_id"], "%2", "newest first");
    assert_eq!(all[0]["argv"], Value::Null, "no argv stored when absent");
    let mine = store.latest(Some("%1"), None, 200).unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["argv"], json!(["pi", "--model", "m1"]));
    assert_eq!(mine[0]["at"], json!(10.0));
    assert_eq!(store.latest(None, Some("other"), 200).unwrap().len(), 1);
    assert_eq!(store.latest(None, None, 1).unwrap().len(), 1);
}

#[test]
fn test_a_row_without_a_pane_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = StateStore::open(dir.path().join("s.db")).expect("store");
    assert!(store
        .add(&[json!({"pane_id": "%1"}), json!({"project": "x"})], 1.0)
        .is_err());
    assert!(
        store.latest(None, None, 10).unwrap().is_empty(),
        "the batch is one transaction"
    );
}

#[test]
fn test_project_of() {
    assert_eq!(project_of(None), None);
    assert_eq!(project_of(Some("")), None);
    assert_eq!(project_of(Some("/")), None);
    assert_eq!(
        project_of(Some("/home/hunter/workspace/dunkingsheep")).as_deref(),
        Some("dunkingsheep")
    );
    assert_eq!(project_of(Some("/home/hunter")).as_deref(), Some("hunter"));
    assert_eq!(project_of(Some("/tmp/x/")).as_deref(), Some("x"));
}

#[test]
fn test_flag_reads_the_next_token_or_the_equals_form() {
    assert_eq!(
        flag(&argv(&["pi", "--model", "m1"]), &MODEL_FLAGS).as_deref(),
        Some("m1")
    );
    assert_eq!(
        flag(&argv(&["pi", "-m", "m2"]), &MODEL_FLAGS).as_deref(),
        Some("m2")
    );
    assert_eq!(
        flag(&argv(&["pi", "--effort=high"]), &THINKING_FLAGS).as_deref(),
        Some("high")
    );
    assert_eq!(
        flag(&argv(&["pi", "--model"]), &MODEL_FLAGS),
        None,
        "a trailing flag has no value"
    );
    assert_eq!(flag(&argv(&["pi"]), &MODEL_FLAGS), None);
    assert_eq!(flag(&[], &MODEL_FLAGS), None);
}
