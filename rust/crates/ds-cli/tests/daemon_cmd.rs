//! T44: the daemon-backed `gh-token` and `git-credential` against a FAKE daemon on a temporary
//! Unix socket, through argv -> generated parser -> `daemon_cmd`. The fake records every request
//! (cmd, args, self_pane_id) so the test can assert what the CLI SENT (the probe, the kind, the
//! pane) and what it did NOT send (no token request after a secrets refusal).
//! One test fn: the environment and the working directory are process-wide.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use ds_cli::args::try_parse;
use ds_cli::daemon_cmd::{gh_token, git_credential};
use serde_json::{json, Value};

type Log = Arc<Mutex<Vec<(String, Value, Value)>>>;

/// A daemon that answers `status` with ok and `gh_token` with `reply` (a whole response line).
fn fake_daemon(socket: &Path, reply: Value) -> Log {
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
            seen.lock().unwrap().push((
                cmd.clone(),
                request["args"].clone(),
                request["self_pane_id"].clone(),
            ));
            let response = if cmd == "gh_token" {
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

fn git(dir: &Path, args: &[&str]) {
    let done = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        done.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
}

fn calls(log: &Log, cmd: &str) -> Vec<(Value, Value)> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(c, _, _)| c == cmd)
        .map(|(_, a, p)| (a.clone(), p.clone()))
        .collect()
}

#[test]
fn gh_token_and_git_credential_talk_to_a_daemon_and_map_failures_like_the_python_cli() {
    let base = tempfile::tempdir().unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::remove_var("DUNKINGSHEEP_SOCKET");
    std::env::remove_var("DUNKINGSHEEP_SERVICE_TOKEN");
    std::env::remove_var("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
    let socket = base.path().join("d.sock");
    let sock = socket.display().to_string();
    let parse = |extra: &[&str]| {
        let mut argv = vec!["dunkingsheep", "--socket", &sock, "--self-pane", "%99"];
        argv.extend_from_slice(extra);
        try_parse(argv).unwrap()
    };

    // 1. A token: printed on stdout; the CLI probed (`status`) and then asked `gh_token` with the
    //    kind and the explicit pane.
    let log = fake_daemon(
        &socket,
        json!({"ok": true, "result": {"token": "ghs_abc", "role": "worker"}}),
    );
    let out = gh_token(&parse(&["gh-token", "--kind", "author"]));
    assert_eq!(
        (0, "ghs_abc\n", ""),
        (out.code, out.stdout.as_str(), out.stderr.as_str())
    );
    assert!(
        !calls(&log, "status").is_empty(),
        "connect probes with status"
    );
    assert_eq!(
        vec![(json!({"kind": "author"}), json!("%99"))],
        calls(&log, "gh_token")
    );

    // 2. No kind sends null; --json prints the whole result.
    let out = gh_token(&parse(&["gh-token", "--json"]));
    assert_eq!(0, out.code);
    assert_eq!(
        json!({"token": "ghs_abc", "role": "worker"}),
        serde_json::from_str::<Value>(&out.stdout).unwrap()
    );
    assert_eq!(json!({"kind": null}), calls(&log, "gh_token")[1].0);

    // 3. A daemon with no token for this role: stderr says why unless --quiet; exit 0.
    let socket2 = base.path().join("d2.sock");
    let sock2 = socket2.display().to_string();
    fake_daemon(
        &socket2,
        json!({"ok": true, "result": {"role": "owner", "note": "owner uses own login"}}),
    );
    let parse2 = |extra: &[&str]| {
        let mut argv = vec!["dunkingsheep", "--socket", &sock2, "--self-pane", "%99"];
        argv.extend_from_slice(extra);
        try_parse(argv).unwrap()
    };
    let out = gh_token(&parse2(&["gh-token"]));
    assert_eq!((0, ""), (out.code, out.stdout.as_str()));
    assert_eq!(
        "no App token for role owner: owner uses own login\n",
        out.stderr
    );
    assert_eq!("", gh_token(&parse2(&["gh-token", "--quiet"])).stderr);

    // 4. The daemon says no: "error: <message>", exit 2 (a DunkError).
    let socket3 = base.path().join("d3.sock");
    let sock3 = socket3.display().to_string();
    fake_daemon(
        &socket3,
        json!({"ok": false, "error": "gh-token --kind is owner/system only"}),
    );
    let out = gh_token(
        &try_parse([
            "dunkingsheep",
            "--socket",
            sock3.as_str(),
            "--self-pane",
            "%99",
            "gh-token",
            "--kind",
            "reviewer",
        ])
        .unwrap(),
    );
    assert_eq!((2, ""), (out.code, out.stdout.as_str()));
    assert_eq!("error: gh-token --kind is owner/system only\n", out.stderr);

    // 5. No daemon at all (and the Rust CLI never starts one): "error: ...", exit 3.
    let gone = base.path().join("nothing.sock").display().to_string();
    let out = gh_token(
        &try_parse([
            "dunkingsheep",
            "--socket",
            gone.as_str(),
            "--self-pane",
            "%99",
            "gh-token",
        ])
        .unwrap(),
    );
    assert_eq!(3, out.code);
    assert!(
        out.stderr.starts_with("error: no dunkingsheep daemon on "),
        "{}",
        out.stderr
    );

    // 6. git-credential: a clean branch gets the daemon's token in the credential format.
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    git(&repo, &["checkout", "-q", "-b", "lane/x"]);
    std::env::set_current_dir(&repo).unwrap();
    let request = "protocol=https\nhost=github.com\n\n";
    let before = calls(&log, "gh_token").len();
    let out = git_credential(&parse(&["git-credential", "get"]), &mut request.as_bytes());
    assert_eq!(
        (0, "username=x-access-token\npassword=ghs_abc\n"),
        (out.code, out.stdout.as_str())
    );
    assert_eq!(before + 1, calls(&log, "gh_token").len());

    // 7. A branch that ADDS a secret: refused BEFORE any token request reaches the daemon.
    std::fs::create_dir_all(repo.join("data")).unwrap();
    std::fs::write(repo.join("data/owner-credentials"), "Password: hunter2\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "oops"]);
    let before = calls(&log, "gh_token").len();
    let out = git_credential(&parse(&["git-credential", "get"]), &mut request.as_bytes());
    assert_eq!(("", 0), (out.stdout.as_str(), out.code));
    assert!(
        out.stderr
            .contains("REFUSED by the dunkingsheep git credential helper (t756)."),
        "{}",
        out.stderr
    );
    assert_eq!(
        before,
        calls(&log, "gh_token").len(),
        "no token was requested after a refusal"
    );

    // 8. store/erase and other hosts: silence, no token request.
    let out = git_credential(
        &parse(&["git-credential", "store"]),
        &mut request.as_bytes(),
    );
    assert_eq!(("", ""), (out.stdout.as_str(), out.stderr.as_str()));
}
