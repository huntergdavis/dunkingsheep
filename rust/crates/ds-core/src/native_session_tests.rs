//! Ported from `tests/test_dunk_native_session.py` (t416): the fail-closed native-session
//! guard, with the saved %30 trace controls.

use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::symlink;

use super::*;

const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";
const OLD: &str = "01a18644-66a3-7772-b01f-9b0f1d25e7fd";
const NEW: &str = "01a18645-98e0-7cb0-b14a-bbc93298907e";
const CWD: &str = "/home/hunter/workspace/poolrad-codex-review-c";
const TIMELINE: &str = include_str!("../../../../tests/fixtures/t416-native-turn-timeline.json");

struct Fixture {
    home: PathBuf,
    rollout: PathBuf,
}

fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("ds-native-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    // Resolve symlinks once, as the guard does (/tmp may be one).
    let home = PathBuf::from(realpath(home.to_str().unwrap()));
    let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
    let f = Fixture { home, rollout };
    f.write_rollout(&[], true);
    let db = Connection::open(f.home.join("state_5.sqlite")).unwrap();
    db.execute(
        "create table threads (id text primary key, rollout_path text, cwd text)",
        [],
    )
    .unwrap();
    db.execute(
        "insert into threads values (?,?,?)",
        (SESSION, f.rollout.to_str().unwrap(), CWD),
    )
    .unwrap();
    f
}

impl Fixture {
    fn write_rollout(&self, events: &[(&str, &str, Option<&str>)], authenticated: bool) {
        let mut text = String::new();
        if authenticated {
            text += &json!({"timestamp": "2026-10-29T06:06:30Z", "type": "session_meta", "payload": {"session_id": SESSION, "id": SESSION, "cwd": CWD}}).to_string();
            text += "\n";
        }
        for (kind, turn, stamp) in events {
            text +=
                &json!({"timestamp": stamp.unwrap_or("2026-10-29T06:06:32Z"), "type": "event_msg",
                "payload": {"type": kind, "turn_id": turn, "thread_id": SESSION}})
                .to_string();
            text += "\n";
        }
        std::fs::write(&self.rollout, text).unwrap();
    }

    fn argv(&self, home: Option<&str>, session: &str, cwd: &str) -> Vec<String> {
        let home = home
            .map(String::from)
            .unwrap_or_else(|| self.home.display().to_string());
        [
            "env".to_string(),
            format!("CODEX_HOME={home}"),
            "codex".into(),
            "--no-daemon".into(),
            "-C".into(),
            cwd.into(),
            "resume".into(),
            session.into(),
            "prompt".into(),
        ]
        .to_vec()
    }

    fn check(&self) -> Result<Option<NativeSessionSnapshot>, NativeSessionError> {
        check_native_session_quiescence(&self.argv(None, SESSION, CWD), Some(CWD))
    }

    fn refusal(&self) -> String {
        self.check().unwrap_err().to_string()
    }
}

#[test]
fn saved_trace_cut_at_060744_refuses_and_names_old_turn() {
    // The real %30 trace: old started 06:06:32, replacement at 06:07:44, old did not complete
    // until 06:09:53.
    let f = fixture("cut");
    f.write_rollout(
        &[("task_started", OLD, Some("2026-10-29T06:06:32.125Z"))],
        true,
    );
    let message = f.refusal();
    assert!(message.contains(OLD), "{message}");
    assert!(
        message.contains("(started 2026-10-29T06:06:32.125Z)"),
        "{message}"
    );
}

#[test]
fn older_turn_completed_before_respawn_allows() {
    let f = fixture("completed");
    f.write_rollout(
        &[
            ("task_started", OLD, Some("2026-10-29T06:06:32.125Z")),
            ("task_complete", OLD, Some("2026-10-29T06:07:40.000Z")),
        ],
        true,
    );
    let snapshot = f.check().unwrap().unwrap();
    assert!(snapshot.active_turns.is_empty());
    assert_eq!(snapshot.started_turns, [OLD]);
    assert_eq!(snapshot.completed_turns, [OLD]);
}

#[test]
fn authenticated_quiet_history_allows() {
    let f = fixture("quiet");
    f.write_rollout(&[], true);
    let snapshot = f.check().unwrap().unwrap();
    assert!(snapshot.active_turns.is_empty() && snapshot.started_turns.is_empty());
}

#[test]
fn empty_history_is_not_quiet() {
    let f = fixture("empty");
    f.write_rollout(&[], false);
    assert!(f.refusal().contains("session_meta"));
}

#[test]
fn wrong_session_meta_refuses() {
    let f = fixture("wrong-meta");
    let line = json!({"type": "session_meta", "payload": {"session_id": "another-session", "id": "another-session", "cwd": CWD}});
    std::fs::write(&f.rollout, format!("{line}\n")).unwrap();
    let message = f.refusal();
    assert!(message.contains("session_meta names"), "{message}");
    assert!(message.contains("'another-session'"), "{message}");
}

#[test]
fn new_turn_after_completed_old_turn_is_active_and_refuses() {
    let f = fixture("new-turn");
    f.write_rollout(
        &[
            ("task_started", OLD, Some("2026-10-29T06:06:32Z")),
            ("task_complete", OLD, Some("2026-10-29T06:09:53Z")),
            ("task_started", NEW, Some("2026-10-29T06:10:43Z")),
        ],
        true,
    );
    let message = f.refusal();
    assert!(message.contains(NEW) && !message.contains(OLD), "{message}");
}

#[test]
fn requires_explicit_isolated_codex_home() {
    let f = fixture("home");
    let no_home = ["env", "CODEX_HOME=", "codex", "resume", SESSION].map(String::from);
    assert!(check_native_session_quiescence(&no_home, Some(CWD))
        .unwrap_err()
        .to_string()
        .contains("CODEX_HOME"));
    let shared = f.argv(Some("~/.codex"), SESSION, CWD);
    assert!(check_native_session_quiescence(&shared, Some(CWD))
        .unwrap_err()
        .to_string()
        .contains("shared"));
}

#[test]
fn missing_session_row_refuses() {
    let f = fixture("row");
    let argv = f.argv(None, "01a00000-0000-0000-0000-000000000000", CWD);
    assert!(check_native_session_quiescence(&argv, Some(CWD))
        .unwrap_err()
        .to_string()
        .contains("not in the private state"));
}

#[test]
fn cwd_mismatch_refuses() {
    let f = fixture("cwd");
    let argv = f.argv(None, SESSION, "/tmp/other");
    assert!(check_native_session_quiescence(&argv, Some(CWD))
        .unwrap_err()
        .to_string()
        .contains("cwd mismatch"));
}

#[test]
fn missing_rollout_refuses() {
    let f = fixture("missing");
    std::fs::remove_file(&f.rollout).unwrap();
    assert!(f.refusal().contains("rollout does not exist"));
}

#[test]
fn completion_without_start_refuses() {
    let f = fixture("no-start");
    f.write_rollout(
        &[("task_complete", OLD, Some("2026-10-29T06:07:00Z"))],
        true,
    );
    assert!(f.refusal().contains("has no start"));
}

#[test]
fn duplicate_start_refuses() {
    let f = fixture("dup-start");
    f.write_rollout(
        &[
            ("task_started", OLD, Some("2026-10-29T06:06:32Z")),
            ("task_started", OLD, Some("2026-10-29T06:06:33Z")),
        ],
        true,
    );
    assert!(f.refusal().contains("duplicate task_started"));
}

#[test]
fn duplicate_completion_refuses() {
    let f = fixture("dup-complete");
    f.write_rollout(
        &[
            ("task_started", OLD, None),
            ("task_complete", OLD, None),
            ("task_complete", OLD, None),
        ],
        true,
    );
    assert!(f.refusal().contains("duplicate task_complete"));
}

#[test]
fn malformed_rollout_refuses() {
    let f = fixture("malformed");
    std::fs::write(
        &f.rollout,
        "{\"type\": \"event_msg\", \"payload\": {broken\n",
    )
    .unwrap();
    assert!(f.refusal().contains("malformed"));
}

#[test]
fn thread_id_mismatch_refuses() {
    let f = fixture("thread");
    let meta = json!({"type": "session_meta", "payload": {"session_id": SESSION, "id": SESSION, "cwd": CWD}});
    let event = json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": OLD, "thread_id": "different-thread"}});
    std::fs::write(&f.rollout, format!("{meta}\n{event}\n")).unwrap();
    assert!(f.refusal().contains("names thread"));
}

#[test]
fn partial_line_refuses() {
    let f = fixture("partial");
    std::fs::write(&f.rollout, "{\"type\": \"event_msg\"").unwrap();
    assert!(f.refusal().contains("partial line"));
}

#[test]
fn snapshot_refuses_if_rollout_changes() {
    let f = fixture("changes");
    f.write_rollout(&[], true);
    let snapshot = f.check().unwrap().unwrap();
    snapshot.assert_unchanged().unwrap();
    let line = json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": NEW, "thread_id": SESSION}});
    let mut text = std::fs::read_to_string(&f.rollout).unwrap();
    text += &format!("{line}\n");
    std::fs::write(&f.rollout, text).unwrap();
    let error = snapshot.assert_unchanged().unwrap_err();
    assert!(matches!(error, NativeSessionError::Changed(_)), "{error}");
    std::fs::remove_file(&f.rollout).unwrap();
    assert!(snapshot
        .assert_unchanged()
        .unwrap_err()
        .to_string()
        .contains("disappeared"));
}

#[test]
fn saved_real_timeline_cut_before_old_completion_refuses() {
    let f = fixture("timeline");
    let timeline: Vec<serde_json::Value> = serde_json::from_str(TIMELINE).unwrap();
    let mut text = std::fs::read_to_string(&f.rollout).unwrap();
    for entry in timeline {
        if entry["timestamp"].as_str().unwrap() <= "2026-10-29T06:07:44" {
            // Reconstruct only the lifecycle records; no private text.
            let payload =
                json!({"type": entry["type"], "turn_id": entry["turn_id"], "thread_id": SESSION});
            text +=
                &json!({"timestamp": entry["timestamp"], "type": "event_msg", "payload": payload})
                    .to_string();
            text += "\n";
        }
    }
    std::fs::write(&f.rollout, text).unwrap();
    assert!(f.refusal().contains("01a18644"));
}

#[test]
fn resume_request_parser_ignores_prompt_and_env() {
    let f = fixture("parser");
    let request = find_resume_request(&f.argv(None, SESSION, CWD)).unwrap();
    assert_eq!(request.session_id, SESSION);
    assert_eq!(
        request.codex_home.as_deref(),
        Some(f.home.to_str().unwrap())
    );
    assert_eq!(request.cwd.as_deref(), Some(CWD));
    assert!(find_resume_request(&["codex", "exec", "hi"].map(String::from)).is_none());
    // a resume with no id is a picker, not a same-session request
    let picker = find_resume_request(&["codex", "resume", "--last"].map(String::from)).unwrap();
    assert_eq!(picker.session_id, "");
    assert!(
        check_native_session_quiescence(&["codex", "resume"].map(String::from), None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_busy_state_db_is_retryable_with_a_fixed_message_and_the_rest_are_refusals() {
    assert!(NativeSessionError::Busy.retryable());
    assert_eq!(
        NativeSessionError::Busy.to_string(),
        "native session state DB busy; retry the request"
    );
    assert!(!NativeSessionError::Refusal("x".into()).retryable());
    assert!(matches!(
        DunkError::from(NativeSessionError::Busy),
        DunkError::Retryable(_)
    ));
    assert!(matches!(
        DunkError::from(NativeSessionError::Changed("c".into())),
        DunkError::Refused(_)
    ));
}

#[test]
fn bytes_splitlines_splits_on_the_three_newlines_only() {
    let lines = split_lines(b"a\nb\r\nc\rd\x0be");
    assert_eq!(lines, [&b"a"[..], b"b", b"c", b"d\x0be"]);
    assert!(split_lines(b"").is_empty());
}

#[cfg(unix)]
#[test]
fn realpath_resolves_symlink_prefixes_with_missing_suffixes_and_parent_traversal() {
    let root = std::env::temp_dir().join(format!("ds-realpath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let target = root.join("target");
    std::fs::create_dir_all(&target).unwrap();
    let existing = target.join("existing");
    std::fs::write(&existing, "fixture").unwrap();
    let alias = root.join("alias");
    symlink(&target, &alias).unwrap();

    let expected_missing = target.join("missing").join("leaf");
    let through_alias = alias.join("missing").join("leaf");
    assert_eq!(
        realpath(through_alias.to_str().unwrap()),
        expected_missing.display().to_string()
    );

    let through_missing_parent = root
        .join("missing")
        .join("..")
        .join("alias")
        .join("missing");
    let expected_parent_result = target.join("missing");
    assert_eq!(
        realpath(through_missing_parent.to_str().unwrap()),
        expected_parent_result.display().to_string()
    );

    assert_eq!(
        realpath(existing.to_str().unwrap()),
        std::fs::canonicalize(&existing)
            .unwrap()
            .display()
            .to_string()
    );
    assert_eq!(
        realpath("."),
        std::env::current_dir()
            .unwrap()
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );

    std::fs::remove_dir_all(root).unwrap();
}
