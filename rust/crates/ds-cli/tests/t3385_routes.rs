//! t3385 batch 2: the three CLI routes that answered "parsed but not ported yet" for input the
//! Python CLI accepts. `context ledger status` is a differential against the Python CLI in the
//! same sandbox (both read the same store and ledger); bare `report` and bare `state`
//! reproduce Python's uncaught exception after the daemon probe, which a fake daemon answers
//! exactly once.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

fn env_for(command: &mut Command, sandbox: &Path, superuser: bool) {
    command
        .env("HOME", sandbox)
        .env("DUNKINGSHEEP_DIR", sandbox.join("cfg"))
        .env(
            "DUNKINGSHEEP_CONTEXT_LEDGER",
            sandbox.join("cfg").join("context-ledger.db"),
        )
        .env("DUNKINGSHEEP_SUPERUSER", if superuser { "1" } else { "0" })
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
}

fn rust_cli(sandbox: &Path, cwd: &Path, args: &[&str], superuser: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
    command.arg("--no-start-daemon").args(args).current_dir(cwd);
    env_for(&mut command, sandbox, superuser);
    command.output().expect("run the Rust CLI")
}

fn python3() -> PathBuf {
    ["/usr/bin/python3", "/bin/python3"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("a python3 on the host")
}

fn python_cli(sandbox: &Path, cwd: &Path, args: &[&str]) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let mut command = Command::new(python3());
    command
        .arg(script)
        .arg("--no-start-daemon")
        .args(args)
        .current_dir(cwd);
    env_for(&mut command, sandbox, false);
    command.output().expect("run the Python CLI")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Run `args` through both CLIs; the Rust stdout must be the Python stdout, byte for byte.
fn differential(sandbox: &Path, cwd: &Path, args: &[&str]) -> String {
    let rust = rust_cli(sandbox, cwd, args, false);
    let python = python_cli(sandbox, cwd, args);
    assert_eq!(
        Some(0),
        python.status.code(),
        "python {args:?}: {}",
        text(&python.stderr)
    );
    assert_eq!(
        Some(0),
        rust.status.code(),
        "rust {args:?}: {}",
        text(&rust.stderr)
    );
    assert_eq!(text(&python.stdout), text(&rust.stdout), "{args:?}");
    assert!(
        rust.stderr.is_empty(),
        "rust {args:?}: {}",
        text(&rust.stderr)
    );
    text(&rust.stdout)
}

fn slug_of(added: &Output) -> String {
    let line = text(&added.stdout);
    line.trim()
        .strip_prefix("added fact/")
        .unwrap_or_else(|| panic!("unexpected add output {line:?}"))
        .to_string()
}

#[test]
fn context_ledger_status_matches_the_python_cli_through_every_state() {
    let sandbox = tempfile::tempdir().unwrap();
    let proj = sandbox.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let init = rust_cli(sandbox.path(), &proj, &["context", "init"], false);
    assert_eq!(Some(0), init.status.code(), "{}", text(&init.stderr));
    let first = rust_cli(
        sandbox.path(),
        &proj,
        &[
            "context",
            "add",
            "--local",
            "--text",
            "the first claim",
            "--class",
            "fact",
            "--subject",
            "first",
        ],
        false,
    );
    assert_eq!(Some(0), first.status.code(), "{}", text(&first.stderr));
    let first_slug = slug_of(&first);

    // Bootstrap: the ledger has no rows at all, so every entry says so and the count is 0.
    let listing = differential(sandbox.path(), &proj, &["context", "ledger", "status"]);
    assert_eq!(
        listing,
        format!("{:<11} {:<13} {}\n", "bootstrap", "fact", first_slug)
    );
    let listing = differential(
        sandbox.path(),
        &proj,
        &["context", "ledger", "status", "--json"],
    );
    assert_eq!(
        serde_json::from_str::<Value>(&listing).unwrap(),
        json!({"entries": [{"slug": first_slug, "class": "fact", "state": "bootstrap"}], "count": 0})
    );

    // The owner adopts: the entry is recorded and the count is 1.
    let adopt = rust_cli(sandbox.path(), &proj, &["context", "ledger", "adopt"], true);
    assert_eq!(Some(0), adopt.status.code(), "{}", text(&adopt.stderr));
    assert!(
        text(&adopt.stdout).starts_with("adopted 1 entries into "),
        "{}",
        text(&adopt.stdout)
    );
    let listing = differential(sandbox.path(), &proj, &["context", "ledger", "status"]);
    assert_eq!(
        listing,
        format!("{:<11} {:<13} {}\n", "recorded", "fact", first_slug)
    );

    // A second entry nobody recorded, and the first one edited after adoption.
    let second = rust_cli(
        sandbox.path(),
        &proj,
        &[
            "context",
            "add",
            "--local",
            "--text",
            "the second claim",
            "--class",
            "fact",
            "--subject",
            "second",
        ],
        false,
    );
    assert_eq!(Some(0), second.status.code(), "{}", text(&second.stderr));
    let second_slug = slug_of(&second);
    let first_file = proj
        .join(".dunkingsheep")
        .join("fact")
        .join(format!("{first_slug}.md"));
    let mut edited = std::fs::read_to_string(&first_file).unwrap();
    edited.push_str("\nAn edit after adoption.\n");
    std::fs::write(&first_file, edited).unwrap();
    let listing = differential(
        sandbox.path(),
        &proj,
        &["context", "ledger", "status", "--json"],
    );
    let listing: Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(listing["count"], 1);
    let mut rows: Vec<(String, String, String)> = listing["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["slug"].as_str().unwrap().to_string(),
                row["class"].as_str().unwrap().to_string(),
                row["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    let mut expected = vec![
        (
            first_slug.clone(),
            "fact".to_string(),
            "modified".to_string(),
        ),
        (
            second_slug.clone(),
            "fact".to_string(),
            "unrecorded".to_string(),
        ),
    ];
    expected.sort();
    assert_eq!(rows, expected);
    differential(sandbox.path(), &proj, &["context", "ledger", "status"]);
}

/// A fake daemon that answers one `status` probe and then records whether anything else
/// arrived before the CLI exited.
fn fake_daemon(listener: UnixListener) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut commands = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => panic!("the fake daemon received no probe: {error}"),
            }
        };
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        commands.push(request["cmd"].as_str().unwrap().to_string());
        let mut writer = &stream;
        writer
            .write_all(format!("{}\n", json!({"ok": true, "result": {}})).as_bytes())
            .unwrap();
        // Nothing else may arrive: the bare command fails before any call, as in Python.
        let quiet_until = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while std::time::Instant::now() < quiet_until {
            if let Ok((stream, _)) = listener.accept() {
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                commands.push(request["cmd"].as_str().unwrap_or("<unparsed>").to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        commands
    })
}

#[test]
fn bare_report_and_state_reproduce_pythons_uncaught_exception_after_the_probe() {
    for (argv, last_line) in [
        (
            &["report"][..],
            "AttributeError: 'Namespace' object has no attribute 'prose'",
        ),
        (&["state"][..], "KeyError: None"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let daemon = fake_daemon(listener);
        let output = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
            .arg("--socket")
            .arg(&socket)
            .args(["--self-pane", "%99"])
            .args(argv)
            .env("HOME", temp.path())
            .env("DUNKINGSHEEP_DIR", temp.path().join("cfg"))
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
            .output()
            .expect("run the Rust CLI");
        assert_eq!(
            Some(1),
            output.status.code(),
            "{argv:?}: {}",
            text(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "{argv:?}: {}",
            text(&output.stdout)
        );
        assert_eq!(
            text(&output.stderr),
            format!("Traceback (most recent call last):\n{last_line}\n"),
            "{argv:?}"
        );
        assert_eq!(
            daemon.join().unwrap(),
            vec!["status".to_string()],
            "{argv:?}"
        );

        // Without a daemon the probe's refusal comes first, as Python's main() refuses
        // before run_client_command can raise.
        let absent = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
            .arg("--no-start-daemon")
            .arg("--socket")
            .arg(temp.path().join("absent.sock"))
            .args(["--self-pane", "%99"])
            .args(argv)
            .env("HOME", temp.path())
            .env("DUNKINGSHEEP_DIR", temp.path().join("cfg"))
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
            .output()
            .expect("run the Rust CLI");
        assert_ne!(Some(0), absent.status.code(), "{argv:?}");
        assert!(
            !text(&absent.stderr).contains("Traceback"),
            "{argv:?}: {}",
            text(&absent.stderr)
        );
        assert!(absent.stdout.is_empty());
    }
}
