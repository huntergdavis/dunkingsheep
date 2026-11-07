//! `dunkingsheep serve` end to end (t3384, the t1605 serve lifecycle): the built binary runs
//! the production daemon (`ds_core::server_runtime::run_daemon`) in a sandbox of its own, binds
//! the socket named by the global `--socket`, answers a request, and stops the three ways the
//! Python `run_daemon` stops: the `shutdown` command, a signal, and a vanished parent. There is
//! no tmux: the backend binary is absent and `DUNKINGSHEEP_ALLOW_EMPTY_BACKEND=1` is the
//! deliberate-trial switch the Python daemon honours too. The environment is cleared first, so
//! the test can never reach a live daemon.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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

/// One `dunkingsheep serve` process in its own sandbox directory.
struct Served {
    child: Child,
    socket: PathBuf,
    config: PathBuf,
    // Last field: removed after the daemon has been joined or killed.
    _temp: tempfile::TempDir,
}

impl Served {
    /// Spawn the built binary and wait until it answers `status` on its socket.
    fn start(extra_env: &[(&str, String)]) -> Self {
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
        command
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
            .stderr(Stdio::piped());
        for (name, value) in extra_env {
            command.env(name, value);
        }
        let child = command.spawn().expect("spawn `dunkingsheep serve`");
        let mut served = Self {
            child,
            socket,
            config,
            _temp: temp,
        };
        // The daemon answers once it is bound; one that exits first is reported with its output.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if served.call("status", 0).is_some() {
                return served;
            }
            if let Some(exit) = served.child.try_wait().expect("poll the daemon process") {
                panic!("serve exited before answering: {exit}\n{}", served.stderr());
            }
            assert!(
                Instant::now() < deadline,
                "serve did not answer status within 30 s\n{}",
                served.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn call(&self, cmd: &str, id: u64) -> Option<Value> {
        call(
            &self.socket,
            &json!({"id": id, "cmd": cmd, "args": {}, "self_pane_id": SELF_PANE}),
        )
    }

    fn stderr(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }

    /// The exit code within `within`, or a panic that shows the daemon's stderr.
    fn exit_code(&mut self, within: Duration, after: &str) -> Option<i32> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll the daemon process") {
                return status.code();
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!(
                    "serve did not exit within {within:?} after {after}\n{}",
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.config.join("daemon.log")).expect("daemon.log")
    }

    /// The `FlockServer.bind` line, with the release version and the daemon's pid.
    fn listening_line(&self) -> String {
        format!(
            " INFO dunkingsheep.server: dunkingsheep serve-test listening on {} (pid {})",
            self.socket.display(),
            self.child.id()
        )
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn has_line(log: &str, suffix: &str) -> bool {
    log.lines().any(|line| line.ends_with(suffix))
}

#[test]
fn serve_runs_the_daemon_until_the_shutdown_command() {
    let mut served = Served::start(&[]);
    let status = served.call("status", 1).expect("status is answered");
    assert_eq!(json!(true), status["ok"], "{status}");
    assert_eq!(json!("serve-test"), status["result"]["version"], "{status}");
    assert_eq!(
        json!(SELF_PANE),
        status["result"]["self_pane_id"],
        "{status}"
    );

    let shutdown = served
        .call("shutdown", 2)
        .expect("the shutdown command is answered");
    assert_eq!(json!(true), shutdown["ok"], "{shutdown}");
    assert_eq!(
        json!(true),
        shutdown["result"]["shutting_down"],
        "{shutdown}"
    );

    let code = served.exit_code(Duration::from_secs(10), "the shutdown command");
    assert_eq!(Some(0), code, "stderr: {}", served.stderr());
    assert!(
        !served.socket.exists(),
        "the socket file is removed on close"
    );
    assert!(
        served.call("status", 3).is_none(),
        "nothing answers after the daemon exited"
    );

    // `configure_logging`: the lifecycle lines in the daemon log under the config directory.
    let log = served.log();
    assert!(has_line(&log, &served.listening_line()), "{log}");
    assert!(
        has_line(&log, " INFO dunkingsheep.server: daemon stopped"),
        "{log}"
    );
}

/// `_on_signal`: SIGTERM is logged, the server drains and closes, the process exits 0.
#[test]
fn serve_stops_on_sigterm_and_logs_the_signal() {
    let mut served = Served::start(&[]);
    let killed = Command::new("kill")
        .args(["-TERM", &served.child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(killed.success(), "kill -TERM the daemon");

    let code = served.exit_code(Duration::from_secs(10), "SIGTERM");
    assert_eq!(Some(0), code, "stderr: {}", served.stderr());
    assert!(
        !served.socket.exists(),
        "the socket file is removed on close"
    );

    let log = served.log();
    assert!(has_line(&log, &served.listening_line()), "{log}");
    assert!(
        has_line(
            &log,
            " INFO dunkingsheep.server: received signal 15, shutting down"
        ),
        "{log}"
    );
    assert!(
        has_line(&log, " INFO dunkingsheep.server: daemon stopped"),
        "{log}"
    );
}

/// DIE WITH THE PARENT (t212): `DUNKINGSHEEP_DIE_WITH_PARENT` names a pid the daemon watches
/// every two seconds; when it is gone the daemon logs it and exits hard with 0, as the Python
/// `_watch_parent` does with `os._exit(0)`. The socket file stays, as after `os._exit`.
#[test]
fn serve_exits_when_the_watched_parent_is_gone() {
    // Long-lived until the daemon is up, so a slow cold start cannot outlive the parent.
    let mut parent = Command::new("sleep")
        .arg("600")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the stand-in parent");
    let parent_pid = parent.id().to_string();
    let mut served = Served::start(&[("DUNKINGSHEEP_DIE_WITH_PARENT", parent_pid.clone())]);
    parent.kill().expect("end the stand-in parent");
    parent.wait().expect("reap the stand-in parent");

    let code = served.exit_code(Duration::from_secs(10), "its parent went away");
    assert_eq!(Some(0), code, "stderr: {}", served.stderr());

    let log = served.log();
    assert!(has_line(&log, &served.listening_line()), "{log}");
    assert!(
        has_line(
            &log,
            &format!(
                " WARNING dunkingsheep.server: parent pid {parent_pid} is gone; a test-spawned daemon exits with it"
            )
        ),
        "{log}"
    );
}
