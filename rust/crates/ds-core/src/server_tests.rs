//! T40 slice 1 tests, ported by name from `tests/test_dunk_server.py` (the server-level ones;
//! the command round-trips there belong to T39) with a fake [`Dispatcher`], plus the acceptance
//! test from t1364: two interleaved connections each keep their own identity.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::terminal::fakes::FakeTerminal;

#[derive(Default)]
struct Fake {
    closed: Mutex<u32>,
}

impl Dispatcher for Fake {
    fn dispatch(&self, cmd: &str, args: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
        match cmd {
            "whoami" => Ok(json!({
                "self_pane_id": ctx.self_pane_id,
                "service_token": ctx.service_token,
                "caller_pid": ctx.caller.as_ref().and_then(|c| c.pid),
                "caller_start": ctx.caller.as_ref().and_then(|c| c.start.clone()),
                "socket_peer": ctx.socket_peer,
            })),
            "shutdown" => Ok(json!({"shutting_down": true})),
            "boom" => panic!("kaboom"),
            "busy" => Err(DunkError::Busy),
            "args" => Ok(args.clone()),
            other => Err(DunkError::Refused(format!("unknown command {other:?}"))),
        }
    }
    fn close(&self) {
        *self.closed.lock().unwrap() += 1;
    }
}

fn sock_path(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-server-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("s.sock")
}

struct Running {
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn start(tag: &str) -> Running {
    let path = sock_path(tag);
    let _ = std::fs::remove_file(&path);
    let mut server = FlockServer::new(Arc::new(Fake::default()), path.clone());
    server.bind().unwrap();
    let stop = server.stop_handle();
    let thread = std::thread::spawn(move || {
        let _ = server.serve_forever();
    });
    Running {
        path,
        stop,
        thread: Some(thread),
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Conn {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

fn connect(path: &PathBuf) -> Conn {
    let writer = UnixStream::connect(path).unwrap();
    let reader = BufReader::new(writer.try_clone().unwrap());
    Conn { writer, reader }
}

impl Conn {
    fn send(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
    }
    fn line(&mut self) -> Value {
        let mut text = String::new();
        self.reader.read_line(&mut text).unwrap();
        serde_json::from_str(&text).unwrap()
    }
    fn call(&mut self, request: Value) -> Value {
        self.send(&format!("{request}\n"));
        self.line()
    }
}

#[test]
fn task_claims_carry_the_socket_peer_agent_identity() {
    let running = start("peer");
    let mut conn = connect(&running.path);
    let reply = conn.call(json!({"id": 1, "cmd": "whoami", "self_pane_id": "w1:p2"}));
    let result = &reply["result"];
    assert!(result["caller_pid"].is_i64(), "{reply}");
    assert!(result["caller_start"].is_string(), "{reply}");
    assert_eq!(result["socket_peer"], json!(true));
}

#[test]
fn errors_are_reported_not_raised_by_the_daemon() {
    let running = start("errors");
    let mut conn = connect(&running.path);
    let reply = conn.call(json!({"id": 1, "cmd": "explode"}));
    assert_eq!(reply["ok"], json!(false));
    assert!(reply["error"].as_str().unwrap().contains("unknown command"));
    assert!(reply.get("retryable").is_none());
    let busy = conn.call(json!({"id": 2, "cmd": "busy"}));
    assert_eq!(busy["retryable"], json!(true));
    assert_eq!(busy["error"], json!("database busy; retry the request"));
    let boom = conn.call(json!({"id": 3, "cmd": "boom"}));
    assert_eq!(boom["error"], json!("internal error: kaboom"));
    // The daemon is still alive afterwards.
    assert_eq!(
        conn.call(json!({"id": 4, "cmd": "whoami"}))["ok"],
        json!(true)
    );
}

#[test]
fn raw_protocol_handles_multiple_requests_and_bad_json() {
    let running = start("raw");
    let mut conn = connect(&running.path);
    conn.send("not json\n{\"id\": 7, \"cmd\": \"whoami\"}\n[1,2]\n");
    let first = conn.line();
    let second = conn.line();
    let third = conn.line();
    assert_eq!(first["ok"], json!(false));
    assert_eq!(first["error"], json!("invalid JSON"));
    assert_eq!(first["id"], Value::Null);
    assert_eq!(second["ok"], json!(true));
    assert_eq!(second["id"], json!(7));
    assert_eq!(third["ok"], json!(false));
    assert_eq!(third["error"], json!("request must be an object"));
}

#[test]
fn args_must_be_an_object_but_falsy_args_mean_empty() {
    let fake = Fake::default();
    let (reply, _) = handle_line(&fake, r#"{"id":1,"cmd":"args","args":[1]}"#, None);
    assert_eq!(reply.error.as_deref(), Some("args must be an object"));
    assert_eq!(reply.id, json!(1));
    for falsy in ["0", "\"\"", "[]", "null", "false"] {
        let line = format!(r#"{{"id":2,"cmd":"args","args":{falsy}}}"#);
        let (reply, _) = handle_line(&fake, &line, None);
        assert_eq!(reply.result, Some(json!({})), "args={falsy}");
    }
}

#[test]
fn shutdown_command_stops_the_server() {
    let mut running = start("shutdown");
    let mut conn = connect(&running.path);
    let reply = conn.call(json!({"id": 1, "cmd": "shutdown"}));
    assert_eq!(reply["result"]["shutting_down"], json!(true));
    running.thread.take().unwrap().join().unwrap();
    assert!(!running.path.exists());
    assert!(UnixStream::connect(&running.path).is_err());
}

#[test]
fn second_server_on_same_socket_refuses() {
    let running = start("second");
    let mut other = FlockServer::new(Arc::new(Fake::default()), running.path.clone());
    assert!(matches!(other.bind(), Err(BindError::AlreadyRunning(_))));
}

#[test]
fn stale_socket_file_is_reclaimed() {
    let path = sock_path("stale");
    let _ = std::fs::remove_file(&path);
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap()); // leaves the file, nobody listens
    assert!(path.exists());
    let mut server = FlockServer::new(Arc::new(Fake::default()), path.clone());
    server.bind().unwrap();
    assert!(server.is_bound());
    server.close();
    assert!(!path.exists());
}

#[test]
fn two_interleaved_connections_each_keep_their_own_identity() {
    let running = start("interleave");
    let mut a = connect(&running.path);
    let mut b = connect(&running.path);
    for round in 0..5 {
        a.send(&format!(
            "{}\n",
            json!({"id": round, "cmd": "whoami", "self_pane_id": "w1:pa", "service_token": "ta"})
        ));
        b.send(&format!(
            "{}\n",
            json!({"id": round, "cmd": "whoami", "self_pane_id": "w1:pb"})
        ));
        let (ra, rb) = (a.line(), b.line());
        assert_eq!(ra["result"]["self_pane_id"], json!("w1:pa"));
        assert_eq!(ra["result"]["service_token"], json!("ta"));
        assert_eq!(rb["result"]["self_pane_id"], json!("w1:pb"));
        assert_eq!(
            rb["result"]["service_token"],
            Value::Null,
            "no token leaks across"
        );
    }
}

struct NoSuchPane;
impl Dispatcher for NoSuchPane {
    fn dispatch(&self, _: &str, _: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
        Ok(
            json!({"pane": ctx.self_pane_id, "bound": ctx.identity_bound, "note": ctx.identity_note}),
        )
    }
    fn pane_info(&self, _: &str) -> Result<Option<PaneInfo>, DunkError> {
        Ok(None)
    }
}

#[test]
fn a_made_up_pane_is_anonymous() {
    let me = i64::from(std::process::id());
    let (reply, _) = handle_line(
        &NoSuchPane,
        r#"{"id":1,"cmd":"x","self_pane_id":"w1:p2"}"#,
        Some(me),
    );
    let result = reply.result.unwrap();
    assert_eq!(result["pane"], Value::Null);
    assert_eq!(result["bound"], json!(false));
    assert!(result["note"].as_str().unwrap().contains("no such pane"));
}

#[test]
fn without_peer_credentials_the_claim_stands_unchecked() {
    let (reply, _) = handle_line(
        &NoSuchPane,
        r#"{"id":1,"cmd":"x","self_pane_id":"w1:p2"}"#,
        None,
    );
    assert_eq!(reply.result.unwrap()["pane"], json!("w1:p2"));
}

#[test]
fn refuses_an_unsupervised_bind_of_the_default_socket() {
    let default = PathBuf::from("/x/dunkingsheep.sock");
    let env = Env::from_pairs(&[]);
    let refusal = startup_refusal(&env, &default, &default, true, &FakeTerminal::new(), "tmux");
    assert_eq!(refusal.unwrap().0, 3);
    let supervised = Env::from_pairs(&[("DUNKINGSHEEP_SUPERVISED", "1")]);
    assert!(startup_refusal(
        &supervised,
        &default,
        &default,
        true,
        &FakeTerminal::new(),
        "tmux"
    )
    .is_none());
}

#[test]
fn a_non_default_socket_is_allowed() {
    let env = Env::from_pairs(&[]);
    let default = PathBuf::from("/x/dunkingsheep.sock");
    let custom = PathBuf::from("/tmp/not-the-unit.sock");
    assert!(startup_refusal(&env, &custom, &default, true, &FakeTerminal::new(), "tmux").is_none());
}

#[test]
fn a_backend_with_zero_panes_is_refused_unless_allowed() {
    let env = Env::from_pairs(&[]);
    let p = PathBuf::from("/tmp/a.sock");
    let empty = FakeTerminal::unavailable();
    if empty.list_panes().is_empty() {
        let (code, text) = startup_refusal(&env, &p, &p, false, &empty, "tmux").unwrap();
        assert_eq!(code, 4);
        assert!(text.contains("sees zero panes"));
        let allow = Env::from_pairs(&[("DUNKINGSHEEP_ALLOW_EMPTY_BACKEND", "1")]);
        assert!(startup_refusal(&allow, &p, &p, false, &empty, "tmux").is_none());
    }
}

#[test]
fn die_with_parent_watches_the_spawner_only_for_test_spawns_or_explicit_pids() {
    assert_eq!(parent_to_watch(&Env::from_pairs(&[]), 42), None);
    assert_eq!(
        parent_to_watch(&Env::from_pairs(&[("DUNKINGSHEEP_TEST_SPAWN", "1")]), 42),
        Some(42)
    );
    assert_eq!(
        parent_to_watch(
            &Env::from_pairs(&[("DUNKINGSHEEP_DIE_WITH_PARENT", "77")]),
            42
        ),
        Some(77)
    );
    assert_eq!(
        parent_to_watch(
            &Env::from_pairs(&[("DUNKINGSHEEP_DIE_WITH_PARENT", "abc")]),
            42
        ),
        None
    );
    assert!(!kill_own_process_group(&Env::from_pairs(&[])));
}
