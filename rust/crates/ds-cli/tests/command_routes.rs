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
    run_cli_with_env(socket, command, &[])
}

fn run_cli_with_env(socket: &str, command: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cli = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
    cli.args(["--socket", socket, "--self-pane", "%99"])
        .args(command)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .env_remove("DUNKINGSHEEP_CLIENT_TIMEOUT")
        .env_remove("DUNKINGSHEEP_SEND_TIMEOUT_S");
    for (key, value) in env {
        cli.env(key, value);
    }
    cli.output().expect("run Rust CLI")
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
fn cli_route_05_dunk_send_extends_client_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        let expected = [
            (
                "fire_dunk",
                json!({"id": "d1", "countdown_s": 0, "wait": true}),
                json!({"id": "d1", "ok": true, "queued": false, "message": "delivered"}),
            ),
            (
                "send_text",
                json!({
                    "target": "Pane A",
                    "text": "slow send",
                    "hold_while_typing": true,
                    "wait_s": 0.1,
                    "desk_token": null,
                    "press_enter": true
                }),
                json!({
                    "queued": false,
                    "ok": true,
                    "target_label": "Pane A",
                    "pane_id": "%42"
                }),
            ),
        ];
        for (rpc, args, result) in expected {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!("status", request["cmd"]);
            let mut writer = &stream;
            writer.write_all(b"{\"ok\":true,\"result\":{}}\n").unwrap();

            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(rpc, request["cmd"]);
            assert_eq!(args, request["args"]);
            assert_eq!("%99", request["self_pane_id"]);
            std::thread::sleep(std::time::Duration::from_millis(350));
            let response = json!({"ok": true, "result": result});
            let mut writer = &stream;
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
    });

    let timing = [
        ("DUNKINGSHEEP_CLIENT_TIMEOUT", "0.2"),
        ("DUNKINGSHEEP_SEND_TIMEOUT_S", "0.6"),
    ];
    let fired = run_cli_with_env(&socket_text, &["test", "d1"], &timing);
    assert_eq!(Some(0), fired.status.code());
    assert_eq!(
        "d1: sent (delivered)\n",
        String::from_utf8(fired.stdout).unwrap()
    );
    assert!(fired.stderr.is_empty());

    let timing = [
        ("DUNKINGSHEEP_CLIENT_TIMEOUT", "0.2"),
        ("DUNKINGSHEEP_SEND_TIMEOUT_S", "0.05"),
    ];
    let sent = run_cli_with_env(
        &socket_text,
        &["send", "Pane A", "slow", "send", "--wait", "0.1"],
        &timing,
    );
    assert_eq!(Some(0), sent.status.code());
    assert_eq!(
        "sent to Pane A (%42)\n",
        String::from_utf8(sent.stdout).unwrap()
    );
    assert!(sent.stderr.is_empty());
    daemon.join().unwrap();
}

#[test]
fn cli_route_05_dunk_send() {
    let fired = run_daemon_cli(
        &["test", "d1"],
        "fire_dunk",
        json!({"id": "d1", "countdown_s": 0, "wait": true}),
        json!({
            "id": "d1",
            "queued": true,
            "ok": true,
            "message_id": "m1",
            "message": "someone is typing"
        }),
    );
    assert_eq!(0, fired.status.code().unwrap());
    assert_eq!(
        "d1: queued (someone is typing)\n",
        String::from_utf8(fired.stdout).unwrap()
    );
    assert!(fired.stderr.is_empty());

    let sent = run_daemon_cli(
        &[
            "send",
            "Pane A",
            "hello",
            "from",
            "Rust",
            "--wait",
            "0.25",
            "--ignore-typing",
            "--no-enter",
        ],
        "send_text",
        json!({
            "target": "Pane A",
            "text": "hello from Rust",
            "hold_while_typing": false,
            "wait_s": 0.25,
            "desk_token": null,
            "press_enter": false
        }),
        json!({
            "queued": true,
            "ok": true,
            "message_id": "m2",
            "target_label": "Pane A",
            "pane_id": "%42"
        }),
    );
    assert_eq!(0, sent.status.code().unwrap());
    assert_eq!(
        "m2 queued for Pane A (%42): someone is typing\n",
        String::from_utf8(sent.stdout).unwrap()
    );
    assert!(sent.stderr.is_empty());

    let keys = run_daemon_cli(
        &["send-keys", "Pane B", "C-x", "Enter"],
        "send_keys",
        json!({
            "target": "Pane B",
            "keys": "C-x Enter",
            "hold_while_typing": true,
            "wait_s": 10.0,
            "desk_token": null
        }),
        json!({
            "queued": false,
            "ok": true,
            "target_label": "Pane B",
            "pane_id": "%43"
        }),
    );
    assert_eq!(0, keys.status.code().unwrap());
    assert_eq!(
        "sent to Pane B (%43)\n",
        String::from_utf8(keys.stdout).unwrap()
    );
    assert!(keys.stderr.is_empty());
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
fn cli_route_03_dunk_edit_lifecycle_a() {
    let updated = run_daemon_cli(
        &[
            "set",
            "d1",
            "-T",
            "owner",
            "-t",
            "",
            "-e",
            "1.5h",
            "-n",
            "",
            "--any-time",
            "--max-sends",
            "0",
            "--always-send",
            "--ignore-typing",
        ],
        "update_dunk",
        json!({
            "id": "d1",
            "target": "owner",
            "text": "",
            "interval_minutes": "1.5h",
            "name": "",
            "only_when": null,
            "max_sends": null,
            "skip_if_unconsumed": false,
            "hold_while_typing": false
        }),
        json!({
            "id": "d1",
            "text": "updated",
            "skip_if_unconsumed": false,
            "hold_while_typing": false
        }),
    );
    assert_eq!(0, updated.status.code().unwrap());
    assert_eq!(
        "id               d1\nskip_if_unconsumed False\nhold_while_typing False\ntext             'updated'\n",
        String::from_utf8(updated.stdout).unwrap()
    );
    assert!(updated.stderr.is_empty());

    let removed = run_daemon_cli(
        &["delete", "d2"],
        "remove_dunk",
        json!({"id": "d2"}),
        json!({"id": "d2"}),
    );
    assert_eq!(0, removed.status.code().unwrap());
    assert_eq!("removed d2\n", String::from_utf8(removed.stdout).unwrap());
    assert!(removed.stderr.is_empty());

    let started = run_daemon_cli(
        &["start", "d3"],
        "start_dunk",
        json!({"id": "d3"}),
        json!({
            "id": "d3",
            "running": true,
            "status": "active",
            "skip_if_unconsumed": false,
            "hold_while_typing": true
        }),
    );
    assert_eq!(0, started.status.code().unwrap());
    assert_eq!(
        "id               d3\nrunning          True\nstatus           active\nskip_if_unconsumed False\nhold_while_typing True\n",
        String::from_utf8(started.stdout).unwrap()
    );
    assert!(started.stderr.is_empty());
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
fn cli_route_01_dunk_read_batch() {
    let turn_start = run_daemon_cli(
        &[
            "turn",
            "start",
            "--pane",
            "%44",
            "--ttl-s",
            "30",
            "--source",
            "hook",
            "--handoff-id",
            "nj-0123456789abcdef0123456789abcdef",
            "--json",
        ],
        "turn_start",
        json!({
            "pane": "%44",
            "ttl_s": 30.0,
            "source": "hook",
            "handoff_id": "nj-0123456789abcdef0123456789abcdef"
        }),
        json!({
            "ok": true,
            "pane_id": "%44",
            "started_at": 1.5,
            "expires_at": 31.5,
            "ttl_s": 30.0,
            "source": "hook",
            "turn_id": "gen-1"
        }),
    );
    assert_eq!(0, turn_start.status.code().unwrap());
    assert_eq!(
        "{\n  \"ok\": true,\n  \"pane_id\": \"%44\",\n  \"started_at\": 1.5,\n  \"expires_at\": 31.5,\n  \"ttl_s\": 30.0,\n  \"source\": \"hook\",\n  \"turn_id\": \"gen-1\"\n}\n",
        String::from_utf8(turn_start.stdout).unwrap()
    );
    assert!(turn_start.stderr.is_empty());

    let turn_start_text = run_daemon_cli(
        &["turn", "start", "--pane", "%44", "--ttl-s", "30"],
        "turn_start",
        json!({"pane": "%44", "ttl_s": 30.0, "source": null, "handoff_id": null}),
        json!({"ok": true, "pane_id": "%44", "ttl_s": 30.0}),
    );
    assert_eq!(0, turn_start_text.status.code().unwrap());
    assert_eq!(
        "turn start %44 ttl 30.0s\n",
        String::from_utf8(turn_start_text.stdout).unwrap()
    );
    assert!(turn_start_text.stderr.is_empty());

    let turn_end = run_daemon_cli(
        &[
            "turn",
            "end",
            "--pane",
            "%44",
            "--reason",
            "done",
            "--turn-id",
            "gen-1",
        ],
        "turn_end",
        json!({"pane": "%44", "reason": "done", "turn_id": "gen-1"}),
        json!({
            "ok": true,
            "pane_id": "%44",
            "ended": true,
            "dispatch": {
                "action": "nudge",
                "task_id": "task-7",
                "delivered": false,
                "outcome": "hold_typing"
            }
        }),
    );
    assert_eq!(0, turn_end.status.code().unwrap());
    assert_eq!(
        "turn end %44 (ended=True); trigger=turn.end nudged task-7: HELD, not delivered (hold_typing)\n",
        String::from_utf8(turn_end.stdout).unwrap()
    );
    assert!(turn_end.stderr.is_empty());

    let poll = run_daemon_cli(
        &[
            "poll",
            "--mailbox",
            "lane-box",
            "--limit",
            "2",
            "--panes",
            "--tasks",
            "--task-scope",
            "self",
            "--include-done",
            "--check-herdr",
        ],
        "poll",
        json!({
            "limit": 2,
            "panes": true,
            "tasks": true,
            "include_done": true,
            "check_herdr": true,
            "mailbox": "lane-box",
            "task_scope": "self"
        }),
        json!({
            "status": {
                "self_pane_id": "%99",
                "version": "2.71.4",
                "running": 2,
                "dunkers": 4
            },
            "inbox_count": {"unread": 1, "total": 3},
            "dunks": {
                "messages": [{"message_id": "m1", "sender": "hunter", "subject": "Review"}],
                "dunks": [{"id": "d1", "running": true, "interval_minutes": 5, "target_label": "Writer"}]
            }
        }),
    );
    assert_eq!(0, poll.status.code().unwrap());
    assert_eq!(
        "self %99  v2.71.4  dunks 2/4 running  inbox 1 unread / 3\n  #m1 hunter: Review\n  d1 running every 5m -> Writer\n",
        String::from_utf8(poll.stdout).unwrap()
    );
    assert!(poll.stderr.is_empty());
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
fn cli_route_04_dunk_edit_lifecycle_b() {
    let stopped = run_daemon_cli(
        &["stop", "d1"],
        "stop_dunk",
        json!({"id": "d1"}),
        json!({
            "id": "d1",
            "running": false,
            "status": "stopped",
            "skip_if_unconsumed": false,
            "hold_while_typing": true
        }),
    );
    assert_eq!(0, stopped.status.code().unwrap());
    assert_eq!(
        "id               d1\nstatus           stopped\nskip_if_unconsumed False\nhold_while_typing True\n",
        String::from_utf8(stopped.stdout).unwrap()
    );
    assert!(stopped.stderr.is_empty());

    let toggled = run_daemon_cli(
        &["toggle", "d2"],
        "toggle_dunk",
        json!({"id": "d2"}),
        json!({
            "id": "d2",
            "running": true,
            "status": "active",
            "skip_if_unconsumed": false,
            "hold_while_typing": true
        }),
    );
    assert_eq!(0, toggled.status.code().unwrap());
    assert_eq!(
        "id               d2\nrunning          True\nstatus           active\nskip_if_unconsumed False\nhold_while_typing True\n",
        String::from_utf8(toggled.stdout).unwrap()
    );
    assert!(toggled.stderr.is_empty());

    let stopped_all = run_daemon_cli(&["stop-all"], "stop_all", json!({}), json!({"dunks": []}));
    assert_eq!(0, stopped_all.status.code().unwrap());
    assert_eq!("no dunks\n", String::from_utf8(stopped_all.stdout).unwrap());
    assert!(stopped_all.stderr.is_empty());
}

#[test]
fn cli_route_16_task_create_read() {
    let added = run_daemon_cli(
        &[
            "task",
            "add",
            "Review",
            "--scope",
            "global",
            "--body",
            "check change",
            "--status",
            "blocked",
            "--priority",
            "high",
            "--assign",
            "%99",
            "--depends-on",
            "t1",
            "--parent",
            "t0",
            "--multi",
            "--kind",
            "review",
            "--author",
            "writer",
            "--repo",
            "owner/repo",
            "--pr",
            "42",
            "--head",
            "abc123",
        ],
        "task_add",
        json!({
            "title": "Review", "scope": "global", "body": "check change",
            "status": "blocked", "priority": "high", "assignees": ["%99"],
            "depends_on": ["t1"], "parent_id": "t0", "multi_assignee": true,
            "kind": "review", "author": "writer", "repo": "owner/repo", "pr": 42,
            "head": "abc123"
        }),
        json!({"id":"t2", "priority":"high", "status":"blocked", "title":"Review", "scope":"global"}),
    );
    assert_eq!(0, added.status.code().unwrap());
    assert_eq!(
        "t2 [high] blocked  Review\nscope       global\n",
        String::from_utf8(added.stdout).unwrap()
    );
    assert!(added.stderr.is_empty());

    let listed = run_daemon_cli(
        &[
            "task",
            "ls",
            "global",
            "--status",
            "open",
            "--assignee",
            "%99",
            "--all",
            "-n",
            "7",
            "--kind",
            "work",
            "--needs-owner",
        ],
        "task_list",
        json!({"scope":"global", "status":"open", "assignee":"%99", "include_done":true, "limit":7, "kind":"work", "needs_owner":true}),
        json!({"scope":"global", "pane_id":"%99", "tasks":[]}),
    );
    assert_eq!(0, listed.status.code().unwrap());
    assert_eq!(
        "scope global (pane %99)\nno tasks\n",
        String::from_utf8(listed.stdout).unwrap()
    );
    assert!(listed.stderr.is_empty());

    let read = run_daemon_cli(
        &["task", "read", "t2"],
        "task_read",
        json!({"id":"t2"}),
        json!({"id":"t2", "priority":"high", "status":"blocked", "title":"Review", "scope":"global"}),
    );
    assert_eq!(0, read.status.code().unwrap());
    assert_eq!(
        "t2 [high] blocked  Review\nscope       global\n",
        String::from_utf8(read.stdout).unwrap()
    );
    assert!(read.stderr.is_empty());
}

#[test]
fn cli_route_08_inbox_write_a() {
    let acked = run_daemon_cli(
        &["inbox", "ack", "i1", "i2", "--mailbox", "reviewer"],
        "inbox_ack",
        json!({"message_ids": "i1,i2", "mailbox": "reviewer"}),
        json!({
            "state": "acked",
            "changed": ["i1", "i2"],
            "acked_in": "reviewer",
            "unchanged": 1
        }),
    );
    assert_eq!(0, acked.status.code().unwrap());
    assert_eq!(
        "acked: i1, i2 in mailbox 'reviewer'; unchanged 1\n",
        String::from_utf8(acked.stdout).unwrap()
    );
    assert!(acked.stderr.is_empty());

    let cancelled = run_daemon_cli(
        &["inbox", "cancel", "i3", "i4"],
        "inbox_cancel",
        json!({"message_ids": "i3,i4"}),
        json!({"state": "cancelled", "changed": ["i3"], "unchanged": 1}),
    );
    assert_eq!(0, cancelled.status.code().unwrap());
    assert_eq!(
        "cancelled: i3; unchanged 1\n",
        String::from_utf8(cancelled.stdout).unwrap()
    );
    assert!(cancelled.stderr.is_empty());

    let deleted = run_daemon_cli(
        &["inbox", "delete", "i5", "i6"],
        "inbox_delete",
        json!({"message_ids": "i5,i6"}),
        json!({"state": "deleted", "deleted": ["i5", "i6"]}),
    );
    assert_eq!(0, deleted.status.code().unwrap());
    assert_eq!(
        "deleted: i5, i6\n",
        String::from_utf8(deleted.stdout).unwrap()
    );
    assert!(deleted.stderr.is_empty());
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

#[test]
fn cli_route_24_owner_inbox_b() {
    let conversation = run_daemon_cli(
        &["owner", "conversation", "--limit", "7", "--before", "12.5"],
        "owner_conversation",
        json!({"limit": 7, "before": 12.5}),
        json!({"mailbox": "owner", "count": 0, "waiting": [], "messages": []}),
    );
    assert_eq!(Some(0), conversation.status.code());
    assert_eq!(
        "mailbox owner  messages 0  waiting 0\n",
        String::from_utf8(conversation.stdout).unwrap()
    );
    assert!(conversation.stderr.is_empty());

    let reply_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(reply_file.path(), "file reply\n").unwrap();
    let reply_path = reply_file.path().to_str().unwrap();
    let reply = run_daemon_cli(
        &["owner", "reply", "question-4", "--body-file", reply_path],
        "owner_reply",
        json!({"message_id": "question-4", "body": "file reply\n"}),
        json!({
            "question": "question-4",
            "reply": {"message_id": "reply-5"},
            "transport": "web",
            "acked": true
        }),
    );
    assert_eq!(Some(0), reply.status.code());
    assert_eq!(
        "answered question-4 with reply-5 on web (question True acked)\n",
        String::from_utf8(reply.stdout).unwrap()
    );
    assert!(reply.stderr.is_empty());

    let ingest_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(ingest_file.path(), "incoming question\n").unwrap();
    let ingest_path = ingest_file.path().to_str().unwrap();
    let ingested = run_daemon_cli(
        &[
            "owner",
            "ingest",
            "telegram",
            "--body-file",
            ingest_path,
            "--external-id",
            "telegram:91",
        ],
        "owner_ingest",
        json!({
            "transport": "telegram",
            "body": "incoming question\n",
            "external_id": "telegram:91"
        }),
        json!({"accepted": true, "message_id": "question-6"}),
    );
    assert_eq!(Some(0), ingested.status.code());
    assert_eq!(
        "{\n  \"accepted\": true,\n  \"message_id\": \"question-6\"\n}\n",
        String::from_utf8(ingested.stdout).unwrap()
    );
    assert!(ingested.stderr.is_empty());

    let temp = tempfile::tempdir().unwrap();
    let missing = run_cli(
        &temp.path().join("no-daemon.sock").display().to_string(),
        &["owner", "reply", "question-7"],
    );
    assert_eq!(Some(1), missing.status.code());
    assert!(missing.stdout.is_empty());
    assert_eq!(
        "owner reply: a body or --body-file is required\n",
        String::from_utf8(missing.stderr).unwrap()
    );
}

#[test]
fn cli_route_25_owner_inbox_c() {
    let outbox = run_daemon_cli(
        &["owner", "outbox", "telegram", "--limit", "9"],
        "owner_outbox",
        json!({"transport": "telegram", "limit": 9}),
        json!({"count": 0, "rows": []}),
    );
    assert_eq!(Some(0), outbox.status.code());
    assert_eq!(
        "{\n  \"count\": 0,\n  \"rows\": []\n}\n",
        String::from_utf8(outbox.stdout).unwrap()
    );
    assert!(outbox.stderr.is_empty());

    let delivered = run_daemon_cli(
        &["owner", "delivered", "question-8", "web"],
        "owner_delivered",
        json!({"message_id": "question-8", "transport": "web"}),
        json!({"delivered": true}),
    );
    assert_eq!(Some(0), delivered.status.code());
    assert_eq!(
        "{\n  \"delivered\": true\n}\n",
        String::from_utf8(delivered.stdout).unwrap()
    );
    assert!(delivered.stderr.is_empty());

    let stats = run_daemon_cli(
        &["owner", "stats"],
        "owner_stats",
        json!({}),
        json!({"counts": {"queued": 2}}),
    );
    assert_eq!(Some(0), stats.status.code());
    assert_eq!(
        "{\n  \"counts\": {\n    \"queued\": 2\n  }\n}\n",
        String::from_utf8(stats.stdout).unwrap()
    );
    assert!(stats.stderr.is_empty());
}

#[test]
fn cli_route_26_owner_fallback_and_body_newlines() {
    let stats = run_daemon_cli(
        &["owner", "--json"],
        "owner_stats",
        json!({}),
        json!({"counts": {"queued": 2}}),
    );
    assert_eq!(Some(0), stats.status.code());
    assert_eq!(
        "{\n  \"counts\": {\n    \"queued\": 2\n  }\n}\n",
        String::from_utf8(stats.stdout).unwrap()
    );
    assert!(stats.stderr.is_empty());

    let reply_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(reply_file.path(), b"first\r\nsecond\rthird").unwrap();
    let reply_path = reply_file.path().to_str().unwrap();
    let reply = run_daemon_cli(
        &["owner", "reply", "question-26", "--body-file", reply_path],
        "owner_reply",
        json!({"message_id": "question-26", "body": "first\nsecond\nthird"}),
        json!({
            "question": "question-26",
            "reply": {"message_id": "reply-26"},
            "transport": "web",
            "acked": true
        }),
    );
    assert_eq!(Some(0), reply.status.code());
    assert!(reply.stderr.is_empty());

    let ingest_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(ingest_file.path(), b"line one\r\nline two\rline three").unwrap();
    let ingest_path = ingest_file.path().to_str().unwrap();
    let ingest = run_daemon_cli(
        &["owner", "ingest", "web", "--body-file", ingest_path],
        "owner_ingest",
        json!({"transport": "web", "body": "line one\nline two\nline three"}),
        json!({"accepted": true}),
    );
    assert_eq!(Some(0), ingest.status.code());
    assert!(ingest.stderr.is_empty());
}

#[test]
fn cli_route_07_inbox_read_b() {
    let read = run_daemon_cli(
        &["inbox", "read", "reviewer", "--limit", "3", "--peek"],
        "inbox_read",
        json!({"mailbox": "reviewer", "limit": 3, "peek": true}),
        json!({
            "mailbox": "reviewer",
            "messages": [{
                "message_id": "i10",
                "sender": "codex",
                "subject": "Review",
                "attachments": [{
                    "id": "a1",
                    "filename": "diff.txt",
                    "size": 3,
                    "media_type": "text/plain"
                }],
                "body": "Please review"
            }],
            "truncated": true,
            "omitted": 2,
            "note": "older messages omitted"
        }),
    );
    assert_eq!(0, read.status.code().unwrap());
    assert_eq!(
        "--- i10 from codex | Review\n    attachment a1  diff.txt  3 bytes  text/plain\nPlease review\n\n[capped: 2 older message(s) left out - older messages omitted]\n",
        String::from_utf8(read.stdout).unwrap()
    );
    assert!(read.stderr.is_empty());

    let recovered = run_daemon_cli(
        &["inbox", "recover", "reviewer"],
        "inbox_recover",
        json!({"mailbox": "reviewer"}),
        json!({
            "mailbox": "reviewer",
            "messages": [{
                "message_id": "i11",
                "sender": "owner",
                "attachments": [],
                "body": "Recovered content"
            }]
        }),
    );
    assert_eq!(0, recovered.status.code().unwrap());
    assert_eq!(
        "mailbox reviewer: recovered 1 delivered message(s) - display them, then inbox ack\n--- i11 from owner\nRecovered content\n\n",
        String::from_utf8(recovered.stdout).unwrap()
    );
    assert!(recovered.stderr.is_empty());

    let status = run_daemon_cli(
        &["inbox", "status", "i12"],
        "inbox_status",
        json!({"message_id": "i12"}),
        json!({
            "message_id": "i12",
            "state": "acked",
            "destination": "reviewer",
            "notify": {"state": "sent", "reason": "accepted"}
        }),
    );
    assert_eq!(0, status.status.code().unwrap());
    assert_eq!(
        "message_id        i12\nstate             acked\ndestination       reviewer\nnotify            sent (accepted)\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());
}
