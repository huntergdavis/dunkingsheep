//! Generic `--json` daemon output through the actual Rust CLI process and an isolated fake socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};

use serde_json::{json, Value};

fn ordered_result() -> Value {
    let mut nested = serde_json::Map::new();
    nested.insert("tiny".into(), json!(1e-7));
    nested.insert("label".into(), json!("café😀"));
    let mut result = serde_json::Map::new();
    result.insert("zeta".into(), Value::Object(nested));
    result.insert("alpha".into(), json!("second"));
    Value::Object(result)
}

fn run_cli(socket: &str, command: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .args(["--socket", socket, "--self-pane", "%99"])
        .args(command)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .output()
        .expect("run Rust CLI")
}

fn run_daemon_cli(command: &[&str], rpc: &str, args: Value, result: Value) -> Output {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let listener = UnixListener::bind(&socket).unwrap();
    let rpc = rpc.to_string();
    let daemon = std::thread::spawn(move || {
        for (expected_cmd, expected_args, result) in [
            ("status".to_string(), json!({}), json!({})),
            (rpc, args, result),
        ] {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(expected_cmd, request["cmd"].as_str().unwrap());
            assert_eq!(expected_args, request["args"]);
            assert_eq!("%99", request["self_pane_id"]);
            let response = json!({"ok": true, "result": result});
            let mut writer = &stream;
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
    });
    let output = run_cli(&socket_text, command);
    daemon.join().unwrap();
    output
}

#[test]
fn cli_route_02_dunk_create_read() {
    let listed = run_daemon_cli(&["ls"], "list_dunks", json!({}), json!({"dunks": []}));
    assert_eq!(0, listed.status.code().unwrap());
    assert_eq!("no dunks\n", String::from_utf8(listed.stdout).unwrap());
    assert!(listed.stderr.is_empty());

    let fetched = run_daemon_cli(
        &["get", "dunk-7", "--json"],
        "get_dunk",
        json!({"id": "dunk-7"}),
        json!({"ok": true, "id": "dunk-7", "running": true, "text": "hello"}),
    );
    assert_eq!(0, fetched.status.code().unwrap());
    assert_eq!(
        "{\n  \"ok\": true,\n  \"id\": \"dunk-7\",\n  \"running\": true,\n  \"text\": \"hello\"\n}\n",
        String::from_utf8(fetched.stdout).unwrap()
    );
    assert!(fetched.stderr.is_empty());

    let added = run_daemon_cli(
        &[
            "add",
            "-T",
            "%42",
            "-t",
            "work",
            "-e",
            "1.5h",
            "-n",
            "hourly",
            "--only-idle",
            "--max-sends",
            "3",
            "--skip-if-unconsumed",
            "--ignore-typing",
            "--no-start",
        ],
        "add_dunk",
        json!({
            "text": "work",
            "interval_minutes": "1.5h",
            "start": false,
            "target": "%42",
            "name": "hourly",
            "only_when": "idle",
            "max_sends": 3,
            "skip_if_unconsumed": true,
            "hold_while_typing": false
        }),
        json!({
            "id": "d8",
            "running": false,
            "status": "stopped",
            "target_label": "writer",
            "interval_minutes": 90,
            "only_when": "idle",
            "max_sends": 3,
            "skip_if_unconsumed": true,
            "hold_while_typing": false,
            "text": "work"
        }),
    );
    assert_eq!(0, added.status.code().unwrap());
    assert_eq!(
        "id               d8\nstatus           stopped\ntarget_label     writer\ninterval_minutes 90\nonly_when        idle\nmax_sends        3\nskip_if_unconsumed True\nhold_while_typing False\ntext             'work'\n",
        String::from_utf8(added.stdout).unwrap()
    );
    assert!(added.stderr.is_empty());
}

#[test]
fn cli_route_06_inbox_read_a() {
    let count = run_daemon_cli(
        &["inbox", "count", "reviewer"],
        "inbox_count",
        json!({"mailbox": "reviewer"}),
        json!({"mailbox": "reviewer", "unread": 2, "total": 5}),
    );
    assert_eq!(0, count.status.code().unwrap());
    assert_eq!(
        "reviewer: 2 unread of 5\n",
        String::from_utf8(count.stdout).unwrap()
    );
    assert!(count.stderr.is_empty());

    let sent = run_daemon_cli(
        &[
            "inbox",
            "send",
            "%155",
            "Please",
            "review",
            "PR",
            "#432",
            "-s",
            "Review",
            "--attach",
            "diff-a",
            "--attach",
            "diff-b",
            "--no-notify",
        ],
        "inbox_send",
        json!({
            "to": "%155",
            "body": "Please review PR #432",
            "subject": "Review",
            "attach": "diff-a,diff-b",
            "notify": false
        }),
        json!({
            "message_id": "i900",
            "destination": "%155",
            "attachments": ["diff-a", "diff-b"],
            "notified": false
        }),
    );
    assert_eq!(0, sent.status.code().unwrap());
    assert_eq!(
        "i900 -> %155 with 2 attachment(s); no nudge\n",
        String::from_utf8(sent.stdout).unwrap()
    );
    assert!(sent.stderr.is_empty());

    let body_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(body_file.path(), "file body\n").unwrap();
    let body_file_path = body_file.path().to_str().unwrap();
    let sent_file = run_daemon_cli(
        &["inbox", "send", "%155", "--body-file", body_file_path],
        "inbox_send",
        json!({
            "to": "%155",
            "body": "file body\n",
            "subject": null,
            "attach": null,
            "notify": true
        }),
        json!({
            "message_id": "i901",
            "destination": "%155",
            "attachments": [],
            "notified": true
        }),
    );
    assert_eq!(0, sent_file.status.code().unwrap());
    assert_eq!(
        "i901 -> %155; recipient nudged\n",
        String::from_utf8(sent_file.stdout).unwrap()
    );
    assert!(sent_file.stderr.is_empty());

    let listed = run_daemon_cli(
        &["inbox", "list", "--all", "--limit", "5"],
        "inbox_list",
        json!({"mailbox": null, "unread_only": false, "limit": 5}),
        json!({"mailbox": "reviewer", "messages": []}),
    );
    assert_eq!(0, listed.status.code().unwrap());
    assert_eq!(
        "mailbox reviewer\nno messages\n",
        String::from_utf8(listed.stdout).unwrap()
    );
    assert!(listed.stderr.is_empty());
}

#[test]
fn cli_generic_json_preserves_daemon_insertion_order() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        // Each CLI command probes with status, then makes its requested call.
        for _ in 0..4 {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if request["cmd"] == "status" {
                json!({})
            } else {
                ordered_result()
            };
            let response = json!({"ok": true, "result": result});
            let mut writer = &stream;
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
    });

    let panes = run_cli(&socket_text, &["panes", "--json"]);
    assert!(
        panes.status.success(),
        "{}",
        String::from_utf8_lossy(&panes.stderr)
    );
    assert_eq!(
        "{\n  \"zeta\": {\n    \"tiny\": 1e-07,\n    \"label\": \"caf\\u00e9\\ud83d\\ude00\"\n  },\n  \"alpha\": \"second\"\n}\n",
        String::from_utf8(panes.stdout).unwrap()
    );

    let token = run_cli(&socket_text, &["gh-token", "--json"]);
    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
    assert_eq!(
        "{\n  \"zeta\": {\n    \"tiny\": 1e-07,\n    \"label\": \"caf\\u00e9\\ud83d\\ude00\"\n  },\n  \"alpha\": \"second\"\n}\n",
        String::from_utf8(token.stdout).unwrap()
    );
    daemon.join().unwrap();
}

#[test]
fn cli_route_17_task_edit_a() {
    let updated = run_daemon_cli(
        &[
            "task",
            "update",
            "t7",
            "--title",
            "Updated",
            "--body",
            "new body",
            "--status",
            "blocked",
            "--assign",
            "%99",
            "--depends-on",
            "t6",
            "--parent",
            "t5",
            "--block-reason",
            "waiting",
            "--no-multi",
        ],
        "task_update",
        json!({
            "id":"t7", "title":"Updated", "body":"new body", "status":"blocked",
            "assignees":["%99"], "depends_on":["t6"], "parent_id":"t5",
            "block_reason":"waiting", "multi_assignee":false
        }),
        json!({"id":"t7", "priority":"low", "status":"blocked", "title":"Updated", "scope":"global"}),
    );
    assert_eq!(0, updated.status.code().unwrap());
    assert_eq!(
        "t7 [low] blocked  Updated\nscope       global\n",
        String::from_utf8(updated.stdout).unwrap()
    );
    assert!(updated.stderr.is_empty());

    let assigned = run_daemon_cli(
        &["task", "assign", "t7", "%98", "%99", "--mode", "set"],
        "task_assign",
        json!({"id":"t7", "assignees":["%98", "%99"], "mode":"set"}),
        json!({"id":"t7", "priority":"low", "status":"open", "title":"Updated", "scope":"global", "assignees":["%98", "%99"]}),
    );
    assert_eq!(0, assigned.status.code().unwrap());
    assert_eq!(
        "t7 [low] open  Updated\nscope       global\nassignees   %98, %99\n",
        String::from_utf8(assigned.stdout).unwrap()
    );
    assert!(assigned.stderr.is_empty());

    let completed = run_daemon_cli(
        &[
            "task",
            "complete",
            "t7",
            "--by",
            "%99",
            "--verdict",
            "needs-fix",
            "--unmerged",
        ],
        "task_complete",
        json!({
            "id": "t7",
            "by": "%99",
            "verdict": "needs-fix",
            "landed": false
        }),
        json!({"id":"t7", "priority":"low", "status":"done", "title":"Updated", "scope":"global"}),
    );
    assert_eq!(0, completed.status.code().unwrap());
    assert_eq!(
        "t7 [low] done  Updated\nscope       global\n",
        String::from_utf8(completed.stdout).unwrap()
    );
    assert!(completed.stderr.is_empty());
}

#[test]
fn cli_route_23_owner_inbox_a() {
    let pushed = run_daemon_cli(
        &["owner", "push", "%42", "hello", "owner"],
        "owner_push",
        json!({"target": "%42", "body": "hello owner", "subject": null}),
        json!({"id": 77, "pane_id": "%42"}),
    );
    assert_eq!(Some(0), pushed.status.code());
    assert_eq!(
        "pushed owner message 77 for %42 (no inbox row, nothing typed)\n",
        String::from_utf8(pushed.stdout).unwrap()
    );
    assert!(pushed.stderr.is_empty());

    let polled = run_daemon_cli(
        &["owner", "poll", "%42"],
        "owner_poll",
        json!({"pane_id": "%42", "by": "harness"}),
        json!({"pane_id": "%42", "by": "harness"}),
    );
    assert_eq!(Some(0), polled.status.code());
    assert_eq!(
        "poll recorded for %42 by harness\n",
        String::from_utf8(polled.stdout).unwrap()
    );
    assert!(polled.stderr.is_empty());

    let taken = run_daemon_cli(
        &["owner", "take", "%42", "--json"],
        "owner_take",
        json!({"pane_id": "%42", "by": null}),
        json!({"messages": []}),
    );
    assert_eq!(Some(0), taken.status.code());
    assert_eq!(
        "{\n  \"messages\": []\n}\n",
        String::from_utf8(taken.stdout).unwrap()
    );
    assert!(taken.stderr.is_empty());
}
