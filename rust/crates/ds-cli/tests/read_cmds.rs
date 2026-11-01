//! T44: the daemon-backed READ commands `panes`, `workspaces` (and its alias `tabs`) and
//! `whoami`, argv -> generated parser -> dispatch -> a FAKE daemon. The expected text is what the
//! REAL Python printers printed for the same payloads (print_panes, print_workspaces, and
//! print_whoami after the CLI's identity_note setdefault). The fake also records which daemon
//! command each CLI command sent.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::{json, Value};

fn fake_daemon(socket: &std::path::Path, result: Value) -> Arc<Mutex<Vec<String>>> {
    let listener = UnixListener::bind(socket).expect("bind");
    let cmds = Arc::new(Mutex::new(Vec::new()));
    let seen = cmds.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let cmd = request["cmd"].as_str().unwrap_or("").to_string();
            seen.lock().unwrap().push(cmd.clone());
            let response = if cmd == "status" {
                json!({"ok": true, "result": {}})
            } else {
                json!({"ok": true, "result": result})
            };
            let mut writer = &stream;
            let _ = writer.write_all(format!("{response}\n").as_bytes());
        }
    });
    cmds
}

#[test]
fn panes_workspaces_tabs_and_whoami_print_what_the_python_printers_printed() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    for var in [
        "DUNKINGSHEEP_SOCKET",
        "DUNKINGSHEEP_SERVICE_TOKEN",
        "DUNKINGSHEEP_SERVICE_TOKEN_FILE",
    ] {
        std::env::remove_var(var);
    }
    // (cli words, daemon command expected, payload, expected text)
    let cases: Vec<(&str, &str, Value, &str)> = vec![
        ("panes", "list_panes", json!({"panes": []}),
         "no panes (is the terminal backend reachable? on the herdr rollback: is the herdr server running?)\n"),
        ("panes", "list_panes",
         json!({"panes": [
             {"pane_id": "%1", "workspace_label": "main", "tab_label": "work", "agent": "claude", "agent_status": "working", "cwd": "/home/x/proj", "is_self": true},
             {"pane_id": "%22222222", "workspace_label": "w".repeat(30), "tab_label": "t", "agent": null, "agent_status": "idle", "cwd": "/tmp"}]}),
         "PANE     WORKSPACE            TAB                      AGENT    STATUS   DIRECTORY\n*%1      main                 work                     claude   working  /home/x/proj\n %22222\u{2026} wwwwwwwwwwwwwwwwwww\u{2026} t                        -        idle     /tmp\n"),
        ("workspaces", "list_workspaces", json!({"workspaces": []}),
         "no workspaces (is the terminal backend reachable? on the herdr rollback: is the herdr server running?)\n"),
        ("tabs", "list_workspaces",
         json!({"workspaces": [{"workspace_id": "w1", "label": "main", "tab_count": 1, "agent_status": "idle",
                                "tabs": [{"tab_id": "w1:t1", "label": "work", "pane_count": 2, "agent_status": "busy"}]}],
                "self": {"workspace_id": "w1", "tab_id": "w1:t1"}}),
         "*w1     main                             1 tabs  idle\n   *w1:t1    work                             2 panes busy\n* = this pane's workspace/tab\n"),
        ("whoami", "whoami",
         json!({"pane_id": "%3", "role": "worker", "via_token": false,
                "registered": {"role": "worker", "pane_pid": 5, "provider": "pi", "by": "owner"}}),
         "pane %3  role worker\n  identity: explicit\n  registered: worker pid 5 provider pi by owner\n"),
        ("whoami", "whoami", json!({"role": "owner"}), "pane -  role owner\n  identity: explicit\n"),
    ];
    for (index, (word, want_cmd, payload, want)) in cases.into_iter().enumerate() {
        let socket = base.path().join(format!("d{index}.sock"));
        let cmds = fake_daemon(&socket, payload.clone());
        let sock = socket.display().to_string();
        let ns = try_parse([
            "dunkingsheep",
            "--socket",
            sock.as_str(),
            "--self-pane",
            "%99",
            word,
        ])
        .unwrap();
        let out = run(&ns);
        assert_eq!(
            (0, want, ""),
            (out.code, out.stdout.as_str(), out.stderr.as_str()),
            "case {index} ({word})"
        );
        assert!(
            cmds.lock().unwrap().iter().any(|c| c == want_cmd),
            "case {index}: sent {:?}",
            cmds.lock().unwrap()
        );
    }

    // --json prints the RAW daemon result: the client's identity note is added only to the HUMAN
    // output (Python's _whoami), never to --json (review t1503). A daemon-provided note wins.
    let raw = json!({"pane_id": "%3", "role": "worker"});
    let socket = base.path().join("json.sock");
    fake_daemon(&socket, raw.clone());
    let sock = socket.display().to_string();
    let ns = try_parse([
        "dunkingsheep",
        "--socket",
        sock.as_str(),
        "--self-pane",
        "%99",
        "whoami",
        "--json",
    ])
    .unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code);
    assert_eq!(
        raw,
        serde_json::from_str::<Value>(&out.stdout).unwrap(),
        "whoami --json must not gain identity_note"
    );
    let with_note = json!({"pane_id": "%3", "role": "worker", "identity_note": "from the daemon"});
    let socket = base.path().join("note.sock");
    fake_daemon(&socket, with_note);
    let sock = socket.display().to_string();
    let ns = try_parse([
        "dunkingsheep",
        "--socket",
        sock.as_str(),
        "--self-pane",
        "%99",
        "whoami",
    ])
    .unwrap();
    assert!(
        run(&ns).stdout.contains("identity: from the daemon"),
        "the daemon's own note wins"
    );
}
