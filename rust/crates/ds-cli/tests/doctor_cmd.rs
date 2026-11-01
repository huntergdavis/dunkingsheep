//! T44: `doctor` through argv -> generated parser -> dispatch -> a FAKE daemon. The expected
//! text was produced by the real Python `print_doctor` on the same payloads (3 cases: ok, a
//! WARN with warnings and a non-dict guard surface, and an empty payload). The exit code is the
//! contract: `ok: false` is 2, anything else 0.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::{json, Value};

fn fake_daemon(socket: &std::path::Path, result: Value) {
    let listener = UnixListener::bind(socket).expect("bind");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let response = if request["cmd"] == "doctor" {
                json!({"ok": true, "result": result})
            } else {
                json!({"ok": true, "result": {}})
            };
            let mut writer = &stream;
            let _ = writer.write_all(format!("{response}\n").as_bytes());
        }
    });
}

#[test]
fn doctor_prints_what_the_python_printer_printed_and_exits_by_the_ok_flag() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::remove_var("DUNKINGSHEEP_SOCKET");
    std::env::remove_var("DUNKINGSHEEP_SERVICE_TOKEN");
    std::env::remove_var("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
    let cases: Vec<(Value, &str, i32)> = vec![
        (
            json!({"ok": true, "herdr_version": "tmux 3.4", "launches": 3,
                   "panes": {"total": 5, "with_agent": 2, "with_session": 1},
                   "surfaces": {"guard": {"installed": true, "shadowing_herdr": false, "active_in_this_shell": true},
                                "pane_read": "ok", "create_tab": "ok"}, "warnings": []}),
            "doctor ok  backend tmux 3.4  launches 3\n  panes 5 (agents 2, sessions 1)\n  guard installed=True shadowing=False active_in_this_shell=True\n  pane_read ok\n  create_tab ok\n",
            0,
        ),
        (
            json!({"ok": false, "herdr_version": null, "launches": 0, "panes": {},
                   "surfaces": {"guard": "n/a"},
                   "warnings": [{"kind": "stale_target", "detail": "d4 -> %9 missing"}, {"kind": "x", "detail": "y"}]}),
            "doctor WARN  backend None  launches 0\n  panes None (agents None, sessions None)\n  guard n/a\n  warning stale_target: d4 -> %9 missing\n  warning x: y\n",
            2,
        ),
        (
            json!({}),
            "doctor WARN  backend None  launches None\n  panes None (agents None, sessions None)\n",
            0,
        ),
    ];
    for (index, (payload, want, want_code)) in cases.into_iter().enumerate() {
        let socket = base.path().join(format!("d{index}.sock"));
        fake_daemon(&socket, payload.clone());
        let sock = socket.display().to_string();
        let ns = try_parse([
            "dunkingsheep",
            "--socket",
            sock.as_str(),
            "--self-pane",
            "%99",
            "doctor",
        ])
        .unwrap();
        let out = run(&ns);
        assert_eq!(
            (want_code, want, ""),
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            "case {index}"
        );
        // --json: the whole result as JSON, same exit code.
        let ns = try_parse([
            "dunkingsheep",
            "--socket",
            sock.as_str(),
            "--self-pane",
            "%99",
            "doctor",
            "--json",
        ])
        .unwrap();
        let out = run(&ns);
        assert_eq!(want_code, out.code);
        assert_eq!(
            payload,
            serde_json::from_str::<Value>(&out.stdout).unwrap(),
            "case {index}"
        );
    }
}
