use super::*;

const SESSION: &str = "11111111-2222-4333-8444-555555555555";
const OTHER: &str = "66666666-7777-4888-9999-aaaaaaaaaaaa";

#[test]
fn explicit_resume_is_durable_without_backend_session_metadata() {
    let dir = crate::testsupport::dir("launch-session-registration");
    let path = dir.path().join("launches.json");
    let flock = Flock::new(&path, dir.path().join("lanes.json"), true);
    let argv = json!([
        "claude",
        "--model",
        "claude-opus-5-5",
        "--effort",
        "medium",
        "--dangerously-skip-permissions",
        "--resume",
        SESSION
    ]);
    flock.record_launch(
        "%303",
        &[
            ("agent", json!("coordinator")),
            ("argv", argv.clone()),
            ("cwd", json!("/work/project")),
            ("source", json!("respawn_pane")),
            ("previous", json!({"argv":["claude"]})),
        ],
    );
    let reloaded = Flock::new(&path, dir.path().join("lanes.json"), true);
    let row = reloaded.launch("%303").unwrap();
    assert_eq!(row["session"], SESSION);
    assert_eq!(row["session_kind"], "uuid");
    assert_eq!(row["session_source"], "explicit Claude resume argv");
    assert_eq!(row["argv"], argv);
    assert_eq!(row["agent"], "coordinator");
    assert_eq!(row["cwd"], "/work/project");
    assert_eq!(row["previous"]["argv"], json!(["claude"]));
    assert!(reloaded.launch("%304").is_none());
}

#[test]
fn same_session_relaunch_keeps_metadata_other_or_unknown_session_clears_it() {
    let dir = crate::testsupport::dir("launch-session-identity");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let old = json!(["claude", "--resume", SESSION]);
    flock.record_launch(
        "%1",
        &[
            ("argv", old.clone()),
            ("session", json!(SESSION)),
            ("session_id", json!(SESSION)),
            ("session_kind", json!("backend-kind")),
            ("session_source", json!("backend")),
        ],
    );
    flock.record_launch("%1", &[("argv", old), ("session", Value::Null)]);
    let row = flock.launch("%1").unwrap();
    assert_eq!(row["session"], SESSION);
    assert_eq!(row["session_id"], SESSION);
    assert_eq!(row["session_kind"], "backend-kind");
    assert_eq!(row["session_source"], "backend");

    flock.record_launch("%1", &[("argv", json!(["claude", "--resume", OTHER]))]);
    let row = flock.launch("%1").unwrap();
    assert_eq!(row["session"], OTHER);
    assert_eq!(row["session_kind"], "uuid");
    assert_eq!(row["session_source"], "explicit Claude resume argv");
    assert!(!row.contains_key("session_id"));

    flock.record_launch("%1", &[("argv", json!(["claude"]))]);
    let row = flock.launch("%1").unwrap();
    for key in SESSION_IDENTITY_KEYS {
        assert!(!row.contains_key(key), "stale {key} retained");
    }
}

#[test]
fn backend_session_metadata_wins_and_ordinary_updates_never_backfill_legacy_rows() {
    let dir = crate::testsupport::dir("launch-session-backend");
    let path = dir.path().join("launches.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"version":1,"panes":{
            "%1":{"argv":["claude","--resume",SESSION],"agent":"coordinator"}
        }}))
        .unwrap(),
    )
    .unwrap();
    let flock = Flock::new(&path, dir.path().join("lanes.json"), true);
    assert!(!flock.launch("%1").unwrap().contains_key("session"));
    flock.record_launch("%1", &[("cwd", json!("/work"))]);
    assert!(!flock.launch("%1").unwrap().contains_key("session"));
    flock.record_launch(
        "%2",
        &[
            ("argv", json!(["claude", "--resume", SESSION])),
            ("session", json!(OTHER)),
            ("session_kind", json!("backend")),
        ],
    );
    let row = flock.launch("%2").unwrap();
    assert_eq!(row["session"], OTHER);
    assert_eq!(row["session_kind"], "backend");
    assert!(!row.contains_key("session_source"));
    assert!(!flock.launch("%1").unwrap().contains_key("session"));
}

#[test]
fn non_claude_registration_keeps_its_existing_backend_contract() {
    let dir = crate::testsupport::dir("launch-session-other-provider");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    flock.record_launch(
        "%1",
        &[
            ("session", json!(SESSION)),
            ("session_kind", json!("codex")),
        ],
    );
    flock.record_launch("%1", &[("argv", json!(["codex", "resume", SESSION]))]);
    let row = flock.launch("%1").unwrap();
    assert_eq!(row["session"], SESSION);
    assert_eq!(row["session_kind"], "codex");
}
