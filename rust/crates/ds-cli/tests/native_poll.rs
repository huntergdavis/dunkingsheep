//! Socket-level coverage of the release-native poll client; no real daemon or credentials.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    thread,
};

#[test]
fn poll_sends_one_batched_read_with_explicit_options() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(
            stream,
            "{}",
            json!({"ok":true,"result":{"status":{"version":"test"},"inbox_count":{"unread":2}}})
        )
        .unwrap();
        request
    });
    let output = Command::new(env!("CARGO_BIN_EXE_dunk-poll"))
        .args([
            "--socket",
            path.to_str().unwrap(),
            "--self-pane",
            "%77",
            "--mailbox",
            "desk",
            "--limit",
            "3",
            "--tasks",
            "--panes",
            "--task-scope",
            "self",
            "--include-done",
            "--check-herdr",
            "--compact",
        ])
        .env_remove("DUNKINGSHEEP_SOCKET")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["inbox_count"]["unread"], 2);
    assert_eq!(
        server.join().unwrap(),
        json!({"id":1,"cmd":"poll","self_pane_id":"%77","args":{"mailbox":"desk","limit":3,"panes":true,"tasks":true,"task_scope":"self","include_done":true,"check_herdr":true}})
    );
}

// ---- t4696: the default answer is compact; --all and --full opt in ----

/// One fake daemon reply for one `dunk-poll` run. Returns the request it received and stdout.
fn poll_with(reply: Value, extra: &[&str]) -> (Value, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("socket");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        writeln!(stream, "{}", json!({"ok": true, "result": reply})).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    });
    let mut args = vec!["--socket", path.to_str().unwrap(), "--self-pane", "%77"];
    args.extend_from_slice(extra);
    let output = Command::new(env!("CARGO_BIN_EXE_dunk-poll"))
        .args(&args)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        server.join().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

/// A fleet of 24 dunks with long texts, two of them aimed at the caller's pane, and tasks with
/// long bodies: the shape of the live answer that was 48 KB (219 KB with tasks).
fn fleet_reply() -> Value {
    let text =
        "OWNER COORDINATOR WATCH (every 15 minutes): inspect every active project coordinator. "
            .repeat(12);
    let dunks: Vec<Value> = (0..24)
        .map(|i| {
            json!({
                "id": format!("d{i}"), "name": if i == 1 { String::new() } else { format!("watch-{i}") },
                "target_pane_id": if i < 2 { "%77".to_string() } else { format!("%{}", 100 + i) },
                "target_label": format!("lane-{i}"), "target_tab": "tab", "target_workspace": "w",
                "target_cwd": "/home/hunter/workspace", "target_agent": "",
                "interval_minutes": 15.0, "running": i % 2 == 0, "status": "Sent 20:15:33",
                "next_send_at": 1794620000.5, "next_send_in_s": 120, "last_error": null,
                "text": text, "last_sent_text": text, "send_count": 57, "hold_count": 3,
                "skip_count": 0, "total_skips": 1, "unknown_skips": 0, "created_at": 1.0,
                "started_at": 2.0, "last_sent_at": 3.0, "last_held_at": null, "max_sends": null,
                "only_when": "idle", "hold_while_typing": true, "skip_if_unconsumed": true,
                "awaiting_consumption": true, "last_skipped_at": null
            })
        })
        .collect();
    let tasks: Vec<Value> = (0..40)
        .map(|i| {
            json!({
                "id": format!("t{i}"), "status": "open", "priority": "high", "scope": "global",
                "assignees": ["%77"], "title": "A task with a long title ".repeat(8),
                "body": "Owner ask, acceptance evidence and current state. ".repeat(40),
                "created_at": 1.0, "updated_at": 2.0, "leases": [], "depends_on": [], "kind": "work"
            })
        })
        .collect();
    json!({
        "status": {"self_pane_id": "%77", "version": "test", "running": 12, "dunkers": 24},
        "inbox_count": {"unread": 1, "total": 9, "mailbox": "writer"},
        "dunks": {"dunks": dunks, "mailbox": "writer",
                  "messages": [{"message_id": "i9", "sender": "%303", "subject": "Next item"}]},
        "tasks": {"pane_id": "%77", "scope": "visible", "tasks": tasks}
    })
}

#[test]
fn the_default_answer_is_the_callers_dunks_without_text_and_under_two_kilobytes() {
    let mut reply = fleet_reply();
    reply.as_object_mut().unwrap().remove("tasks");
    let (request, stdout) = poll_with(reply.clone(), &["--compact"]);
    // Read-only, and the request is what it always was: shaping happens after the read.
    assert_eq!(request["cmd"], "poll");
    assert_eq!(request["args"]["tasks"], false);

    let out: Value = serde_json::from_str(&stdout).unwrap();
    let dunks = out["dunks"]["dunks"].as_array().unwrap();
    assert_eq!(
        vec!["d0", "d1"],
        dunks
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        "only the dunks aimed at this pane"
    );
    for dunk in dunks {
        assert!(
            dunk.get("text").is_none() && dunk.get("last_sent_text").is_none(),
            "{dunk}"
        );
        for field in [
            "id",
            "name",
            "target_pane_id",
            "target_label",
            "interval_minutes",
            "running",
            "status",
            "next_send_at",
            "last_error",
        ] {
            assert!(dunk.get(field).is_some(), "missing {field}: {dunk}");
        }
    }
    // A dunk is still recognisable: its name, or the start of its text when it has none.
    assert_eq!(dunks[0]["label"], "watch-0");
    let unnamed = dunks[1]["label"].as_str().unwrap();
    assert!(
        unnamed.starts_with("OWNER COORDINATOR WATCH (every 15 minutes)"),
        "{unnamed}"
    );
    assert_eq!(61, unnamed.chars().count(), "60 characters and an ellipsis");
    assert_eq!(out["dunks"]["total"], 24);
    assert_eq!(out["dunks"]["scope"], "self");
    assert!(out["dunks"]["note"]
        .as_str()
        .unwrap()
        .starts_with("2 of 24 fleet dunks"));
    // Status, the inbox count and the unread headers are untouched.
    assert_eq!(out["status"], reply["status"]);
    assert_eq!(out["inbox_count"], reply["inbox_count"]);
    assert_eq!(out["dunks"]["messages"], reply["dunks"]["messages"]);

    assert!(
        stdout.len() < 2048,
        "{} bytes for a pane with 2 dunks",
        stdout.len()
    );
    let (_, pretty) = poll_with(reply.clone(), &[]);
    assert!(pretty.len() < 2048, "{} bytes pretty-printed", pretty.len());
    let (_, full) = poll_with(reply, &["--full", "--compact"]);
    assert!(
        full.len() > 40_000,
        "the fixture is as large as the live answer: {}",
        full.len()
    );
}

#[test]
fn all_lists_the_fleet_without_text_and_full_returns_the_daemons_answer_unchanged() {
    let mut reply = fleet_reply();
    reply.as_object_mut().unwrap().remove("tasks");
    let (_, stdout) = poll_with(reply.clone(), &["--all", "--compact"]);
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let dunks = out["dunks"]["dunks"].as_array().unwrap();
    assert_eq!(24, dunks.len());
    assert!(dunks.iter().all(|d| d.get("text").is_none()));
    assert_eq!(out["dunks"]["scope"], "all");
    assert!(out["dunks"].get("note").is_none());

    let (_, stdout) = poll_with(reply.clone(), &["--full", "--compact"]);
    let mut expected = reply;
    expected.sort_all_objects();
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), expected);
}

#[test]
fn tasks_are_compact_rows_by_default_and_complete_with_full() {
    let (request, stdout) = poll_with(fleet_reply(), &["--tasks", "--compact"]);
    assert_eq!(request["args"]["tasks"], true);
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let tasks = out["tasks"]["tasks"].as_array().unwrap();
    assert_eq!(40, tasks.len(), "the same tasks, smaller rows");
    for task in tasks {
        let mut keys: Vec<&str> = task
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            vec!["assignees", "id", "priority", "scope", "status", "title"],
            keys
        );
        let title = task["title"].as_str().unwrap();
        assert!(
            title.chars().count() <= 101 && title.ends_with('\u{2026}'),
            "{title}"
        );
    }
    assert!(stdout.len() < 12_000, "{} bytes for 40 tasks", stdout.len());

    let (_, full) = poll_with(fleet_reply(), &["--tasks", "--full", "--compact"]);
    let full: Value = serde_json::from_str(&full).unwrap();
    assert!(full["tasks"]["tasks"][0]["body"].as_str().unwrap().len() > 1000);
}

#[test]
fn text_output_says_when_the_dunk_list_is_only_this_panes() {
    let mut reply = fleet_reply();
    reply.as_object_mut().unwrap().remove("tasks");
    let (_, text) = poll_with(reply.clone(), &["--text"]);
    // The header still counts the fleet (12 of 24 running), so the list is labelled.
    assert!(text.contains("dunks 12/24 running"), "{text}");
    assert!(
        text.contains("  dunks: 2 of 24 fleet dunks target this pane; --all lists every dunk"),
        "{text}"
    );
    assert_eq!(
        2,
        text.lines()
            .filter(|line| line.contains(" every 15.0m -> "))
            .count(),
        "{text}"
    );

    let (_, all) = poll_with(reply, &["--text", "--all"]);
    assert!(!all.contains("fleet dunks target this pane"), "{all}");
    // --text prints at most ten rows, as before.
    assert_eq!(
        10,
        all.lines()
            .filter(|line| line.contains(" every 15.0m -> "))
            .count(),
        "{all}"
    );
}
