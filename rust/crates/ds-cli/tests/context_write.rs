//! T44: `context add|promote|supersede|retire` through argv -> generated parser -> dispatch -> a
//! FAKE daemon that records each request (cmd + args). Asserts what the CLI SENT (the argument
//! names are the wire contract with the daemon: `cls`, `type`, `global_store`, `project_path`...)
//! and what it printed / exited with in each failure class, as `_context_via_daemon` does.
//! One test fn: cwd and environment are process-wide.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::{json, Value};

type Log = Arc<Mutex<Vec<(String, Value)>>>;

fn fake_daemon(socket: &std::path::Path, reply: Value) -> Log {
    let listener = UnixListener::bind(socket).expect("bind");
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            let cmd = request["cmd"].as_str().unwrap_or("").to_string();
            let response = if cmd.starts_with("context_") {
                seen.lock().unwrap().push((cmd, request["args"].clone()));
                reply.clone()
            } else {
                json!({"ok": true, "result": {}})
            };
            let mut writer = &stream;
            let _ = writer.write_all(format!("{response}\n").as_bytes());
        }
    });
    log
}

#[test]
fn context_writes_go_to_the_daemon_with_the_pythons_arguments_and_failure_mapping() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    for var in [
        "DUNKINGSHEEP_SOCKET",
        "DUNKINGSHEEP_SERVICE_TOKEN",
        "DUNKINGSHEEP_SERVICE_TOKEN_FILE",
    ] {
        std::env::remove_var(var);
    }
    std::env::set_current_dir(base.path()).unwrap();
    let project = std::env::current_dir()
        .unwrap()
        .join(".dunkingsheep")
        .display()
        .to_string();
    let socket = base.path().join("d.sock");
    let sock = socket.display().to_string();
    let log = fake_daemon(
        &socket,
        json!({"ok": true, "result": {"class": "fact", "slug": "2026-11-01-thing"}}),
    );
    let go = |words: &[&str]| {
        let mut argv = vec![
            "dunkingsheep",
            "--socket",
            sock.as_str(),
            "--self-pane",
            "%99",
        ];
        argv.extend_from_slice(words);
        run(&try_parse(argv).unwrap())
    };

    // add --text: every argument the Python sends, by the Python's names.
    let out = go(&[
        "context",
        "add",
        "--text",
        "Flux needs 1.21 GW.",
        "--class",
        "fact",
        "--subject",
        "flux",
        "--type",
        "reference",
        "--authority",
        "measured",
        "--source",
        "lab",
        "--stated",
        "2026-03-04",
    ]);
    assert_eq!(
        (0, "added fact/2026-11-01-thing\n", ""),
        (out.code, out.stdout.as_str(), out.stderr.as_str())
    );
    let (cmd, args) = log.lock().unwrap().last().unwrap().clone();
    assert_eq!("context_add", cmd);
    assert_eq!(
        json!({"body": "Flux needs 1.21 GW.", "cls": "fact", "subject": "flux", "type": "reference",
               "authority": "measured", "source": "lab", "stated": "2026-03-04", "recheck_after": null,
               "supersedes": null, "global_store": false, "project_path": project}),
        args
    );

    // add --global: project_path is null and global_store true.
    go(&[
        "context",
        "add",
        "--text",
        "g",
        "--class",
        "fact",
        "--subject",
        "s",
        "--global",
    ]);
    let (_, args) = log.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (json!(true), Value::Null),
        (args["global_store"].clone(), args["project_path"].clone())
    );

    // promote / retire / supersede (body from a file).
    let out = go(&["context", "promote", "some-slug"]);
    assert_eq!("promoted 2026-11-01-thing\n", out.stdout);
    assert_eq!(
        json!({"slug": "some-slug", "global_store": false, "project_path": project}),
        log.lock().unwrap().last().unwrap().1
    );
    let out = go(&["context", "retire", "some-slug", "--reason", "obsolete"]);
    assert_eq!("retired 2026-11-01-thing\n", out.stdout);
    assert_eq!(
        json!("obsolete"),
        log.lock().unwrap().last().unwrap().1["reason"]
    );
    let file = base.path().join("body.md");
    std::fs::write(&file, "new body\n").unwrap();
    let out = go(&["context", "supersede", "some-slug", file.to_str().unwrap()]);
    assert_eq!("superseded by 2026-11-01-thing\n", out.stdout);
    assert_eq!(
        json!("new body\n"),
        log.lock().unwrap().last().unwrap().1["body"]
    );

    // A missing body file is a refusal (exit 2) naming the path with a Python repr; --json does not
    // change the one-line output of this path.
    let out = go(&["context", "supersede", "some-slug", "/no/such/file.md"]);
    assert_eq!(2, out.code);
    assert_eq!(
        "refused: no such file: '/no/such/file.md' - the body comes from --text, stdin or a file\n",
        out.stderr
    );
    let out = go(&["context", "promote", "some-slug", "--json"]);
    assert_eq!("promoted 2026-11-01-thing\n", out.stdout);

    // --local writes the files directly: not ported (and nothing reaches the daemon).
    let before = log.lock().unwrap().len();
    let out = go(&["context", "promote", "x", "--local"]);
    assert_eq!(1, out.code);
    assert!(out.stderr.contains("not ported yet"), "{}", out.stderr);
    assert_eq!(before, log.lock().unwrap().len());

    // The daemon says no: "refused: <reason>", exit 2.
    let socket2 = base.path().join("d2.sock");
    let sock2 = socket2.display().to_string();
    fake_daemon(
        &socket2,
        json!({"ok": false, "error": "only the owner may promote a ground truth"}),
    );
    let out = run(&try_parse([
        "dunkingsheep",
        "--socket",
        sock2.as_str(),
        "--self-pane",
        "%99",
        "context",
        "promote",
        "s",
    ])
    .unwrap());
    assert_eq!(2, out.code);
    assert_eq!(
        "refused: only the owner may promote a ground truth\n",
        out.stderr
    );

    // No daemon: the Python's wording, exit 1.
    let gone = base.path().join("nothing.sock").display().to_string();
    let out = run(&try_parse([
        "dunkingsheep",
        "--socket",
        gone.as_str(),
        "--self-pane",
        "%99",
        "context",
        "promote",
        "s",
    ])
    .unwrap());
    assert_eq!(1, out.code);
    assert!(
        out.stderr.starts_with("daemon unavailable ("),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .ends_with("; the owner may write locally with --local\n"),
        "{}",
        out.stderr
    );
}
