//! `dunkingsheep serve` end to end (t3384, the t1605 serve lifecycle): the built binary runs
//! the production daemon (`ds_core::server_runtime::run_daemon`) in a sandbox of its own, binds
//! the socket named by the global `--socket`, answers a request, and exits 0 with the socket
//! removed after the `shutdown` command. There is no tmux: the backend binary is absent and
//! `DUNKINGSHEEP_ALLOW_EMPTY_BACKEND=1` is the deliberate-trial switch the Python daemon honours
//! too. The environment is cleared first, so the test can never reach a live daemon.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The caller's pane. The backend cannot answer for it, so the claim stands, as the Python
/// `_bind_identity` leaves a claim the backend cannot check.
const SELF_PANE: &str = "w1:p1";

/// One request on one connection, as `FlockClient.call` does; `None` when nothing answers.
fn call(socket: &Path, request: &Value) -> Option<Value> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    writeln!(stream, "{request}").ok()?;
    stream.shutdown(Shutdown::Write).ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn stderr_of(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut text);
    }
    text
}

fn wait_for_exit(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().expect("poll the daemon process") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn serve_runs_the_daemon_until_the_shutdown_command() {
    // Under /tmp: a unix socket path has to stay short.
    let temp = tempfile::Builder::new()
        .prefix("ds-serve-")
        .tempdir_in("/tmp")
        .expect("sandbox directory");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&config).expect("config directory");
    // The legacy allowlist the daemon opens at startup: the caller may shut it down.
    std::fs::write(
        config.join("superusers.json"),
        json!({"panes": [SELF_PANE], "names": []}).to_string(),
    )
    .expect("superusers fixture");
    let socket = temp.path().join("ds.sock");
    let state_file = temp.path().join("dunks.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .arg("--socket")
        .arg(&socket)
        .arg("serve")
        .arg("--state-file")
        .arg(&state_file)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", temp.path())
        .env("DUNKINGSHEEP_DIR", &config)
        .env("DUNKINGSHEEP_TERMINAL_BACKEND", "tmux")
        .env("DUNKINGSHEEP_TMUX_BIN", temp.path().join("no-such-tmux"))
        .env("DUNKINGSHEEP_ALLOW_EMPTY_BACKEND", "1")
        .env("DUNKINGSHEEP_VERSION", "serve-test")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `dunkingsheep serve`");
    let daemon_pid = child.id();

    // The daemon answers once it is bound; one that exits first is reported with its output.
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let request = json!({"id": 1, "cmd": "status", "args": {}, "self_pane_id": SELF_PANE});
        if let Some(reply) = call(&socket, &request) {
            break reply;
        }
        if let Some(exit) = child.try_wait().expect("poll the daemon process") {
            panic!(
                "serve exited before answering: {exit}\n{}",
                stderr_of(&mut child)
            );
        }
        assert!(
            Instant::now() < deadline,
            "serve did not answer status within 30 s\n{}",
            stderr_of(&mut child)
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(json!(true), status["ok"], "{status}");
    assert_eq!(json!("serve-test"), status["result"]["version"], "{status}");
    assert_eq!(
        json!(SELF_PANE),
        status["result"]["self_pane_id"],
        "{status}"
    );

    let shutdown = call(
        &socket,
        &json!({"id": 2, "cmd": "shutdown", "args": {}, "self_pane_id": SELF_PANE}),
    )
    .expect("the shutdown command is answered");
    assert_eq!(json!(true), shutdown["ok"], "{shutdown}");
    assert_eq!(
        json!(true),
        shutdown["result"]["shutting_down"],
        "{shutdown}"
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(10)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!(
            "serve did not exit within 10 s of the shutdown command\n{}",
            stderr_of(&mut child)
        );
    });
    let output = child.wait_with_output().expect("collect the daemon output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(Some(0), exit.code(), "stderr: {stderr}");
    assert!(!socket.exists(), "the socket file is removed on close");
    assert!(
        call(&socket, &json!({"id": 3, "cmd": "status", "args": {}})).is_none(),
        "nothing answers after the daemon exited"
    );

    // `configure_logging`: the lifecycle lines in the daemon log under the config directory.
    let log = std::fs::read_to_string(config.join("daemon.log")).expect("daemon.log");
    let listening = format!(
        " INFO dunkingsheep.server: dunkingsheep serve-test listening on {} (pid {daemon_pid})",
        socket.display()
    );
    assert!(log.lines().any(|line| line.ends_with(&listening)), "{log}");
    assert!(
        log.lines()
            .any(|line| line.ends_with(" INFO dunkingsheep.server: daemon stopped")),
        "{log}"
    );
}
