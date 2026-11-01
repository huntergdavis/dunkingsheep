//! T44: `status` through argv -> parser -> dispatch -> a FAKE daemon: the printed text is what the
//! REAL Python `print_status` printed for the same payloads (4 cases: a full payload, an unknown
//! code identity with errors, a stale code with changed modules, an empty payload), and the CLI
//! sent `check_herdr` as the Python does (`--check-herdr` -> true, default false).
//! The `code_mtime` / `started_at` lines depend on the clock and the timezone; they are covered by
//! the printer differential (tests/printers_fleet.rs), not here.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::{json, Value};

fn fake_daemon(socket: &std::path::Path, result: Value) -> Arc<Mutex<Vec<Value>>> {
    let listener = UnixListener::bind(socket).expect("bind");
    let args = Arc::new(Mutex::new(Vec::new()));
    let seen = args.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            // The connect probe is ALSO a `status` (with no args): record only a real call.
            if request["cmd"] == "status" && request["args"].get("check_herdr").is_some() {
                seen.lock().unwrap().push(request["args"].clone());
            }
            let mut writer = &stream;
            let _ =
                writer.write_all(format!("{}\n", json!({"ok": true, "result": result})).as_bytes());
        }
    });
    args
}

#[test]
fn status_prints_what_the_python_printed_and_sends_check_herdr() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    for var in [
        "DUNKINGSHEEP_SOCKET",
        "DUNKINGSHEEP_SERVICE_TOKEN",
        "DUNKINGSHEEP_SERVICE_TOKEN_FILE",
    ] {
        std::env::remove_var(var);
    }
    let cases: Vec<(Value, &str)> = vec![
        (json!({"version": "2.71.4", "code_hash": "abc123", "code_stale": false, "pid": 42, "dunkers": 6, "running": 3,
                "queued_messages": 0, "herdr_available": false, "herdr_hint": "", "state_file": "/s",
                "uptime_s": 12.5, "self_pane_id": "%3", "message": "ok"}),
         "version          2.71.4 (code abc123)\ncode_stale       no\npid              42\ndunkers          6\nrunning          3\nqueued_messages  0\nherdr_available  False\nstate_file       /s\nuptime_s         12.5\nself_pane_id     %3\nmessage          ok\n"),
        (json!({"version": "2.71.4", "code_stale": null, "code_identity_errors": ["no git", "no hash"]}),
         "version          2.71.4\ncode_stale       UNKNOWN - source identity unavailable: no git, no hash\n"),
        (json!({"version": "1", "code_hash": "h", "code_stale": true, "code_changed_modules": ["dunk_api", "dunk_core"]}),
         "version          1 (code h)\ncode_stale       YES - code on disk differs from this process; restart it (dunk_api, dunk_core)\n"),
        (json!({}), "code_stale       UNKNOWN - source identity unavailable\n"),
    ];
    for (index, (payload, want)) in cases.into_iter().enumerate() {
        for (flag, want_flag) in [(None, false), (Some("--check-herdr"), true)] {
            let socket = base.path().join(format!("d{index}{want_flag}.sock"));
            let sent = fake_daemon(&socket, payload.clone());
            let sock = socket.display().to_string();
            let mut argv = vec![
                "dunkingsheep",
                "--socket",
                sock.as_str(),
                "--self-pane",
                "%99",
                "status",
            ];
            argv.extend(flag);
            let out = run(&try_parse(argv).unwrap());
            assert_eq!(
                (0, want, ""),
                (out.code, out.stdout.as_str(), out.stderr.as_str()),
                "case {index}"
            );
            assert_eq!(
                vec![json!({"check_herdr": want_flag})],
                *sent.lock().unwrap(),
                "case {index} {flag:?}"
            );
        }
    }
}
