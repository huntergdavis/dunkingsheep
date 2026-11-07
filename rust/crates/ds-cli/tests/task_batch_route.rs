//! t3583: `dunkingsheep task batch <ids...>` sends the ordered id list to `task_batch_claim`
//! with the lease and busy flags, and renders the claim or the refusal. The fake daemon checks
//! the request it receives, so a wrong argument fails the test.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};

use serde_json::{json, Value};

/// Run the CLI against a fake daemon that expects `status`, then `rpc` with exactly `args`.
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
    let mut cli = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
    cli.args(["--socket", &socket_text, "--self-pane", "%99"])
        .args(command)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .env_remove("DUNKINGSHEEP_CLIENT_TIMEOUT")
        .env_remove("DUNKINGSHEEP_SEND_TIMEOUT_S");
    let output = cli.output().expect("run Rust CLI");
    daemon.join().unwrap();
    output
}

#[test]
fn task_batch_sends_the_ordered_ids_with_the_lease_and_busy_flags() {
    let claimed = run_daemon_cli(
        &["task", "batch", "t3", "t1", "--lease", "90", "--allow-busy"],
        "task_batch_claim",
        json!({"ids": ["t3", "t1"], "lease_seconds": 90.0, "allow_busy": true}),
        json!({
            "batch_id": 7,
            "tasks": [{"id": "t3"}, {"id": "t1"}],
            "pane_id": "%99",
            "lease_seconds": 90.0,
            "expires_at": 1.0,
            "caller": {},
        }),
    );
    assert_eq!(0, claimed.status.code().unwrap());
    assert_eq!(
        "claimed batch 7: t3 t1\n",
        String::from_utf8(claimed.stdout).unwrap()
    );
    assert!(claimed.stderr.is_empty());
}

#[test]
fn task_batch_renders_a_refusal_with_its_reason_and_the_members_at_fault() {
    let refused = run_daemon_cli(
        &["task", "batch", "t1", "t2"],
        "task_batch_claim",
        json!({"ids": ["t1", "t2"], "lease_seconds": null, "allow_busy": false}),
        json!({
            "batch_id": null,
            "tasks": [],
            "pane_id": "%99",
            "refused": {"reason": "batch_conflict", "task_ids": ["t2"]},
        }),
    );
    assert_eq!(0, refused.status.code().unwrap());
    assert_eq!(
        "refused (batch_conflict): t2\n",
        String::from_utf8(refused.stdout).unwrap()
    );
    assert!(refused.stderr.is_empty());
}
