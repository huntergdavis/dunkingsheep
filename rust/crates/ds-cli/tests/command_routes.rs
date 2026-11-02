//! Generic `--json` daemon output through the actual Rust CLI process and an isolated fake socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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

fn run_local_context_cli(
    cwd: &Path,
    config_dir: &Path,
    superuser: &str,
    args: &[String],
) -> Output {
    run_local_context_cli_with_ledger(cwd, config_dir, superuser, args, None)
}

fn run_local_context_cli_with_ledger(
    cwd: &Path,
    config_dir: &Path,
    superuser: &str,
    args: &[String],
    ledger: Option<&Path>,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
    command
        .args(["--self-pane", "%99"])
        .args(args)
        .current_dir(cwd)
        .env("DUNKINGSHEEP_DIR", config_dir)
        .env("DUNKINGSHEEP_SUPERUSER", superuser)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_CONTEXT_LEDGER")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
    if let Some(ledger) = ledger {
        command.env("DUNKINGSHEEP_CONTEXT_LEDGER", ledger);
    }
    command.output().expect("run Rust CLI")
}

fn run_cli_with_stdin(socket: &str, command: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .args(["--socket", socket, "--self-pane", "%99"])
        .args(command)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn Rust CLI");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait for Rust CLI")
}

fn run_local_cli(
    socket: &Path,
    cwd: &Path,
    global_dir: &Path,
    command: &[&str],
    stdin: Option<&[u8]>,
    superuser: Option<&str>,
) -> Output {
    let mut cli = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
    cli.args(["--socket", socket.to_str().unwrap(), "--self-pane", "%99"])
        .args(command)
        .current_dir(cwd)
        .env("DUNKINGSHEEP_DIR", global_dir)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .env_remove("DUNKINGSHEEP_SUPERUSER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(superuser) = superuser {
        cli.env("DUNKINGSHEEP_SUPERUSER", superuser);
    }
    let mut child = cli.spawn().expect("run local Rust CLI");
    if let Some(stdin) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin)
            .expect("write CLI stdin");
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().expect("wait for local Rust CLI")
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
fn cli_route_14_pm_nonfinite_demo() {
    // Golden nonfinite/surrogate payloads from the Python CLI's json.loads/json.dumps path.
    for (raw, expected) in [
        (r###"{"note": NaN}"###, r###"{"note": NaN, "demo": true}"###),
        (
            r###"{"a": [NaN, Infinity, -Infinity], "nested": {"n": NaN}, "demo": false}"###,
            r###"{"a": [NaN, Infinity, -Infinity], "nested": {"n": NaN}, "demo": true}"###,
        ),
        (
            r###"{"literal": "NaN Infinity -Infinity", "escaped": "\"NaN\"", "unicode": "café"}"###,
            r###"{"literal": "NaN Infinity -Infinity", "escaped": "\"NaN\"", "unicode": "caf\u00e9", "demo": true}"###,
        ),
        (
            r###"{"collision": ["\u005f_dunk_pm_nonfinite_NaN"], "n": NaN}"###,
            r###"{"collision": ["__dunk_pm_nonfinite_NaN"], "n": NaN, "demo": true}"###,
        ),
        (
            r###"{NaN: 1}"###,
            r###"{"note": "{NaN: 1}", "demo": true}"###,
        ),
        (
            r###"{"n": NaNwat}"###,
            r###"{"note": "{\"n\": NaNwat}", "demo": true}"###,
        ),
        (
            r###"{"n":1e400,"negative":-1e400,"zero":-0,"floatzero":-0.0}"###,
            r###"{"n": Infinity, "negative": -Infinity, "zero": 0, "floatzero": -0.0, "demo": true}"###,
        ),
        (
            r###"{"note":"\ud800"}"###,
            r###"{"note": "\ud800", "demo": true}"###,
        ),
        (
            r###"{"note":"\udfff"}"###,
            r###"{"note": "\udfff", "demo": true}"###,
        ),
        (
            r###"{"note":"\ud83d\ude00"}"###,
            r###"{"note": "\ud83d\ude00", "demo": true}"###,
        ),
        (
            r###"{"note":"\\ud800","pair":"\\ud83d\\ude00"}"###,
            r###"{"note": "\\ud800", "pair": "\\ud83d\\ude00", "demo": true}"###,
        ),
        (
            r###"{"\ud800":["\udfff","\ud800\ud83d\ude00"],"n":NaN}"###,
            r###"{"\ud800": ["\udfff", "\ud800\ud83d\ude00"], "n": NaN, "demo": true}"###,
        ),
        (
            r###"{"literal":"x\u005f_dunk_pm_escape_surrogate_d800_y","note":"\ud800","constant":["__dunk_pm_escape_NaN"],"n":NaN}"###,
            r###"{"literal": "x__dunk_pm_escape_surrogate_d800_y", "note": "\ud800", "constant": ["__dunk_pm_escape_NaN"], "n": NaN, "demo": true}"###,
        ),
        (
            r###"{"note":"\ud80x"}"###,
            r###"{"note": "{\"note\":\"\\ud80x\"}", "demo": true}"###,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("daemon.sock");
        let socket_text = socket.display().to_string();
        let listener = UnixListener::bind(&socket).unwrap();
        let expected = expected.to_string();
        let daemon = std::thread::spawn(move || {
            for (command, args, result) in [
                ("status", json!({}), json!({})),
                (
                    "pm_digest",
                    json!({"project": "nonfinite", "payload": expected, "to": null}),
                    json!({"ok": true}),
                ),
            ] {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(command, request["cmd"]);
                assert_eq!(args, request["args"]);
                assert_eq!("%99", request["self_pane_id"]);
                let response = json!({"ok": true, "result": result});
                (&stream)
                    .write_all(format!("{response}\n").as_bytes())
                    .unwrap();
            }
        });
        let output = run_cli_with_stdin(
            &socket_text,
            &["pm", "digest", "nonfinite", "--demo", "--json"],
            raw,
        );
        daemon.join().unwrap();
        assert_eq!(Some(0), output.status.code(), "{raw}");
        assert!(output.stderr.is_empty(), "{raw}");
        assert_eq!(
            json!({"ok": true}),
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        );
    }
}

#[test]
fn cli_route_14_pm() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        let expected = [
            (
                "pm_digest",
                json!({"project": "alpha", "payload": "{\"label\": \"caf\\u00e9\", \"demo\": true}", "to": "%1"}),
                json!({"action":"updated", "digest_id":"d1", "destination":"%1", "state":"stored", "body_bytes":48}),
            ),
            (
                "pm_digest",
                json!({"project": "beta", "payload": "{\"note\": \"hello\\n\", \"demo\": true}", "to": null}),
                json!({"action":"updated", "digest_id":"d2", "destination":"%2", "state":"stored", "body_bytes":35}),
            ),
            (
                "pm_digest",
                json!({"project": "gamma", "payload": "raw payload\n", "to": null}),
                json!({"action":"updated", "digest_id":"d3", "destination":"%3", "state":"stored", "body_bytes":12}),
            ),
            (
                "pm_digest",
                json!({
                    "project": "delta",
                    "payload": "{\"n\": 18446744073709551617, \"demo\": true}",
                    "to": null
                }),
                json!({"action":"updated", "digest_id":"d4", "destination":"%4", "state":"stored", "body_bytes":41}),
            ),
            (
                "pm_digest",
                json!({
                    "project": "epsilon",
                    "payload": "{\"label\": \"\\u007f\", \"demo\": true}",
                    "to": null
                }),
                json!({"action":"updated", "digest_id":"d5", "destination":"%5", "state":"stored", "body_bytes":33}),
            ),
            (
                "pm_routing",
                json!({}),
                json!({"registry":"/tmp/registry", "routing":{"alpha":{"pm":"%1", "mode":"digest", "members":["%2"]}}}),
            ),
        ];
        for (expected_cmd, expected_args, result) in expected {
            for (cmd, args, result) in [
                ("status", json!({}), json!({})),
                (expected_cmd, expected_args, result),
            ] {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(cmd, request["cmd"]);
                assert_eq!(args, request["args"]);
                assert_eq!("%99", request["self_pane_id"]);
                let response = json!({"ok": true, "result": result});
                let mut writer = &stream;
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .unwrap();
            }
        }
    });

    let demo_json = run_cli_with_stdin(
        &socket_text,
        &["pm", "digest", "alpha", "--to", "%1", "--demo"],
        "{\"label\": \"café\"}",
    );
    assert_eq!(Some(0), demo_json.status.code());
    assert_eq!(
        "updated d1 -> %1 (stored, 48 bytes)\n",
        String::from_utf8(demo_json.stdout).unwrap()
    );
    assert!(demo_json.stderr.is_empty());

    let demo_note = run_cli_with_stdin(
        &socket_text,
        &["pm", "digest", "beta", "--demo", "--json"],
        "hello\n",
    );
    assert_eq!(Some(0), demo_note.status.code());
    assert!(demo_note.stderr.is_empty());
    let result: Value = serde_json::from_slice(&demo_note.stdout).unwrap();
    assert_eq!("d2", result["digest_id"]);

    let raw = run_cli_with_stdin(&socket_text, &["pm", "digest", "gamma"], "raw payload\n");
    assert_eq!(Some(0), raw.status.code());
    assert_eq!(
        "updated d3 -> %3 (stored, 12 bytes)\n",
        String::from_utf8(raw.stdout).unwrap()
    );
    assert!(raw.stderr.is_empty());

    let large_integer = run_cli_with_stdin(
        &socket_text,
        &["pm", "digest", "delta", "--demo", "--json"],
        "{\"n\":18446744073709551617}",
    );
    assert_eq!(Some(0), large_integer.status.code());
    assert!(large_integer.stderr.is_empty());
    let result: Value = serde_json::from_slice(&large_integer.stdout).unwrap();
    assert_eq!("d4", result["digest_id"]);

    let del = run_cli_with_stdin(
        &socket_text,
        &["pm", "digest", "epsilon", "--demo", "--json"],
        "{\"label\":\"\u{7f}\"}",
    );
    assert_eq!(Some(0), del.status.code());
    assert!(del.stderr.is_empty());
    let result: Value = serde_json::from_slice(&del.stdout).unwrap();
    assert_eq!("d5", result["digest_id"]);

    let routing = run_cli(&socket_text, &["pm", "routing"]);
    assert_eq!(Some(0), routing.status.code());
    assert_eq!(
        "alpha: pm %1  mode digest  lanes %2\n",
        String::from_utf8(routing.stdout).unwrap()
    );
    assert!(routing.stderr.is_empty());
    daemon.join().unwrap();
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
fn cli_route_56_mcp_setup() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let capture = temp.path().join("claude-argv");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\nexit 7\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let original_path = std::env::var("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_dunkingsheep"))
        .unwrap()
        .display()
        .to_string();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("DUNKINGSHEEP_DIR", home.join("dunkingsheep"))
            .env("PATH", &path)
            .env("CAPTURE", &capture)
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
            .output()
            .unwrap()
    };

    let json = run(&["mcp-setup", "json"]);
    assert_eq!(0, json.status.code().unwrap());
    assert_eq!(
        format!(
            "{{\n  \"mcpServers\": {{\n    \"dunkingsheep\": {{\n      \"command\": \"{executable}\",\n      \"args\": [\"mcp\"]\n    }}\n  }}\n}}\n\n"
        ),
        String::from_utf8(json.stdout).unwrap()
    );
    assert!(json.stderr.is_empty());

    let apply = run(&["mcp-setup", "claude", "--scope", "project", "--apply"]);
    assert_eq!(7, apply.status.code().unwrap());
    assert_eq!(
        format!(
            "# Claude Code\nclaude mcp add --scope project dunkingsheep -- {executable} mcp\nclaude mcp list   # expect: dunkingsheep: {executable} mcp - Connected\n\n$ claude mcp add --scope project dunkingsheep -- {executable} mcp\n"
        ),
        String::from_utf8(apply.stdout).unwrap()
    );
    assert!(apply.stderr.is_empty());
    assert_eq!(
        format!("mcp\nadd\n--scope\nproject\ndunkingsheep\n--\n{executable}\nmcp\n"),
        std::fs::read_to_string(&capture).unwrap()
    );

    let cursor = run(&["mcp-setup", "cursor", "--apply"]);
    assert_eq!(2, cursor.status.code().unwrap());
    assert!(String::from_utf8(cursor.stdout)
        .unwrap()
        .contains("\"args\": [\"mcp\"]"));
    assert_eq!(
        "error: cursor has no CLI to apply; paste the block above\n",
        String::from_utf8(cursor.stderr).unwrap()
    );
    assert!(!home.join("config").exists());
    assert!(!home.join("dunkingsheep").exists());

    let quoted_dir = temp
        .path()
        .join("quoted space \"double\" 'single' \\backslash");
    std::fs::create_dir_all(&quoted_dir).unwrap();
    let quoted_executable = quoted_dir.join("dunkingsheep");
    std::fs::copy(env!("CARGO_BIN_EXE_dunkingsheep"), &quoted_executable).unwrap();
    let run_quoted = |harness: &str| {
        Command::new(&quoted_executable)
            .args(["mcp-setup", harness])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("DUNKINGSHEEP_DIR", home.join("dunkingsheep"))
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
            .output()
            .unwrap()
    };

    let quoted_json = run_quoted("json");
    assert_eq!(0, quoted_json.status.code().unwrap());
    assert!(quoted_json.stderr.is_empty());
    let json_config: Value = serde_json::from_slice(&quoted_json.stdout).unwrap();
    assert_eq!(
        quoted_executable.display().to_string(),
        json_config["mcpServers"]["dunkingsheep"]["command"]
    );
    assert_eq!(
        json!("mcp"),
        json_config["mcpServers"]["dunkingsheep"]["args"][0]
    );

    let quoted_toml = run_quoted("toml");
    assert_eq!(0, quoted_toml.status.code().unwrap());
    assert!(quoted_toml.stderr.is_empty());
    let encoded_path = serde_json::to_string(&quoted_executable.display().to_string()).unwrap();
    assert_eq!(
        format!("[mcp_servers.dunkingsheep]\ncommand = {encoded_path}\nargs = [\"mcp\"]\n\n"),
        String::from_utf8(quoted_toml.stdout).unwrap()
    );

    let quoted_path = quoted_executable.display().to_string();
    for (harness, expected_argv) in [
        (
            "claude",
            vec![
                "claude".into(),
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                "user".into(),
                "dunkingsheep".into(),
                "--".into(),
                quoted_path.clone(),
                "mcp".into(),
            ],
        ),
        (
            "codex",
            vec![
                "codex".into(),
                "mcp".into(),
                "add".into(),
                "dunkingsheep".into(),
                "--".into(),
                quoted_path.clone(),
                "mcp".into(),
            ],
        ),
        (
            "gemini",
            vec![
                "gemini".into(),
                "mcp".into(),
                "add".into(),
                "--scope".into(),
                "user".into(),
                "dunkingsheep".into(),
                quoted_path.clone(),
                "mcp".into(),
            ],
        ),
    ] {
        let setup = run_quoted(harness);
        assert_eq!(0, setup.status.code().unwrap());
        assert!(setup.stderr.is_empty());
        let setup = String::from_utf8(setup.stdout).unwrap();
        let command_line = setup.lines().nth(1).unwrap();
        let parsed = Command::new("sh")
            .args([
                "-c",
                "eval \"set -- $1\"; printf '%s\\n' \"$@\"",
                "--",
                command_line,
            ])
            .output()
            .unwrap();
        assert!(parsed.status.success());
        let parsed: Vec<String> = String::from_utf8(parsed.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            expected_argv, parsed,
            "rendered {harness} argv did not round-trip"
        );
    }
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
fn cli_route_18_task_edit_b() {
    let blocked = run_daemon_cli(
        &["task", "block", "t8", "--reason", "waiting on input"],
        "task_block",
        json!({"id":"t8", "reason":"waiting on input"}),
        json!({"id":"t8", "priority":"medium", "status":"blocked", "title":"Work", "scope":"global", "block_reason":"waiting on input"}),
    );
    assert_eq!(0, blocked.status.code().unwrap());
    assert_eq!(
        "t8 [medium] blocked  Work\nscope       global\nblocked     waiting on input\n",
        String::from_utf8(blocked.stdout).unwrap()
    );
    assert!(blocked.stderr.is_empty());

    let unblocked = run_daemon_cli(
        &["task", "unblock", "t8"],
        "task_unblock",
        json!({"id":"t8"}),
        json!({"id":"t8", "priority":"medium", "status":"open", "title":"Work", "scope":"global"}),
    );
    assert_eq!(0, unblocked.status.code().unwrap());
    assert_eq!(
        "t8 [medium] open  Work\nscope       global\n",
        String::from_utf8(unblocked.stdout).unwrap()
    );
    assert!(unblocked.stderr.is_empty());

    let cancelled = run_daemon_cli(
        &["task", "cancel", "t8"],
        "task_cancel",
        json!({"id":"t8", "reason":null}),
        json!({"id":"t8", "priority":"medium", "status":"cancelled", "title":"Work", "scope":"global"}),
    );
    assert_eq!(0, cancelled.status.code().unwrap());
    assert_eq!(
        "t8 [medium] cancelled  Work\nscope       global\n",
        String::from_utf8(cancelled.stdout).unwrap()
    );
    assert!(cancelled.stderr.is_empty());
}

#[test]
fn cli_route_19_task_edit_c() {
    let deleted = run_daemon_cli(
        &["task", "delete", "t9"],
        "task_delete",
        json!({"id":"t9"}),
        json!({"deleted":"t9"}),
    );
    assert_eq!(0, deleted.status.code().unwrap());
    assert_eq!("deleted t9\n", String::from_utf8(deleted.stdout).unwrap());
    assert!(deleted.stderr.is_empty());

    let noted = run_daemon_cli(
        &[
            "task", "note", "t9", "--event", "review", "--reason", "checked",
        ],
        "task_note",
        json!({"id":"t9", "event":"review", "reason":"checked"}),
        json!({"noted":"t9", "event":"review"}),
    );
    assert_eq!(0, noted.status.code().unwrap());
    assert_eq!(
        "noted t9: review\n",
        String::from_utf8(noted.stdout).unwrap()
    );
    assert!(noted.stderr.is_empty());

    let next = run_daemon_cli(
        &["task", "next", "mine", "--lease", "90.5", "--allow-busy"],
        "task_next",
        json!({"scope":"mine", "lease_seconds":90.5, "allow_busy":true}),
        json!({"task":null, "refused":{"reason":"empty"}, "following":true}),
    );
    assert_eq!(0, next.status.code().unwrap());
    assert_eq!(
        "no eligible task: the queue is empty for this scope\nfollowing the queue: the daemon will push the next task when this pane is idle\n",
        String::from_utf8(next.stdout).unwrap()
    );
    assert!(next.stderr.is_empty());

    for (lease_arg, lease_seconds, rendered) in [
        ("90.5", json!(90.5), "90"),
        ("1e20", json!(1e20), "100000000000000000000"),
    ] {
        let claimed = run_daemon_cli(
            &["task", "next", "mine", "--lease", lease_arg],
            "task_next",
            json!({"scope":"mine", "lease_seconds":lease_seconds.clone(), "allow_busy":false}),
            json!({"task":{"id":"t9", "priority":"medium", "status":"in_progress", "title":"Work", "scope":"global"}, "lease_seconds":lease_seconds}),
        );
        assert_eq!(Some(0), claimed.status.code());
        assert_eq!(
            format!("claimed t9 (lease {rendered}s)\nt9 [medium] in_progress  Work\nscope       global\n"),
            String::from_utf8(claimed.stdout).unwrap()
        );
        assert!(claimed.stderr.is_empty());
    }
}

#[test]
fn cli_route_20_task_claim_a() {
    let heartbeat = run_daemon_cli(
        &["task", "heartbeat", "t10", "--lease", "90.5"],
        "task_heartbeat",
        json!({"id":"t10", "lease_seconds":90.5}),
        json!({"task_id":"t10", "lease_seconds":90.5}),
    );
    assert_eq!(0, heartbeat.status.code().unwrap());
    assert_eq!(
        "lease on t10 extended (90s)\n",
        String::from_utf8(heartbeat.stdout).unwrap()
    );
    assert!(heartbeat.stderr.is_empty());

    let long_heartbeat = run_daemon_cli(
        &["task", "heartbeat", "t10", "--lease", "1e20"],
        "task_heartbeat",
        json!({"id":"t10", "lease_seconds":1e20}),
        json!({"task_id":"t10", "lease_seconds":1e20}),
    );
    assert_eq!(Some(0), long_heartbeat.status.code());
    assert_eq!(
        "lease on t10 extended (100000000000000000000s)\n",
        String::from_utf8(long_heartbeat.stdout).unwrap()
    );
    assert!(long_heartbeat.stderr.is_empty());

    let released = run_daemon_cli(
        &["task", "release", "t10"],
        "task_release",
        json!({"id":"t10"}),
        json!({"id":"t10", "priority":"medium", "status":"open", "title":"Work", "scope":"global"}),
    );
    assert_eq!(0, released.status.code().unwrap());
    assert_eq!(
        "t10 [medium] open  Work\nscope       global\n",
        String::from_utf8(released.stdout).unwrap()
    );
    assert!(released.stderr.is_empty());

    let followed = run_daemon_cli(
        &["task", "follow", "self"],
        "task_follow",
        json!({"scope":"self"}),
        json!({"scope":"self", "pane_id":"%99", "followers":["%99", "%98"]}),
    );
    assert_eq!(0, followed.status.code().unwrap());
    assert_eq!(
        "following self as %99 (2 follower(s))\n",
        String::from_utf8(followed.stdout).unwrap()
    );
    assert!(followed.stderr.is_empty());
}

#[test]
fn cli_route_21_task_claim_b() {
    let unfollowed = run_daemon_cli(
        &["task", "unfollow"],
        "task_unfollow",
        json!({}),
        json!({"was_following":true}),
    );
    assert_eq!(0, unfollowed.status.code().unwrap());
    assert_eq!(
        "stopped following\n",
        String::from_utf8(unfollowed.stdout).unwrap()
    );
    assert!(unfollowed.stderr.is_empty());

    let followers = run_daemon_cli(
        &["task", "followers"],
        "task_followers",
        json!({}),
        json!({"followers":[{"pane_id":"%99", "scope":"global"}]}),
    );
    assert_eq!(0, followers.status.code().unwrap());
    assert_eq!(
        "%99          global\n",
        String::from_utf8(followers.stdout).unwrap()
    );
    assert!(followers.stderr.is_empty());

    let stats = run_daemon_cli(
        &["task", "stats", "global"],
        "task_stats",
        json!({"scope":"global"}),
        json!({"totals":{"open":2, "done":1}, "by_scope":{"global":{"open":2, "done":0}}}),
    );
    assert_eq!(0, stats.status.code().unwrap());
    assert_eq!(
        "open=2 done=1\n  global: open=2\n",
        String::from_utf8(stats.stdout).unwrap()
    );
    assert!(stats.stderr.is_empty());
}

#[test]
fn cli_route_22_task_metrics() {
    let metrics = run_daemon_cli(
        &[
            "task", "metrics", "global", "--window", "7200.5", "--since", "100.25", "--until",
            "7300.75", "--bucket", "day",
        ],
        "task_metrics",
        json!({"scope":"global", "window_s":7200.5, "since":100.25, "until":7300.75, "bucket":"day"}),
        json!({
            "scope":"global", "window":{"seconds":3600, "bucket":"day"},
            "current":{"by_status":{"open":2}, "by_priority":{"high":2}, "by_scope":{"global":2, "pane":0}, "leader_held":{"count":0, "oldest_age_s":0}},
            "throughput":{"created":3, "claimed":1, "completed":1, "cancelled":0, "requeued":1, "buckets":[]}
        }),
    );
    assert_eq!(0, metrics.status.code().unwrap());
    assert_eq!(
        "scope global  window 3600s (day buckets)\nstatus:   open=2\npriority: high=2\nscope:    global=2 pane=0\nthroughput: created=3 claimed=1 completed=1 cancelled=0 requeued=1\nleader-held tasks: 0\n",
        String::from_utf8(metrics.stdout).unwrap()
    );
    assert!(metrics.stderr.is_empty());

    let reap = run_daemon_cli(
        &["task", "reap"],
        "task_reap",
        json!({}),
        json!({"requeued":[{"task_id":"t7"}, {"task_id":"t8"}]}),
    );
    assert_eq!(0, reap.status.code().unwrap());
    assert_eq!(
        "requeued 2 lease(s): t7, t8\n",
        String::from_utf8(reap.stdout).unwrap()
    );
    assert!(reap.stderr.is_empty());
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

#[test]
fn cli_inbox_read_messages_only_forwards_opt_out() {
    let output = run_daemon_cli(
        &["inbox", "read", "reviewer", "--messages-only", "--peek"],
        "inbox_read",
        json!({"mailbox": "reviewer", "limit": 10, "peek": true, "include_tasks": false}),
        json!({"mailbox": "reviewer", "messages": [], "truncated": false, "omitted": 0, "note": null}),
    );
    assert_eq!(0, output.status.code().unwrap());
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_route_11_context_local_write_a() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let global_dir = root.join("isolated-global");
    let socket = root.join("unused-daemon.sock");
    let source_file = root.join("body.md");
    std::fs::write(&source_file, b"file first\r\nfile second\rfile third").unwrap();
    let source_path = source_file.to_str().unwrap();

    let inline = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            source_path,
            "--text",
            "inline wins",
            "--class",
            "fact",
            "--subject",
            "inline entry",
            "--type",
            "measurement",
            "--authority",
            "measured",
            "--source",
            "fixture",
            "--stated",
            "2026-10-20",
            "--recheck-after",
            "30d",
            "--local",
            "--json",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), inline.status.code());
    assert!(inline.stderr.is_empty());
    assert_eq!(
        format!(
            "{{\n  \"slug\": \"2026-10-20-inline-entry\",\n  \"path\": \".dunkingsheep/fact/2026-10-20-inline-entry.md\",\n  \"class\": \"fact\"\n}}\n"
        ),
        String::from_utf8(inline.stdout).unwrap()
    );
    let inline_path = root.join(".dunkingsheep/fact/2026-10-20-inline-entry.md");
    let inline_article = std::fs::read_to_string(&inline_path).unwrap();
    assert!(inline_article.ends_with("inline wins\n"));
    assert!(!inline_article.contains("file first"));

    let from_file = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            source_path,
            "--class",
            "fact",
            "--subject",
            "file entry",
            "--source",
            "fixture",
            "--stated",
            "2026-10-21",
            "--local",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), from_file.status.code());
    assert_eq!(
        "added fact/2026-10-21-file-entry\n",
        String::from_utf8(from_file.stdout).unwrap()
    );
    assert!(from_file.stderr.is_empty());
    let original_path = root.join(".dunkingsheep/fact/2026-10-21-file-entry.md");
    let original = std::fs::read_to_string(&original_path).unwrap();
    assert!(original.ends_with("file first\nfile second\nfile third\n"));

    let from_stdin = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "--class",
            "provisional",
            "--subject",
            "stdin entry",
            "--stated",
            "2026-10-22",
            "--recheck-after",
            "5d",
            "--local",
        ],
        Some(b"stdin first\r\nstdin second\rstdin third"),
        None,
    );
    assert_eq!(Some(0), from_stdin.status.code());
    assert_eq!(
        "added provisional/2026-10-22-stdin-entry\n",
        String::from_utf8(from_stdin.stdout).unwrap()
    );
    assert!(from_stdin.stderr.is_empty());
    let stdin_article =
        std::fs::read_to_string(root.join(".dunkingsheep/provisional/2026-10-22-stdin-entry.md"))
            .unwrap();
    assert!(stdin_article.ends_with("stdin first\nstdin second\nstdin third\n"));

    let global = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "--local",
            "--global",
            "--class",
            "fact",
            "--subject",
            "global entry",
            "--text",
            "global body",
            "--stated",
            "2026-10-23",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), global.status.code());
    assert_eq!(
        "added fact/2026-10-23-global-entry\n",
        String::from_utf8(global.stdout).unwrap()
    );
    assert!(global.stderr.is_empty());
    let global_article_path = global_dir.join("context/fact/2026-10-23-global-entry.md");
    assert!(global_article_path.is_file());

    let missing = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "missing.md",
            "--class",
            "fact",
            "--subject",
            "missing",
            "--local",
        ],
        None,
        None,
    );
    assert_eq!(Some(2), missing.status.code());
    assert!(missing.stdout.is_empty());
    assert_eq!(
        "no such file: 'missing.md' - the body comes from --text, stdin or a file; for text inline use: dunkingsheep context add --text '<claim>' --class ... --subject ...\n",
        String::from_utf8(missing.stderr).unwrap()
    );
    assert!(!root
        .join(".dunkingsheep/fact/2026-10-22-missing.md")
        .exists());

    let refused = run_local_cli(
        &socket,
        root,
        &global_dir,
        &["context", "promote", "2026-10-21-file-entry", "--local"],
        None,
        None,
    );
    assert_eq!(Some(2), refused.status.code());
    assert!(refused.stdout.is_empty());
    assert_eq!(
        "refused: local context promote cannot verify the bound owner role; omit --local to use the daemon\n",
        String::from_utf8(refused.stderr).unwrap()
    );
    assert_eq!(original, std::fs::read_to_string(&original_path).unwrap());
    let ground_truth = root.join(".dunkingsheep/ground-truth");
    assert!(std::fs::read_dir(&ground_truth)
        .map(|entries| entries.count() == 0)
        .unwrap_or(!ground_truth.exists()));

    let protected_add = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "--text",
            "must not be written",
            "--class",
            "fact",
            "--subject",
            "protected local entry",
            "--type",
            "rule",
            "--stated",
            "2026-10-24",
            "--local",
        ],
        None,
        Some("1"),
    );
    assert_eq!(Some(2), protected_add.status.code());
    assert!(protected_add.stdout.is_empty());
    assert_eq!(
        "refused: local context add cannot verify the bound owner role for protected entries; omit --local to use the daemon\n",
        String::from_utf8(protected_add.stderr).unwrap()
    );
    assert!(!root
        .join(".dunkingsheep/fact/2026-10-24-protected-local-entry.md")
        .exists());

    let absent = run_local_cli(
        &socket,
        root,
        &global_dir,
        &["context", "promote", "does-not-exist", "--local"],
        None,
        Some("1"),
    );
    assert_eq!(Some(1), absent.status.code());
    assert!(absent.stdout.is_empty());
    assert!(absent.stderr.is_empty());

    let promoted = run_local_cli(
        &socket,
        root,
        &global_dir,
        &["context", "promote", "2026-10-21-file-entry", "--local"],
        None,
        Some("1"),
    );
    assert_eq!(Some(2), promoted.status.code());
    assert!(promoted.stdout.is_empty());
    assert_eq!(
        "refused: local context promote cannot verify the bound owner role; omit --local to use the daemon\n",
        String::from_utf8(promoted.stderr).unwrap()
    );
    let superseded = std::fs::read_to_string(original_path).unwrap();
    assert_eq!(original, superseded);
    assert!(std::fs::read_dir(&ground_truth)
        .map(|entries| entries.count() == 0)
        .unwrap_or(!ground_truth.exists()));

    let global_promoted = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "promote",
            "2026-10-23-global-entry",
            "--local",
            "--global",
        ],
        None,
        Some("1"),
    );
    assert_eq!(Some(2), global_promoted.status.code());
    assert!(global_promoted.stdout.is_empty());
    assert_eq!(
        "refused: local context promote cannot verify the bound owner role; omit --local to use the daemon\n",
        String::from_utf8(global_promoted.stderr).unwrap()
    );
    let global_truth = global_dir.join("context/ground-truth");
    assert!(std::fs::read_dir(&global_truth)
        .map(|entries| entries.count() == 0)
        .unwrap_or(!global_truth.exists()));
}

#[test]
fn cli_route_12_context_local_write_b() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let global_dir = root.join("isolated-global");
    let socket = root.join("unused-daemon.sock");
    let body_file = root.join("replacement.md");
    std::fs::write(
        &body_file,
        b"replacement first\r\nreplacement second\rthird",
    )
    .unwrap();

    let seed = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "--local",
            "--text",
            "old body",
            "--class",
            "fact",
            "--subject",
            "file replacement",
            "--stated",
            "2026-01-01",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), seed.status.code());
    assert_eq!(
        "added fact/2026-01-01-file-replacement\n",
        String::from_utf8(seed.stdout).unwrap()
    );
    assert!(seed.stderr.is_empty());

    let superseded = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "supersede",
            "2026-01-01-file-replacement",
            body_file.to_str().unwrap(),
            "--local",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), superseded.status.code());
    assert!(superseded.stderr.is_empty());
    let superseded_text = String::from_utf8(superseded.stdout).unwrap();
    let replacement_slug = superseded_text
        .strip_prefix("superseded by ")
        .and_then(|text| text.strip_suffix('\n'))
        .unwrap();
    let replacement_path = root
        .join(".dunkingsheep/fact")
        .join(format!("{replacement_slug}.md"));
    let replacement = std::fs::read_to_string(&replacement_path).unwrap();
    assert!(replacement.contains("subject: file replacement\n"));
    assert!(replacement.contains("supersedes: 2026-01-01-file-replacement\n"));
    assert!(replacement.ends_with("replacement first\nreplacement second\nthird\n"));
    let old =
        std::fs::read_to_string(root.join(".dunkingsheep/fact/2026-01-01-file-replacement.md"))
            .unwrap();
    assert!(old.contains(&format!("superseded_by: {replacement_slug}\n")));

    let protected = root.join(".dunkingsheep/ground-truth/2026-01-03-protected-rule.md");
    std::fs::create_dir_all(protected.parent().unwrap()).unwrap();
    let protected_contents = "---\nclass: ground-truth\nsubject: protected rule\ntype: principle\nscope: project\nauthority: owner\nstated: 2026-01-03\nsource: fixture\n---\n\nprotected body\n";
    std::fs::write(&protected, protected_contents).unwrap();
    let protected_supersede = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "supersede",
            "2026-01-03-protected-rule",
            body_file.to_str().unwrap(),
            "--local",
        ],
        None,
        Some("1"),
    );
    assert_eq!(Some(2), protected_supersede.status.code());
    assert!(protected_supersede.stdout.is_empty());
    assert_eq!(
        "refused: local context supersede cannot verify the bound owner role for protected entries; omit --local to use the daemon\n",
        String::from_utf8(protected_supersede.stderr).unwrap()
    );
    assert_eq!(
        protected_contents,
        std::fs::read_to_string(&protected).unwrap()
    );
    assert_eq!(
        1,
        std::fs::read_dir(protected.parent().unwrap())
            .unwrap()
            .count()
    );

    let stdin_seed = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "add",
            "--local",
            "--text",
            "stdin old",
            "--class",
            "fact",
            "--subject",
            "stdin replacement",
            "--stated",
            "2026-01-02",
        ],
        None,
        None,
    );
    assert_eq!(Some(0), stdin_seed.status.code());
    assert_eq!(
        "added fact/2026-01-02-stdin-replacement\n",
        String::from_utf8(stdin_seed.stdout).unwrap()
    );
    assert!(stdin_seed.stderr.is_empty());
    let stdin_superseded = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "supersede",
            "2026-01-02-stdin-replacement",
            "--local",
        ],
        Some(b"stdin first\r\nstdin second\rthird"),
        None,
    );
    assert_eq!(Some(0), stdin_superseded.status.code());
    assert!(stdin_superseded.stderr.is_empty());
    let stdin_slug = String::from_utf8(stdin_superseded.stdout)
        .unwrap()
        .strip_prefix("superseded by ")
        .unwrap()
        .trim()
        .to_string();
    let stdin_article = std::fs::read_to_string(
        root.join(".dunkingsheep/fact")
            .join(format!("{stdin_slug}.md")),
    )
    .unwrap();
    assert!(stdin_article.ends_with("stdin first\nstdin second\nthird\n"));

    let retired = run_local_cli(
        &socket,
        root,
        &global_dir,
        &[
            "context",
            "retire",
            replacement_slug,
            "--reason",
            "obsolete note",
            "--local",
            "--json",
        ],
        None,
        Some("1"),
    );
    assert_eq!(Some(2), retired.status.code());
    assert!(retired.stdout.is_empty());
    assert_eq!(
        "refused: local context retire cannot verify the bound owner role; omit --local to use the daemon\n",
        String::from_utf8(retired.stderr).unwrap()
    );
    let retired_article = std::fs::read_to_string(replacement_path).unwrap();
    assert_eq!(replacement, retired_article);
    assert!(
        !socket.exists(),
        "local context writes must not create or contact a daemon socket"
    );

    let missing = run_local_cli(
        &socket,
        root,
        &global_dir,
        &["context", "supersede", "missing-entry", "--local"],
        Some(b"unused body"),
        None,
    );
    assert_eq!(Some(2), missing.status.code());
    assert!(missing.stdout.is_empty());
    assert_eq!(
        "context: entry missing-entry not found\n",
        String::from_utf8(missing.stderr).unwrap()
    );
}
#[test]
fn cli_route_27_state() {
    let captured = run_daemon_cli(
        &["state", "capture"],
        "state_capture",
        json!({"source": "periodic"}),
        json!({"captured": true, "source": "periodic"}),
    );
    assert_eq!(0, captured.status.code().unwrap());
    assert_eq!(
        "{\n  \"captured\": true,\n  \"source\": \"periodic\"\n}\n",
        String::from_utf8(captured.stdout).unwrap()
    );
    assert!(captured.stderr.is_empty());

    let latest = run_daemon_cli(
        &[
            "state",
            "latest",
            "%14",
            "--project",
            "demo",
            "--limit",
            "7",
        ],
        "state_latest",
        json!({"pane_id": "%14", "project": "demo", "limit": 7}),
        json!({"rows": [{"pane_id": "%14"}], "count": 1}),
    );
    assert_eq!(0, latest.status.code().unwrap());
    assert_eq!(
        "{\n  \"rows\": [\n    {\n      \"pane_id\": \"%14\"\n    }\n  ],\n  \"count\": 1\n}\n",
        String::from_utf8(latest.stdout).unwrap()
    );
    assert!(latest.stderr.is_empty());
}

#[test]
fn cli_route_28_views_a() {
    let roster = run_daemon_cli(
        &["roster", "--json"],
        "roster",
        json!({}),
        json!({"lanes": [{"id": "cli"}]}),
    );
    assert_eq!(Some(0), roster.status.code());
    assert_eq!(
        "{\n  \"lanes\": [\n    {\n      \"id\": \"cli\"\n    }\n  ]\n}\n",
        String::from_utf8(roster.stdout).unwrap()
    );
    assert!(roster.stderr.is_empty());

    let message_state = run_daemon_cli(
        &["message-state", "i1,i2", "--json"],
        "message_state",
        json!({"message_ids": "i1,i2"}),
        json!({"messages": [{"id": "i1", "state": "unread"}]}),
    );
    assert_eq!(Some(0), message_state.status.code());
    assert_eq!("{\n  \"messages\": [\n    {\n      \"id\": \"i1\",\n      \"state\": \"unread\"\n    }\n  ]\n}\n", String::from_utf8(message_state.stdout).unwrap());
    assert!(message_state.stderr.is_empty());

    let reply_stats_default = run_daemon_cli(
        &["reply-stats", "--json"],
        "reply_stats",
        json!({}),
        json!({"total": 3}),
    );
    assert_eq!(Some(0), reply_stats_default.status.code());
    assert_eq!(
        "{\n  \"total\": 3\n}\n",
        String::from_utf8(reply_stats_default.stdout).unwrap()
    );
    assert!(reply_stats_default.stderr.is_empty());

    let reply_stats_boxes = run_daemon_cli(
        &["reply-stats", "reviewer,codex", "--json"],
        "reply_stats",
        json!({"mailboxes": "reviewer,codex"}),
        json!({"total": 2}),
    );
    assert_eq!(Some(0), reply_stats_boxes.status.code());
    assert_eq!(
        "{\n  \"total\": 2\n}\n",
        String::from_utf8(reply_stats_boxes.stdout).unwrap()
    );
    assert!(reply_stats_boxes.stderr.is_empty());
}

#[test]
fn cli_route_29_views_b() {
    let summary_default = run_daemon_cli(
        &["project-summary", "--json"],
        "project_summary",
        json!({"project": null}),
        json!({"project": null, "count": 0}),
    );
    assert_eq!(Some(0), summary_default.status.code());
    assert_eq!(
        "{\n  \"project\": null,\n  \"count\": 0\n}\n",
        String::from_utf8(summary_default.stdout).unwrap()
    );
    assert!(summary_default.stderr.is_empty());

    let summary_path = run_daemon_cli(
        &[
            "project-summary",
            "demo",
            "--path",
            "/tmp/project",
            "--json",
        ],
        "project_summary",
        json!({"project": "demo", "path": "/tmp/project"}),
        json!({"project": "demo", "path": "/tmp/project", "count": 4}),
    );
    assert_eq!(Some(0), summary_path.status.code());
    assert_eq!(
        "{\n  \"project\": \"demo\",\n  \"path\": \"/tmp/project\",\n  \"count\": 4\n}\n",
        String::from_utf8(summary_path.stdout).unwrap()
    );
    assert!(summary_path.stderr.is_empty());
}

#[test]
fn cli_route_30_poolrad() {
    let sync = run_daemon_cli(
        &["poolrad", "backlog-sync"],
        "poolrad_sync",
        json!({"limit": 6, "dry_run": false}),
        json!({
            "base": "main",
            "skipped": {"queue_rows": 2, "credited": 1, "already_filed": 0},
            "filed": [{"task": "t901", "id": "d7", "rank": 2}],
            "errors": ["conflict"]
        }),
    );
    assert_eq!(Some(0), sync.status.code());
    assert_eq!(
        "base main; queue rows 2, credited 1, already filed 0\n  filed t901 d7 rank 2\n  ! conflict\n",
        String::from_utf8(sync.stdout).unwrap()
    );
    assert!(sync.stderr.is_empty());

    let dry_run = run_daemon_cli(
        &["poolrad", "backlog-sync", "-n", "3", "--dry-run"],
        "poolrad_sync",
        json!({"limit": 3, "dry_run": true}),
        json!({
            "base": null,
            "skipped": {"queue_rows": 1, "credited": 0, "already_filed": 0},
            "filed": [{"id": "d8", "rank": 1}],
            "errors": []
        }),
    );
    assert_eq!(Some(0), dry_run.status.code());
    assert_eq!(
        "base ?; queue rows 1, credited 0, already filed 0\n  filed (dry) d8 rank 1\n",
        String::from_utf8(dry_run.stdout).unwrap()
    );
    assert!(dry_run.stderr.is_empty());

    let progress = run_daemon_cli(
        &["poolrad", "progress"],
        "poolrad_progress",
        json!({}),
        json!({
            "credited": 10,
            "identities": 4,
            "ceiling": 20,
            "ceiling_note": "strict body ceiling",
            "composable_bodies": 6,
            "queue_candidates": 8,
            "main": "main",
            "replaced": null,
            "replaced_note": "no replacement source"
        }),
    );
    assert_eq!(Some(0), progress.status.code());
    assert_eq!(
        "credited strict bodies: 10  identities: 4  CEILING 20\n  strict body ceiling\n  composable in queue: 6 of 8 candidates; main main\n  replaced: n/a (no replacement source)\n",
        String::from_utf8(progress.stdout).unwrap()
    );
    assert!(progress.stderr.is_empty());

    let progress_json = run_daemon_cli(
        &["poolrad", "progress", "--json"],
        "poolrad_progress",
        json!({}),
        json!({"credited": 10, "identities": 4}),
    );
    assert_eq!(Some(0), progress_json.status.code());
    assert_eq!(
        "{\n  \"credited\": 10,\n  \"identities\": 4\n}\n",
        String::from_utf8(progress_json.stdout).unwrap()
    );
    assert!(progress_json.stderr.is_empty());
}

#[test]
fn cli_route_31_review() {
    let sync = run_daemon_cli(
        &["review", "sync"],
        "review_sync",
        json!({"dry_run": false}),
        json!({
            "repos": [{"repo": "a/b", "open": 2}],
            "filed": [{"repo": "a/b", "pr": 44, "head": "123456789012abc", "author": null}],
            "cancelled": ["t1"],
            "statuses": [{"repo": "a/b", "head": "abcdef", "state": "ok"}],
            "errors": ["issue"]
        }),
    );
    assert_eq!(Some(0), sync.status.code());
    assert_eq!(
        "a/b: 2 open PR(s)\n  filed (dry) a/b#44 @ 123456789012 author ?\n  cancelled t1\n  status a/b@abcdef ok\n  ! issue\n",
        String::from_utf8(sync.stdout).unwrap()
    );
    assert!(sync.stderr.is_empty());

    let sync_dry_run = run_daemon_cli(
        &["review", "sync", "--dry-run"],
        "review_sync",
        json!({"dry_run": true}),
        json!({"repos": [], "filed": [], "cancelled": [], "statuses": [], "errors": []}),
    );
    assert_eq!(Some(0), sync_dry_run.status.code());
    assert_eq!(
        "  nothing to do\n",
        String::from_utf8(sync_dry_run.stdout).unwrap()
    );
    assert!(sync_dry_run.stderr.is_empty());

    let review_list = run_daemon_cli(
        &["review", "list"],
        "review_list",
        json!({}),
        json!({
            "reviews": [{
                "task": "t12",
                "status": "open",
                "repo": "x/y",
                "pr": 7,
                "head": "abcdef0123456789",
                "author": null,
                "reviewer": ["revA", "revB"],
                "age_s": 125
            }]
        }),
    );
    assert_eq!(Some(0), review_list.status.code());
    assert_eq!(
        "t12    open        x/y#7 @ abcdef0123456789 author ?        reviewer revA,revB  age 2m\n",
        String::from_utf8(review_list.stdout).unwrap()
    );
    assert!(review_list.stderr.is_empty());

    let review_list_json = run_daemon_cli(
        &["review", "list", "--json"],
        "review_list",
        json!({}),
        json!({"reviews": []}),
    );
    assert_eq!(Some(0), review_list_json.status.code());
    assert_eq!(
        "{\n  \"reviews\": []\n}\n",
        String::from_utf8(review_list_json.stdout).unwrap()
    );
    assert!(review_list_json.stderr.is_empty());
}

#[test]
fn cli_route_09_inbox_write_b() {
    let archived = run_daemon_cli(
        &[
            "inbox",
            "archive",
            "reviewer",
            "--before",
            "2026-10-01",
            "--state",
            "read,acked",
        ],
        "inbox_archive",
        json!({
            "mailbox": "reviewer",
            "before": "2026-10-01",
            "state": "read,acked"
        }),
        json!({
            "mailbox": "reviewer",
            "archived": 2,
            "states": ["read", "acked"],
            "before": "2026-10-01"
        }),
    );
    assert_eq!(0, archived.status.code().unwrap());
    assert_eq!(
        "archived 2 message(s) in reviewer (states read,acked before 2026-10-01)\n",
        String::from_utf8(archived.stdout).unwrap()
    );
    assert!(archived.stderr.is_empty());

    let stats = run_daemon_cli(
        &["inbox", "stats"],
        "inbox_stats",
        json!({}),
        json!({"mailboxes": []}),
    );
    assert_eq!(0, stats.status.code().unwrap());
    assert_eq!("no mailboxes\n", String::from_utf8(stats.stdout).unwrap());
    assert!(stats.stderr.is_empty());

    let dest = tempfile::tempdir().unwrap();
    let dest_path = dest.path().to_str().unwrap();
    let fetched = run_daemon_cli(
        &["inbox", "fetch", "attachment-1", "--dest", dest_path],
        "inbox_fetch",
        json!({"attachment_id": "attachment-1", "dest": dest_path}),
        json!({"path": "/sandbox/attachment.txt", "size": 12}),
    );
    assert_eq!(0, fetched.status.code().unwrap());
    assert_eq!(
        "wrote /sandbox/attachment.txt (12 bytes)\n",
        String::from_utf8(fetched.stdout).unwrap()
    );
    assert!(fetched.stderr.is_empty());
}

#[test]
fn cli_route_13_load_report() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let config = temp.path().join("config");
    let policy_file = temp.path().join("policies.json");
    let state_file = temp.path().join("load-state.json");
    std::fs::write(&policy_file, "not json").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        for _ in 0..4 {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if request["cmd"] == "status" {
                json!({})
            } else {
                assert_eq!("task_list", request["cmd"]);
                assert_eq!(json!({"limit": 500}), request["args"]);
                json!({"tasks": [{"caller_pid": 2147483647_i64,
                    "caller_start": 10, "caller_name": "fixture"}]})
            };
            let response = json!({"ok": true, "result": result});
            let mut writer = &stream;
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
    });

    let run_load = |json_output: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
        command
            .args(["--socket", &socket_text, "--self-pane", "%99", "load"])
            .args(["--policies", policy_file.to_str().unwrap()])
            .env("HOME", temp.path())
            .env("DUNKINGSHEEP_DIR", &config)
            .env("DUNKINGSHEEP_LOAD_STATE", &state_file)
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
        if json_output {
            command.arg("--json");
        }
        command.output().expect("run Rust CLI load")
    };

    let human = run_load(false);
    assert_eq!(Some(0), human.status.code());
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(human_stdout.starts_with("window Nones | host load1 "));
    assert!(human_stdout.contains("fixture"));
    assert!(human_stdout.contains("skipped: process gone\n"));
    assert!(human.stderr.is_empty());

    let raw = run_load(true);
    assert_eq!(Some(0), raw.status.code());
    assert!(raw.stderr.is_empty());
    let report: Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!("fixture", report["skipped"][0]["name"]);
    assert_eq!("process gone", report["skipped"][0]["reason"]);
    assert_eq!(json!([]), report["would_kill"]);
    let state: Value = serde_json::from_slice(&std::fs::read(&state_file).unwrap()).unwrap();
    assert!(state.get("at").is_some());
    assert_eq!(json!({}), state["callers"]);
    daemon.join().unwrap();
}

#[test]
fn cli_route_15_allocation() {
    let status = run_daemon_cli(
        &["allocation", "status"],
        "allocation",
        json!({"action": "status"}),
        json!({"workers": [], "offers": []}),
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        "{\n  \"workers\": [],\n  \"offers\": []\n}\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());

    let registered = run_daemon_cli(
        &[
            "--json",
            "allocation",
            "register",
            "--pane-id",
            "%42",
            "--pool",
            "compute",
            "--pools",
            "cpu",
            "--pools",
            "gpu",
            "--roles",
            "worker",
            "--roles",
            "reviewer",
            "--skills",
            "rust",
            "--skills",
            "cli",
            "--requires",
            "linux",
            "--task-id",
            "t8",
            "--revision",
            "main",
            "--body",
            "checked body",
            "--disable",
        ],
        "allocation",
        json!({
            "action": "register",
            "pane_id": "%42",
            "pool": "compute",
            "pools": ["cpu", "gpu"],
            "roles": ["worker", "reviewer"],
            "skills": ["rust", "cli"],
            "requires": ["linux"],
            "task_id": "t8",
            "revision": "main",
            "body": "checked body",
            "enabled": false
        }),
        json!({"action": "registered", "pane_id": "%42"}),
    );
    assert_eq!(Some(0), registered.status.code());
    assert_eq!(
        "{\n  \"action\": \"registered\",\n  \"pane_id\": \"%42\"\n}\n",
        String::from_utf8(registered.stdout).unwrap()
    );
    assert!(registered.stderr.is_empty());
}

#[test]
fn cli_route_26_reports() {
    let prose_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(prose_file.path(), "file prose\n").unwrap();
    let prose_path = prose_file.path().to_str().unwrap();
    let ran = run_daemon_cli(
        &[
            "report",
            "run",
            "--no-post",
            "--prose",
            "ignored",
            "--prose-file",
            prose_path,
            "--out-dir",
            "/reports",
        ],
        "report_run",
        json!({"post": false, "prose": "file prose\n", "out_dir": "/reports"}),
        json!({
            "path": "/reports/2026-11-01.md",
            "generated_at": "2026-11-01T12:00:00-08:00",
            "posted": {"message_id": "i42", "transport": "socket"},
            "prose_rejected": ["unknown task"]
        }),
    );
    assert_eq!(0, ran.status.code().unwrap());
    assert_eq!(
        "report /reports/2026-11-01.md  (generated 2026-11-01T12:00:00-08:00)\nposted to the owner mailbox as i42 (transport socket)\nprose rejected: unknown task\n",
        String::from_utf8(ran.stdout).unwrap()
    );
    assert!(ran.stderr.is_empty());

    let listed = run_daemon_cli(
        &["report", "list"],
        "report_list",
        json!({"out_dir": null}),
        json!({"count": 0, "dir": "/reports", "reports": []}),
    );
    assert_eq!(0, listed.status.code().unwrap());
    assert_eq!(
        "0 report(s) in /reports\n",
        String::from_utf8(listed.stdout).unwrap()
    );
    assert!(listed.stderr.is_empty());
}

#[test]
fn cli_route_32_token() {
    let issued = run_daemon_cli(
        &["token", "issue", "--role", "owner"],
        "token_issue",
        json!({"role": "owner", "label": null}),
        json!({"token": "single-use-secret", "role": "owner", "label": null}),
    );
    assert_eq!(Some(0), issued.status.code());
    assert_eq!(
        "single-use-secret\n",
        String::from_utf8(issued.stdout).unwrap()
    );
    assert!(issued.stderr.is_empty());

    let issued_json = run_daemon_cli(
        &[
            "token", "issue", "--role", "web", "--label", "web app", "--json",
        ],
        "token_issue",
        json!({"role": "web", "label": "web app"}),
        json!({"token": "json-secret", "role": "web", "label": "web app"}),
    );
    assert_eq!(Some(0), issued_json.status.code());
    assert_eq!(
        "{\n  \"token\": \"json-secret\",\n  \"role\": \"web\",\n  \"label\": \"web app\"\n}\n",
        String::from_utf8(issued_json.stdout).unwrap()
    );
    assert!(issued_json.stderr.is_empty());

    let revoked = run_daemon_cli(
        &["token", "revoke", "expired-web"],
        "token_revoke",
        json!({"label": "expired-web"}),
        json!({"revoked": 2, "label": "expired-web"}),
    );
    assert_eq!(Some(0), revoked.status.code());
    assert_eq!(
        "revoked 2 token(s) labelled expired-web\n",
        String::from_utf8(revoked.stdout).unwrap()
    );
    assert!(revoked.stderr.is_empty());

    let revoked_json = run_daemon_cli(
        &["token", "revoke", "missing", "--json"],
        "token_revoke",
        json!({"label": "missing"}),
        json!({"revoked": 0, "label": "missing"}),
    );
    assert_eq!(Some(0), revoked_json.status.code());
    assert_eq!(
        "{\n  \"revoked\": 0,\n  \"label\": \"missing\"\n}\n",
        String::from_utf8(revoked_json.stdout).unwrap()
    );
    assert!(revoked_json.stderr.is_empty());

    let listed = run_daemon_cli(
        &["token", "list"],
        "superuser_status",
        json!({}),
        json!({
            "tokens": [
                {"label": "web desk", "role": "web"},
                {"label": "owner", "role": "owner"}
            ]
        }),
    );
    assert_eq!(Some(0), listed.status.code());
    assert_eq!(
        format!(
            "{:<24} {}\n{:<24} {}\n",
            "web desk", "web", "owner", "owner"
        ),
        String::from_utf8(listed.stdout).unwrap()
    );
    assert!(listed.stderr.is_empty());

    let empty = run_daemon_cli(
        &["token", "list"],
        "superuser_status",
        json!({}),
        json!({"tokens": []}),
    );
    assert_eq!(Some(0), empty.status.code());
    assert_eq!("no tokens\n", String::from_utf8(empty.stdout).unwrap());
    assert!(empty.stderr.is_empty());

    let listed_json = run_daemon_cli(
        &["token", "list", "--json"],
        "superuser_status",
        json!({}),
        json!({"tokens": [{"label": "system", "role": "system"}]}),
    );
    assert_eq!(Some(0), listed_json.status.code());
    assert_eq!(
        "{\n  \"tokens\": [\n    {\n      \"label\": \"system\",\n      \"role\": \"system\"\n    }\n  ]\n}\n",
        String::from_utf8(listed_json.stdout).unwrap()
    );
    assert!(listed_json.stderr.is_empty());
}

#[test]
fn cli_route_10_context_local_seed() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("work");
    let config = temp.path().join("config");
    let store = temp.path().join("project-store");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&config).unwrap();

    let empty_path_init = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".to_string(),
            "init".to_string(),
            "--path".to_string(),
            String::new(),
        ],
    );
    assert_eq!(Some(0), empty_path_init.status.code());
    assert_eq!(
        "store at .dunkingsheep\n",
        String::from_utf8(empty_path_init.stdout).unwrap()
    );
    assert!(empty_path_init.stderr.is_empty());
    let default_store = cwd.join(".dunkingsheep");
    assert!(default_store.join("ground-truth").is_dir());
    assert!(default_store.join("fact").is_dir());
    assert!(!cwd.join("ground-truth").exists());
    assert!(!cwd.join("fact").exists());

    let init = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".into(),
            "init".into(),
            "--path".into(),
            store.display().to_string(),
        ],
    );
    assert_eq!(Some(0), init.status.code());
    assert_eq!(
        format!("store at {}\n", store.display()),
        String::from_utf8(init.stdout).unwrap()
    );
    assert!(init.stderr.is_empty());
    for class in ["ground-truth", "fact", "history", "provisional"] {
        assert!(
            store.join(class).is_dir(),
            "missing class directory {class}"
        );
    }
    assert!(store.join("README.md").is_file());

    let init_json = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".into(),
            "init".into(),
            "--path".into(),
            store.display().to_string(),
            "--json".into(),
        ],
    );
    assert_eq!(Some(0), init_json.status.code());
    assert_eq!(
        format!("{{\n  \"root\": \"{}\"\n}}\n", store.display()),
        String::from_utf8(init_json.stdout).unwrap()
    );
    assert!(init_json.stderr.is_empty());

    let entry = store.join("fact").join("fixture.md");
    std::fs::write(
        &entry,
        "---\nclass: fact\nsubject: fixture\ntype: reference\nscope: project\nauthority: agent\nsource: test\nstated: 2026-11-01\n---\nFixture body.\n",
    )
    .unwrap();
    let ledger = config.join("context-ledger.db");
    let refused = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".into(),
            "ledger".into(),
            "adopt".into(),
            "--path".into(),
            store.display().to_string(),
        ],
    );
    assert_eq!(Some(1), refused.status.code());
    assert!(refused.stdout.is_empty());
    assert_eq!(
        "refused: `context ledger adopt` is the owner's bootstrap; run it with DUNKINGSHEEP_SUPERUSER=1 from the owner's terminal\n",
        String::from_utf8(refused.stderr).unwrap()
    );
    assert!(
        !ledger.exists(),
        "unauthorized adopt must not create the ledger"
    );

    let failing_ledger = config.join("ledger-is-a-directory");
    std::fs::create_dir(&failing_ledger).unwrap();
    let failed_adopt = run_local_context_cli_with_ledger(
        &cwd,
        &config,
        "1",
        &[
            "context".into(),
            "ledger".into(),
            "adopt".into(),
            "--path".into(),
            store.display().to_string(),
        ],
        Some(&failing_ledger),
    );
    assert_ne!(Some(0), failed_adopt.status.code());
    assert!(failed_adopt.stdout.is_empty());
    let error = String::from_utf8(failed_adopt.stderr).unwrap();
    assert!(!error.trim().is_empty());
    assert!(!error.contains("adopted"));

    let empty_path_snapshot = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".to_string(),
            "snapshot".to_string(),
            "--path".to_string(),
            String::new(),
        ],
    );
    assert_eq!(Some(0), empty_path_snapshot.status.code());
    assert!(empty_path_snapshot.stderr.is_empty());
    let empty_path_snapshot: Value = serde_json::from_slice(&empty_path_snapshot.stdout).unwrap();
    assert_eq!(
        ".dunkingsheep",
        empty_path_snapshot["stores"]["project"]["path"]
    );

    let snapshot = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".into(),
            "snapshot".into(),
            "--project".into(),
            "--path".into(),
            store.display().to_string(),
        ],
    );
    assert_eq!(Some(0), snapshot.status.code());
    assert!(snapshot.stderr.is_empty());
    let snapshot: Value = serde_json::from_slice(&snapshot.stdout).unwrap();
    assert_eq!("context-snapshot/1", snapshot["schema"]);
    assert!(snapshot["as_of"].as_u64().unwrap() > 0);
    assert!(snapshot["digest"]
        .as_str()
        .unwrap()
        .contains("Fixture body."));
    assert_eq!("ok", snapshot["stores"]["project"]["status"]);
    assert_eq!(
        store.display().to_string(),
        snapshot["stores"]["project"]["path"]
    );
    assert_eq!(1, snapshot["stores"]["project"]["counts"]["fact"]);
    assert_eq!(
        1,
        snapshot["stores"]["project"]["entries"]
            .as_array()
            .unwrap()
            .len()
    );

    let adopted = run_local_context_cli(
        &cwd,
        &config,
        "1",
        &[
            "context".into(),
            "ledger".into(),
            "adopt".into(),
            "--path".into(),
            store.display().to_string(),
        ],
    );
    assert_eq!(Some(0), adopted.status.code());
    assert_eq!(
        format!("adopted 1 entries into {}\n", ledger.display()),
        String::from_utf8(adopted.stdout).unwrap()
    );
    assert!(adopted.stderr.is_empty());
    assert!(ledger.is_file());

    let status = run_local_context_cli(
        &cwd,
        &config,
        "0",
        &[
            "context".into(),
            "ledger".into(),
            "status".into(),
            "--path".into(),
            store.display().to_string(),
        ],
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        format!("{:11} {:13} {}\n", "recorded", "fact", "fixture"),
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());
}

#[test]
fn cli_route_33_superuser_a() {
    let status = run_daemon_cli(
        &["superuser", "status"],
        "superuser_status",
        json!({}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [{"label": "owner", "role": "owner"}]
        }),
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  tokens: owner=owner\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());

    let added = run_daemon_cli(
        &["superuser", "add", "worker@example"],
        "superuser_add",
        json!({"entry": "worker@example"}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [],
            "added": "worker@example"
        }),
    );
    assert_eq!(Some(0), added.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  added: worker@example\n",
        String::from_utf8(added.stdout).unwrap()
    );
    assert!(added.stderr.is_empty());

    let removed_json = run_daemon_cli(
        &["superuser", "remove", "worker@example", "--json"],
        "superuser_remove",
        json!({"entry": "worker@example"}),
        json!({"removed": 1, "entry": "worker@example"}),
    );
    assert_eq!(Some(0), removed_json.status.code());
    assert_eq!(
        "{\n  \"removed\": 1,\n  \"entry\": \"worker@example\"\n}\n",
        String::from_utf8(removed_json.stdout).unwrap()
    );
    assert!(removed_json.stderr.is_empty());
}

#[test]
fn cli_route_36_memory_b() {
    let hook = run_daemon_cli(
        &[
            "memory",
            "hook-context",
            "--query",
            "start prompt",
            "--sessions",
            "2",
        ],
        "memory_context",
        json!({"query": "start prompt", "sessions": 2}),
        json!({"text": "prior session notes"}),
    );
    assert_eq!(Some(0), hook.status.code());
    assert_eq!(
        "{\"hookSpecificOutput\": {\"hookEventName\": \"SessionStart\", \"additionalContext\": \"Dunking Sheep memory - relevant past sessions for this workspace:\\n\\nprior session notes\"}}\n",
        String::from_utf8(hook.stdout).unwrap()
    );
    assert!(hook.stderr.is_empty());

    let cwd_name = std::env::current_dir()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let hook_json = run_daemon_cli(
        &["memory", "hook-context", "--query", "", "--json"],
        "memory_context",
        json!({"query": cwd_name, "sessions": 5}),
        json!({"text": ""}),
    );
    assert_eq!(Some(0), hook_json.status.code());
    assert_eq!(
        "{\n  \"text\": \"\"\n}\n",
        String::from_utf8(hook_json.stdout).unwrap()
    );
    assert!(hook_json.stderr.is_empty());

    let stats = run_daemon_cli(
        &["memory", "stats"],
        "memory_stats",
        json!({}),
        json!({
            "path": "/tmp/memory.db",
            "schema_version": 2,
            "ok": true,
            "sources": 4,
            "sessions": 3,
            "chunks": 9,
            "queue_depth": 0,
            "queue": "idle",
            "db_bytes": 100,
            "queries": {"count": 7, "p50": 1.25, "p95": 2.5, "p99": 3.5, "errors": 0},
            "harness": [{"harness": "codex", "chunks": 9, "sessions": 3}]
        }),
    );
    assert_eq!(Some(0), stats.status.code());
    assert_eq!(
        "path /tmp/memory.db  schema 2  ok True\nsources 4  sessions 3  chunks 9\nqueue depth 0  idle\ndb bytes 100\nqueries n=7 p50=1.25 p95=2.5 p99=3.5 errors=0\n  codex: 9 chunks / 3 sessions\n",
        String::from_utf8(stats.stdout).unwrap()
    );
    assert!(stats.stderr.is_empty());

    let status = run_daemon_cli(
        &["memory", "status", "--json"],
        "memory_status",
        json!({}),
        json!({"ok": true, "sources": 0}),
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        "{\n  \"ok\": true,\n  \"sources\": 0\n}\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());
}

#[test]
fn cli_route_34_superuser_b() {
    let status = run_daemon_cli(
        &["superuser", "status"],
        "superuser_status",
        json!({}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [{"label": "owner", "role": "owner"}]
        }),
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  tokens: owner=owner\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());

    let added = run_daemon_cli(
        &["superuser", "add", "worker@example"],
        "superuser_add",
        json!({"entry": "worker@example"}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [],
            "added": "worker@example"
        }),
    );
    assert_eq!(Some(0), added.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  added: worker@example\n",
        String::from_utf8(added.stdout).unwrap()
    );
    assert!(added.stderr.is_empty());

    let removed = run_daemon_cli(
        &["superuser", "remove", "worker@example"],
        "superuser_remove",
        json!({"entry": "worker@example"}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [],
            "removed": 1,
            "entry": "worker@example"
        }),
    );
    assert_eq!(Some(0), removed.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  removed: 1 (worker@example)\n",
        String::from_utf8(removed.stdout).unwrap()
    );
    assert!(removed.stderr.is_empty());

    let recovered = run_daemon_cli(
        &["superuser", "recover"],
        "superuser_recover",
        json!({"name": null}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [],
            "recovered": true,
            "entry": {"role": "owner"}
        }),
    );
    assert_eq!(Some(0), recovered.status.code());
    assert_eq!(
        "roles /tmp/roles.json\n  owner: yes (pane %99, role owner)\n  (no lanes registered; `lane register --role owner` bootstraps)\n  recovered: %99 (None)\n",
        String::from_utf8(recovered.stdout).unwrap()
    );
    assert!(recovered.stderr.is_empty());

    let named = run_daemon_cli(
        &[
            "superuser",
            "recover",
            "--name",
            "bootstrap owner",
            "--json",
        ],
        "superuser_recover",
        json!({"name": "bootstrap owner"}),
        json!({
            "path": "/tmp/roles.json",
            "superuser": true,
            "pane_id": "%99",
            "role": "owner",
            "lanes": {},
            "tokens": [],
            "recovered": true,
            "entry": {"role": "owner"}
        }),
    );
    assert_eq!(Some(0), named.status.code());
    assert_eq!(
        "{\n  \"path\": \"/tmp/roles.json\",\n  \"superuser\": true,\n  \"pane_id\": \"%99\",\n  \"role\": \"owner\",\n  \"lanes\": {},\n  \"tokens\": [],\n  \"recovered\": true,\n  \"entry\": {\n    \"role\": \"owner\"\n  }\n}\n",
        String::from_utf8(named.stdout).unwrap()
    );
    assert!(named.stderr.is_empty());
}

#[test]
fn cli_route_35_memory_a() {
    let recalled = run_daemon_cli(
        &[
            "memory",
            "recall",
            "CLI routes",
            "--limit",
            "2",
            "--harness",
            "codex",
            "--project",
            "dunkingsheep",
            "--since",
            "123.5",
        ],
        "memory_recall",
        json!({
            "query": "CLI routes",
            "limit": 2,
            "harness": "codex",
            "project": "dunkingsheep",
            "since": 123.5
        }),
        json!([{
            "score": 2.25,
            "harness": "codex",
            "project": "dunkingsheep",
            "session_id": "s42",
            "role": "assistant",
            "snippet": "first line\nsecond line"
        }]),
    );
    assert_eq!(Some(0), recalled.status.code());
    assert_eq!(
        format!(
            "{:.3} {:<8} {:<20} #{} [{}]\n  {}\n",
            2.25, "codex", "dunkingsheep", "s42", "assistant", "first line second line"
        ),
        String::from_utf8(recalled.stdout).unwrap()
    );
    assert!(recalled.stderr.is_empty());

    let searched = run_daemon_cli(
        &["memory", "search", "nothing", "--json"],
        "memory_search",
        json!({
            "query": "nothing",
            "limit": 10,
            "harness": null,
            "project": null,
            "since": null
        }),
        json!([]),
    );
    assert_eq!(Some(0), searched.status.code());
    assert_eq!("[]\n", String::from_utf8(searched.stdout).unwrap());
    assert!(searched.stderr.is_empty());

    let context = run_daemon_cli(
        &["memory", "context", "memory plan", "--sessions", "3"],
        "memory_context",
        json!({"query": "memory plan", "sessions": 3}),
        json!({
            "query": "memory plan",
            "counts": {"sessions": 1, "snippets": 1},
            "sessions": [{
                "harness": "codex",
                "project": "dunkingsheep",
                "session_id": "s7",
                "title": "Saved plan",
                "score": 1.25,
                "snippets": [{"snippet": "saved\nline"}]
            }]
        }),
    );
    assert_eq!(Some(0), context.status.code());
    assert_eq!(
        "query 'memory plan'  sessions 1  snippets 1\n- codex dunkingsheep #s7 Saved plan (score 1.250)\n    saved line\n",
        String::from_utf8(context.stdout).unwrap()
    );
    assert!(context.stderr.is_empty());
}

#[test]
fn cli_route_37_memory_c() {
    let ingested = run_daemon_cli(
        &[
            "memory",
            "ingest",
            "/tmp/session.jsonl",
            "--harness",
            "codex",
            "--project",
            "demo",
        ],
        "memory_ingest",
        json!({
            "path": "/tmp/session.jsonl",
            "harness": "codex",
            "project": "demo"
        }),
        json!({"path": "/tmp/session.jsonl", "harness": "codex"}),
    );
    assert_eq!(Some(0), ingested.status.code());
    assert_eq!(
        "queued /tmp/session.jsonl (codex)\n",
        String::from_utf8(ingested.stdout).unwrap()
    );
    assert!(ingested.stderr.is_empty());

    let started = run_daemon_cli(
        &["memory", "backfill", "--limit", "4"],
        "memory_backfill",
        json!({"action": "start", "limit": 4}),
        json!({
            "enqueued": 2,
            "discovered": 7,
            "paused": false,
            "status": {"ingestor": {"throttle_s": 15}}
        }),
    );
    assert_eq!(Some(0), started.status.code());
    assert_eq!(
        "queued 2 of 7 source(s); paused False; throttle 15\npath None  schema None  ok None\nsources None  sessions None  chunks None\nqueue depth None  None\ndb bytes None\nqueries n=None p50=None p95=None p99=None errors=None\ningestor {'throttle_s': 15}\n",
        String::from_utf8(started.stdout).unwrap()
    );
    assert!(started.stderr.is_empty());

    let paused = run_daemon_cli(
        &[
            "memory", "backfill", "--pause", "--resume", "--status", "--limit", "3", "--json",
        ],
        "memory_backfill",
        json!({"action": "pause", "limit": 3}),
        json!({"paused": true}),
    );
    assert_eq!(Some(0), paused.status.code());
    assert_eq!(
        "{\n  \"paused\": true\n}\n",
        String::from_utf8(paused.stdout).unwrap()
    );
    assert!(paused.stderr.is_empty());

    let resumed = run_daemon_cli(
        &["memory", "backfill", "--resume", "--status", "--json"],
        "memory_backfill",
        json!({"action": "resume", "limit": null}),
        json!({"resumed": true}),
    );
    assert_eq!(Some(0), resumed.status.code());
    assert_eq!(
        "{\n  \"resumed\": true\n}\n",
        String::from_utf8(resumed.stdout).unwrap()
    );
    assert!(resumed.stderr.is_empty());

    let status = run_daemon_cli(
        &["memory", "backfill", "--status", "--json"],
        "memory_backfill",
        json!({"action": "status", "limit": null}),
        json!({"status": "idle"}),
    );
    assert_eq!(Some(0), status.status.code());
    assert_eq!(
        "{\n  \"status\": \"idle\"\n}\n",
        String::from_utf8(status.stdout).unwrap()
    );
    assert!(status.stderr.is_empty());
}
