//! `tests/test_dunk_server.py::ServerRoundTripTests`, ported against the PRODUCTION composition
//! (t3384, the t1605 serve lifecycle): `run_daemon_captured` with the real `RuntimeDispatcher`,
//! stores, loops, timer and outbox workers, over the fake terminal. `server_tests.rs` runs the
//! same wire protocol against a fake dispatcher; these prove the daemon that `dunkingsheep serve`
//! starts answers the Python fixture's requests and stops the way the Python server does.
//!
//! The Python fixture is `Flock(herdr=FakeHerdr, persist=False, restore=False, superusers=...)`
//! with `superusers.add("w1:p2")` and `FlockClient(self_pane_id="w1:p2")`. The Rust fake's
//! canned panes are w1:p1, w2:p2 and w3:p3, so the agent pane w2:p2 is the caller here and the
//! allowlist is the legacy `superusers.json` the daemon reads at startup.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::{run_daemon_captured, DaemonOptions, DaemonStartup};
use crate::config::{CapturedPythonHome, Env};
use crate::error::DunkError;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::loops::LoopIntervals;
use crate::terminal::fakes::FakeTerminal;

/// `FlockClient(self_pane_id="w1:p2")` in the Python fixture; see the module doc.
const SELF_PANE: &str = "w2:p2";

/// One production daemon on its own socket, with the Python fixture's client.
struct ProductionDaemon {
    socket: PathBuf,
    /// Every backend event of every worker-local fake terminal, in one shared log: the
    /// Python fake is one object, so `self.herdr.sends` sees what a worker thread typed.
    events: Arc<Mutex<Vec<Value>>>,
    finished: mpsc::Receiver<()>,
    handle: Option<std::thread::JoinHandle<Result<i32, DunkError>>>,
    stop: Arc<AtomicBool>,
    next_id: u64,
    // Last field: removed after `Drop` has stopped and joined the daemon.
    _dir: tempfile::TempDir,
}

impl ProductionDaemon {
    /// `ServerRoundTripTests.setUp`.
    fn start(tag: &str) -> Self {
        let dir = crate::testsupport::dir(&format!("daemon-socket-{tag}"));
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config directory");
        // `Superusers(path).add("w1:p2")`: the legacy allowlist the daemon opens at startup.
        std::fs::write(
            config.join("superusers.json"),
            json!({"panes": [SELF_PANE], "names": []}).to_string(),
        )
        .expect("superusers fixture");
        let socket = dir.path().join("s.sock");
        let env = Env::from_pairs(&[
            (
                "DUNKINGSHEEP_DIR",
                config.to_str().expect("UTF-8 fixture path"),
            ),
            (
                "DUNKINGSHEEP_SOCKET",
                socket.to_str().expect("UTF-8 socket path"),
            ),
            ("HOME", dir.path().to_str().expect("UTF-8 fixture home")),
        ]);
        let home = CapturedPythonHome::capture_with(&env, || Ok(None));
        // The daemon binds a claimed pane to the socket peer's agent ancestor, so every canned
        // pane is rooted at this test's own ancestor and the fixture's claim binds as it does
        // for a real pane. The Python fake has no pane pids, so its claim stands unchecked.
        let caller = crate::proc::caller_from_peer(
            Some(i64::from(std::process::id())),
            Some(SELF_PANE.into()),
        );
        let caller_root_pid = caller.pid.expect("resolved caller root pid");
        let events = Arc::new(Mutex::new(Vec::new()));
        let factory_events = Arc::clone(&events);
        let terminal_factory: LoopTerminalFactory = Arc::new(move || {
            let mut terminal = FakeTerminal::new();
            terminal.raw_events = Arc::clone(&factory_events);
            for pane in &mut terminal.panes {
                pane.pane_pid = Some(caller_root_pid);
            }
            Box::new(terminal)
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let stop = Arc::new(AtomicBool::new(false));
        let startup = DaemonStartup {
            env,
            home,
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(terminal_factory),
            // The background loops run once at start and then stay out of the way.
            loop_intervals: Some(LoopIntervals {
                push: Duration::from_secs(60),
                stuck: Duration::from_secs(60),
                reaper: Duration::from_secs(60),
            }),
            install_signals: false,
            ready: Some(ready_tx),
            loop_test_gate: None,
            stop_test_handle: Some(Arc::clone(&stop)),
        };
        let options = DaemonOptions {
            socket_path: Some(socket.clone()),
            state_file: Some(dir.path().join("dunks.json")),
            no_log_file: true,
        };
        let (finished_tx, finished_rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = run_daemon_captured(options, startup);
            let _ = finished_tx.send(());
            result
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("daemon bound and ready");
        Self {
            socket,
            events,
            finished: finished_rx,
            handle: Some(handle),
            stop,
            next_id: 0,
            _dir: dir,
        }
    }

    /// `FlockClient.call`: one connection per request, claiming the fixture pane; the whole
    /// response line, so a test can look at `ok` and `error` as the Python client does.
    fn response(&mut self, cmd: &str, args: Value) -> Value {
        self.next_id += 1;
        let request = json!({
            "id": self.next_id,
            "cmd": cmd,
            "args": args,
            "self_pane_id": SELF_PANE,
        });
        let mut stream = UnixStream::connect(&self.socket).expect("connect the daemon socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("request timeout");
        writeln!(stream, "{request}").expect("write request");
        stream
            .shutdown(Shutdown::Write)
            .expect("finish request input");
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("read the response line");
        assert!(
            !line.is_empty(),
            "daemon closed the connection without replying"
        );
        serde_json::from_str(&line).expect("decode the response")
    }

    /// The Python client raises on `ok: false`; here that is an assertion with the reply.
    fn call(&mut self, cmd: &str, args: Value) -> Value {
        let mut reply = self.response(cmd, args);
        assert_eq!(json!(true), reply["ok"], "{cmd}: {reply}");
        reply["result"].take()
    }

    /// `self.herdr.sends`: what was typed into which pane, across every worker.
    fn typed(&self) -> Vec<(String, String)> {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|event| event[0] == "typed")
            .map(|event| {
                (
                    event[1].as_str().expect("pane").to_owned(),
                    event[2].as_str().expect("text").to_owned(),
                )
            })
            .collect()
    }

    /// `FlockClient.is_running`: a `status` call that is answered.
    fn is_running(&self) -> bool {
        let Ok(mut stream) = UnixStream::connect(&self.socket) else {
            return false;
        };
        if stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
        {
            return false;
        }
        let request = json!({"id": "probe", "cmd": "status", "args": {}});
        if writeln!(stream, "{request}").is_err() || stream.shutdown(Shutdown::Write).is_err() {
            return false;
        }
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .ok()
            .filter(|count| *count > 0)
            .and_then(|_| serde_json::from_str::<Value>(&line).ok())
            .is_some_and(|reply| reply["ok"] == json!(true))
    }

    /// `self.thread.join(timeout)` followed by `assertFalse(self.thread.is_alive())`: the
    /// daemon thread's own result, or a panic when it is still running.
    fn join_within(&mut self, timeout: Duration) -> Result<i32, DunkError> {
        self.finished
            .recv_timeout(timeout)
            .expect("the daemon thread is still alive after the shutdown reply");
        self.handle
            .take()
            .expect("daemon thread guard")
            .join()
            .expect("daemon thread")
    }
}

impl Drop for ProductionDaemon {
    /// `tearDown`: `server.shutdown()` then join. The accept loop polls the stop flag.
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = self.finished.recv_timeout(Duration::from_secs(10));
            let _ = handle.join();
        }
    }
}

#[test]
fn status_and_crud_over_the_socket() {
    let mut daemon = ProductionDaemon::start("crud");
    let status = daemon.call("status", json!({}));
    assert_eq!(json!(0), status["dunkers"], "{status}");
    assert_eq!(json!(SELF_PANE), status["self_pane_id"], "{status}");

    let dunk = daemon.call(
        "add_dunk",
        json!({"target": "self", "text": "hi {id}", "interval_minutes": 15}),
    );
    assert_eq!(json!(SELF_PANE), dunk["target_pane_id"], "{dunk}");
    assert_eq!(json!(true), dunk["running"], "{dunk}");
    let id = dunk["id"].as_str().expect("created dunk id").to_owned();

    let listed = daemon.call("list_dunks", json!({}));
    let listed_ids: Vec<&Value> = listed["dunks"]
        .as_array()
        .expect("dunk rows")
        .iter()
        .map(|row| &row["id"])
        .collect();
    let expected_id = json!(id);
    assert_eq!(vec![&expected_id], listed_ids, "{listed}");

    let fired = daemon.call("fire_dunk", json!({"id": id}));
    assert_eq!(json!(true), fired["ok"], "{fired}");
    assert_eq!(
        vec![(SELF_PANE.to_owned(), "hi d1".to_owned())],
        daemon.typed(),
        "the fire typed the expanded text into the caller's pane"
    );

    daemon.call("stop_dunk", json!({"id": id}));
    let stopped = daemon.call("get_dunk", json!({"id": id}));
    assert_eq!(json!(false), stopped["running"], "{stopped}");
    daemon.call("remove_dunk", json!({"id": id}));
    let emptied = daemon.call("list_dunks", json!({}));
    assert_eq!(json!([]), emptied["dunks"], "{emptied}");
}

#[test]
fn raw_protocol_handles_multiple_requests_and_bad_json() {
    let daemon = ProductionDaemon::start("raw");
    let mut stream = UnixStream::connect(&daemon.socket).expect("connect the daemon socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("request timeout");
    stream
        .write_all(b"not json\n{\"id\": 7, \"cmd\": \"status\"}\n[1,2]\n")
        .expect("write three requests");
    let mut reader = BufReader::new(stream.try_clone().expect("reader half"));
    let mut line = |what: &str| -> Value {
        let mut text = String::new();
        reader.read_line(&mut text).expect(what);
        serde_json::from_str(&text).expect(what)
    };
    let first = line("first response");
    let second = line("second response");
    let third = line("third response");
    drop(stream);
    assert_eq!(json!(false), first["ok"], "{first}");
    assert_eq!(json!("invalid JSON"), first["error"], "{first}");
    assert_eq!(json!(true), second["ok"], "{second}");
    assert_eq!(json!(7), second["id"], "{second}");
    assert!(second["result"].is_object(), "{second}");
    assert_eq!(json!(false), third["ok"], "{third}");
    assert_eq!(
        json!("request must be an object"),
        third["error"],
        "{third}"
    );
}

#[test]
fn shutdown_command_stops_the_server() {
    let mut daemon = ProductionDaemon::start("shutdown");
    // A running dunk means a live timer worker that the shutdown has to stop and join.
    let dunk = daemon.call(
        "add_dunk",
        json!({"target": "w1:p1", "interval_minutes": 5}),
    );
    assert_eq!(json!(true), dunk["running"], "{dunk}");

    let result = daemon.call("shutdown", json!({"stop_all": true}));
    assert_eq!(json!(true), result["shutting_down"], "{result}");

    assert_eq!(
        0,
        daemon
            .join_within(Duration::from_secs(3))
            .expect("clean daemon exit after the shutdown command")
    );
    assert!(
        !daemon.socket.exists(),
        "the socket file is removed on close"
    );
    assert!(!daemon.is_running(), "nothing answers on the closed socket");
}
