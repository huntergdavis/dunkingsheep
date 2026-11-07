//! T40 slice 1 tests, ported by name from `tests/test_dunk_server.py` (the server-level ones;
//! the command round-trips there belong to T39) with a fake [`Dispatcher`], plus the acceptance
//! test from t1364: two interleaved connections each keep their own identity.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};

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

fn sock_dir(tag: &str) -> tempfile::TempDir {
    crate::testsupport::dir(&format!("server-{tag}"))
}

struct Running {
    path: PathBuf,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    // Last field: removed after `Drop` has stopped and joined the server (t1490).
    _dir: tempfile::TempDir,
}

fn start(tag: &str) -> Running {
    let dir = sock_dir(tag);
    let path = dir.path().join("s.sock");
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
        _dir: dir,
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
fn socket_requests_without_peer_credentials_keep_an_unverified_verdict() {
    let fake = Fake::default();
    let (reply, _) = handle_line(&fake, r#"{"id":1,"cmd":"whoami"}"#, None);
    assert_eq!(
        reply.result.expect("whoami result")["socket_peer"],
        json!(false)
    );
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

struct BlockingDispatcher {
    entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    completed: std::sync::atomic::AtomicBool,
    closed_after_completion: std::sync::atomic::AtomicBool,
}

impl Dispatcher for BlockingDispatcher {
    fn dispatch(&self, _: &str, _: &Value, _: &RequestCtx) -> Result<Value, DunkError> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            entered.send(()).unwrap();
        }
        let (released, changed) = &*self.release;
        let mut released = released.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
        self.completed.store(true, Ordering::SeqCst);
        Ok(json!({"finished": true}))
    }

    fn close(&self) {
        self.closed_after_completion
            .store(self.completed.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

#[test]
fn close_waits_for_an_active_handler_before_closing_the_dispatcher() {
    let dir = sock_dir("drain-active");
    let path = dir.path().join("s.sock");
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let dispatcher = Arc::new(BlockingDispatcher {
        entered: Mutex::new(Some(entered_tx)),
        release: Arc::clone(&release),
        completed: std::sync::atomic::AtomicBool::new(false),
        closed_after_completion: std::sync::atomic::AtomicBool::new(false),
    });
    let dispatcher_trait: Arc<dyn Dispatcher> = dispatcher.clone();
    let mut server = FlockServer::new(dispatcher_trait, path.clone());
    server.bind().unwrap();
    let stop = server.stop_handle();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || done_tx.send(server.serve_forever()).unwrap());

    let mut client = connect(&path);
    client.send("{\"id\":1,\"cmd\":\"hold\"}\n");
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    assert!(done_rx
        .recv_timeout(std::time::Duration::from_millis(100))
        .is_err());

    let (released, changed) = &*release;
    *released.lock().unwrap() = true;
    changed.notify_all();
    done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    let response = client.line();
    assert_eq!(response["result"]["finished"], true);
    let mut eof = String::new();
    assert_eq!(client.reader.read_line(&mut eof).unwrap(), 0);
    assert!(dispatcher.closed_after_completion.load(Ordering::SeqCst));
    assert!(!path.exists());
}

#[test]
fn completed_socket_churn_is_reaped_before_shutdown() {
    let dir = sock_dir("drain-churn");
    let path = dir.path().join("s.sock");
    let mut server = FlockServer::new(Arc::new(Fake::default()), path.clone());
    server.bind().unwrap();
    let stop = server.stop_handle();
    let registry = Arc::clone(&server.connections);
    let changed = Arc::clone(&server.connections_changed);
    let thread = std::thread::spawn(move || server.serve_forever());

    for id in 0..32 {
        let mut client = connect(&path);
        let reply = client.call(json!({"id": id, "cmd": "whoami"}));
        assert_eq!(reply["ok"], true);
    }
    let registry_state = registry.lock().unwrap();
    let (registry_state, _) = changed
        .wait_timeout_while(registry_state, std::time::Duration::from_secs(5), |state| {
            !state.workers.is_empty()
        })
        .unwrap();
    assert!(
        registry_state.workers.is_empty(),
        "finished workers were retained"
    );
    drop(registry_state);

    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap().unwrap();
    assert!(!path.exists());
}

#[test]
fn draining_wakes_and_joins_a_registered_idle_reader() {
    let (server_stream, mut peer) = UnixStream::pair().unwrap();
    let registry = Arc::new(Mutex::new(ConnectionRegistry::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    admit_connection(
        Arc::new(Fake::default()),
        server_stream,
        1,
        Arc::clone(&stop),
        Arc::clone(&registry),
    )
    .unwrap();
    assert_eq!(registry.lock().unwrap().workers.len(), 1);

    let changed = Condvar::new();
    let (panicked, wake_errors, poisoned) = drain_connections(&registry, &changed);
    assert!(panicked.is_empty());
    assert!(wake_errors.is_empty());
    assert!(!poisoned);
    assert!(registry.lock().unwrap().workers.is_empty());
    assert!(!registry.lock().unwrap().admitting);
    let mut reader = BufReader::new(&mut peer);
    let mut line = String::new();
    assert_eq!(reader.read_line(&mut line).unwrap(), 0);
}

#[test]
fn admission_and_close_share_one_registry_gate() {
    let registry = Arc::new(Mutex::new(ConnectionRegistry::new()));
    let gate = registry.lock().unwrap();
    let (server_stream, peer) = UnixStream::pair().unwrap();
    let thread_registry = Arc::clone(&registry);
    let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
    let admission = std::thread::spawn(move || {
        let result = admit_connection(
            Arc::new(Fake::default()),
            server_stream,
            7,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            thread_registry,
        );
        admitted_tx.send(result).unwrap();
    });
    drop(gate);
    admitted_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap()
        .unwrap();
    admission.join().unwrap();
    assert_eq!(registry.lock().unwrap().workers.len(), 1);
    let changed = Condvar::new();
    let (_, wake_errors, poisoned) = drain_connections(&registry, &changed);
    assert!(wake_errors.is_empty());
    assert!(!poisoned);
    drop(peer);

    let closed_registry = Arc::new(Mutex::new(ConnectionRegistry::new()));
    let changed = Condvar::new();
    let (_, wake_errors, poisoned) = drain_connections(&closed_registry, &changed);
    assert!(wake_errors.is_empty());
    assert!(!poisoned);
    let (server_stream, mut peer) = UnixStream::pair().unwrap();
    admit_connection(
        Arc::new(Fake::default()),
        server_stream,
        8,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::clone(&closed_registry),
    )
    .unwrap();
    assert!(closed_registry.lock().unwrap().workers.is_empty());
    let mut reader = BufReader::new(&mut peer);
    let mut line = String::new();
    assert_eq!(reader.read_line(&mut line).unwrap(), 0);
}

#[test]
fn finished_worker_panics_are_joined_and_reported() {
    let registry = Mutex::new(ConnectionRegistry::new());
    let (wake, peer) = UnixStream::pair().unwrap();
    let join = std::thread::spawn(|| panic!("worker failed"));
    registry
        .lock()
        .unwrap()
        .workers
        .insert(9, ConnectionWorker { wake, join });

    let changed = Condvar::new();
    let (panicked, wake_errors, poisoned) = drain_connections(&registry, &changed);
    assert_eq!(panicked, vec![9]);
    assert!(wake_errors.is_empty());
    assert!(!poisoned);
    assert!(registry.lock().unwrap().workers.is_empty());
    drop(peer);
}

struct PanickingClose(Arc<std::sync::atomic::AtomicUsize>);

impl Dispatcher for PanickingClose {
    fn dispatch(&self, _: &str, _: &Value, _: &RequestCtx) -> Result<Value, DunkError> {
        Ok(Value::Null)
    }

    fn close(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("dispatcher close failed");
    }
}

#[test]
fn dispatcher_close_panics_are_reported_after_socket_cleanup() {
    let dir = sock_dir("close-panic");
    let path = dir.path().join("s.sock");
    let close_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut server = FlockServer::new(
        Arc::new(PanickingClose(Arc::clone(&close_calls))),
        path.clone(),
    );
    server.bind().unwrap();
    let (wake, _peer) = UnixStream::pair().unwrap();
    let worker = std::thread::spawn(|| panic!("connection worker failed"));
    server
        .connections
        .lock()
        .unwrap()
        .workers
        .insert(41, ConnectionWorker { wake, join: worker });

    assert!(matches!(
        server.close(),
        Err(BindError::Shutdown(failure))
            if failure.dispatcher_panicked
                && failure.worker_panics == vec![41]
                && failure.worker_wake_errors.is_empty()
                && !failure.registry_poisoned
                && failure.socket_error.is_none()
    ));
    assert!(!path.exists());
    assert!(matches!(
        server.close(),
        Err(BindError::Shutdown(failure))
            if failure.dispatcher_panicked && failure.worker_panics == vec![41]
    ));
    assert_eq!(close_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn second_server_on_same_socket_refuses() {
    let running = start("second");
    let mut other = FlockServer::new(Arc::new(Fake::default()), running.path.clone());
    assert!(matches!(other.bind(), Err(BindError::AlreadyRunning(_))));
}

#[test]
fn stale_socket_file_is_reclaimed() {
    let _dir = sock_dir("stale");
    let path = _dir.path().join("s.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap()); // leaves the file, nobody listens
    assert!(path.exists());
    let mut server = FlockServer::new(Arc::new(Fake::default()), path.clone());
    server.bind().unwrap();
    assert!(server.is_bound());
    server.close().unwrap();
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

struct ListingFails;
impl Dispatcher for ListingFails {
    fn dispatch(&self, _: &str, _: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
        Ok(
            json!({"pane": ctx.self_pane_id, "bound": ctx.identity_bound, "note": ctx.identity_note}),
        )
    }
    fn pane_info(&self, _: &str) -> Result<Option<PaneInfo>, DunkError> {
        Err(DunkError::Refused("tmux listing refused".into()))
    }
}

/// A backend that cannot answer does not contradict the claim: the deliberate no-backend trial
/// relies on it (the serve test). Only a listing that answers without the pane refuses (t3368).
#[test]
fn a_backend_that_cannot_answer_does_not_contradict_the_claim() {
    let me = i64::from(std::process::id());
    let (reply, _) = handle_line(
        &ListingFails,
        r#"{"id":1,"cmd":"x","self_pane_id":"w1:p2"}"#,
        Some(me),
    );
    assert_eq!(reply.result.unwrap()["pane"], json!("w1:p2"));
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

struct ThreadId;
impl Dispatcher for ThreadId {
    fn dispatch(&self, _: &str, _: &Value, _: &RequestCtx) -> Result<Value, DunkError> {
        Ok(json!(format!("{:?}", std::thread::current().id())))
    }
}

/// main's flock keeps caller state thread-local (CALLERS): every request of a connection must run
/// on ONE thread, start to finish, and two connections must not share one.
#[test]
fn a_connections_requests_all_run_on_one_thread_and_connections_do_not_share() {
    let _dir = sock_dir("threads");
    let path = _dir.path().join("s.sock");
    let mut server = FlockServer::new(Arc::new(ThreadId), path.clone());
    server.bind().unwrap();
    let stop = server.stop_handle();
    let thread = std::thread::spawn(move || {
        let _ = server.serve_forever();
    });
    let mut a = connect(&path);
    let mut b = connect(&path);
    let seen = |c: &mut Conn| c.call(json!({"id": 1, "cmd": "t"}))["result"].clone();
    let (a1, b1, a2, b2) = (seen(&mut a), seen(&mut b), seen(&mut a), seen(&mut b));
    assert_eq!(a1, a2);
    assert_eq!(b1, b2);
    assert_ne!(a1, b1);
    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}

#[test]
fn socket_python_nonfinite_wire_preserves_raw_literals_and_collisions() {
    let running = start("nonfinite-wire");
    let mut conn = connect(&running.path);
    conn.writer
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    conn.send(concat!(
        r#"{"id":NaN,"cmd":"args","args":{"raw":[NaN,Infinity,-Infinity,1e309],"literal":"Infinity","nested":{"$serde_json::private::Number":"NaN"},"escaped":"\"NaN","huge":123456789012345678901234567890}}"#,
        "\n"
    ));
    let mut reply = String::new();
    conn.reader.read_line(&mut reply).unwrap();
    assert_eq!(
        reply,
        concat!(
            r#"{"id":NaN,"ok":true,"result":{"raw":[NaN,Infinity,-Infinity,Infinity],"literal":"Infinity","nested":{"$serde_json::private::Number":"NaN"},"escaped":"\"NaN","huge":123456789012345678901234567890}}"#,
            "\n"
        )
    );
    // Invalid numeric grammar does not dispatch, and the following frame still succeeds.
    conn.send("{\"id\":1,\"cmd\":\"args\",\"args\":{\"x\":NaNx}}\n{\"id\":2,\"cmd\":\"whoami\"}\n");
    assert_eq!(
        conn.line(),
        json!({"id":null,"ok":false,"error":"invalid JSON"})
    );
    let identity = conn.line();
    assert_eq!(identity["result"]["socket_peer"], true);
    assert!(identity["result"]["caller_pid"].is_i64());
    assert_eq!(identity["result"]["self_pane_id"], Value::Null);
}

#[test]
fn actual_client_and_server_preserve_nonfinite_number_arguments() {
    let running = start("nonfinite-client");
    let mut client = crate::client::FlockClient::new(
        crate::client::UnixTransport {
            path: running.path.clone(),
        },
        Some("w1:p1".into()),
        "fixture".into(),
        std::time::Duration::from_secs(5),
        None,
    );
    let args = crate::wire_json::from_str(r#"{"n":NaN,"p":Infinity,"m":-Infinity,"literal":"NaN","$serde_json::private::Number":"Infinity"}"#).unwrap();
    let result = client
        .call("args", args.as_object().unwrap().clone())
        .unwrap();
    assert_eq!(
        crate::wire_json::to_string(&result).unwrap(),
        crate::wire_json::to_string(&args).unwrap()
    );
    assert!(result["n"].is_number());
    assert_eq!(crate::api::python_value_string(&result["n"]), "nan");
    assert_eq!(result["literal"], "NaN");
}
