use crate::api::{Dispatcher, FlockGate, SendCommandEnv};
use crate::config::Env;
use crate::error::DunkError;
use crate::flock::{panes::Stores, Flock};
use crate::inbox_store::Store;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::{outbox_runtime::OutboxRuntime, RequestRuntimeAdapter};
use crate::terminal::{
    fakes::FakeTerminal, AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

type KeyBatches = Arc<Mutex<Vec<(String, Vec<String>)>>>;

#[derive(Debug)]
struct RecordingTerminal {
    inner: FakeTerminal,
    batches: KeyBatches,
}
impl Terminal for RecordingTerminal {
    fn version(&self) -> String {
        self.inner.version()
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    fn server_error_hint(&self) -> String {
        self.inner.server_error_hint()
    }
    fn list_panes(&self) -> Vec<Pane> {
        self.inner.list_panes()
    }
    fn get_pane(&self, id: &str) -> Option<Pane> {
        self.inner.get_pane(id)
    }
    fn list_tabs(&self) -> Vec<Tab> {
        self.inner.list_tabs()
    }
    fn list_workspaces(&self) -> Vec<Workspace> {
        self.inner.list_workspaces()
    }
    fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
        self.inner.list_panes_grouped()
    }
    fn agent_child_pids(&self, pid: i64) -> Result<Vec<i64>, TerminalError> {
        self.inner.agent_child_pids(pid)
    }
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        self.inner.create_workspace(label, cwd, focus)
    }
    fn create_tab(
        &mut self,
        workspace: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        self.inner.create_tab(workspace, label, cwd, focus)
    }
    fn split_pane(
        &mut self,
        id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError> {
        self.inner.split_pane(id, direction, cwd)
    }
    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        self.inner.start_agent(spec)
    }
    fn rename(&mut self, kind: &str, id: &str, label: &str) -> Result<(), TerminalError> {
        self.inner.rename(kind, id, label)
    }
    fn close(&mut self, kind: &str, id: &str) -> Result<(), TerminalError> {
        self.inner.close(kind, id)
    }
    fn focus(&mut self, kind: &str, id: &str) -> Result<(), TerminalError> {
        self.inner.focus(kind, id)
    }
    fn run_command(&mut self, id: &str, command: &str) -> Result<(), TerminalError> {
        self.inner.run_command(id, command)
    }
    fn send_text(&mut self, id: &str, text: &str) -> Result<(), TerminalError> {
        self.inner.send_text(id, text)
    }
    fn send_text_and_enter(&mut self, id: &str, text: &str) -> Result<(), TerminalError> {
        self.inner.send_text_and_enter(id, text)
    }
    fn send_keys(&mut self, id: &str, keys: &[&str]) -> Result<(), TerminalError> {
        self.inner.send_keys(id, keys)?;
        self.batches
            .lock()
            .unwrap()
            .push((id.into(), keys.iter().map(|s| (*s).into()).collect()));
        Ok(())
    }
    fn read_pane(&self, id: &str, lines: i64, ansi: bool) -> Result<Option<String>, TerminalError> {
        self.inner.read_pane(id, lines, ansi)
    }
    fn agent_status(&self, id: &str) -> Option<String> {
        self.inner.agent_status(id)
    }
    fn resize(&mut self, id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
        self.inner.resize(id, cols, rows)
    }
}

struct Fixture {
    outbox: OutboxRuntime,
    flock: Arc<Flock>,
    runtime: tokio::runtime::Runtime,
    terminal: Mutex<Box<dyn Terminal>>,
    batches: KeyBatches,
    worker_threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    _dir: tempfile::TempDir,
}

fn terminal(batches: KeyBatches) -> Box<dyn Terminal> {
    let mut inner = FakeTerminal::new();
    for pane in &mut inner.panes {
        pane.pane_pid = Some(std::process::id() as i64);
    }
    Box::new(RecordingTerminal { inner, batches })
}

impl Fixture {
    fn new() -> Self {
        let dir = crate::testsupport::dir("raw-key-provider");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(Stores {
            inbox: Some(Arc::new(
                Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap(),
            )),
            ..Stores::default()
        });
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        for (pane, role) in [("w1:p1", "worker"), ("w2:p2", "owner"), ("w3:p3", "desk")] {
            roles
                .register(pane, role, None, None, None, None, None, None)
                .unwrap();
        }
        flock.attach_roles(roles, None);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_threads = Arc::new(Mutex::new(Vec::new()));
        let outbox = OutboxRuntime::new(Arc::clone(&flock), runtime.handle().clone(), {
            let batches = Arc::clone(&batches);
            let gate = Arc::clone(&gate);
            let threads = Arc::clone(&worker_threads);
            Arc::new(move || {
                threads.lock().unwrap().push(std::thread::current().id());
                let (released, wake) = &*gate;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
                terminal(Arc::clone(&batches))
            })
        })
        .unwrap();
        Self {
            terminal: Mutex::new(terminal(Arc::clone(&batches))),
            batches,
            gate,
            worker_threads,
            outbox,
            flock,
            runtime,
            _dir: dir,
        }
    }

    fn request(pane: &str) -> RequestCtx {
        let pid = std::process::id() as i64;
        let process = crate::proc::stat(pid).unwrap();
        RequestCtx {
            self_pane_id: Some(pane.into()),
            socket_peer: Some(true),
            caller: Some(crate::proc::Caller::new(
                Some(pid),
                Some(process.start),
                Some("codex".into()),
                Some(pane.into()),
                "agent",
            )),
            ..RequestCtx::default()
        }
    }

    fn dispatch(&self, args: Value, pane: &str) -> Result<Value, DunkError> {
        let request = Self::request(pane);
        let adapter = RequestRuntimeAdapter::new(
            &self.flock,
            &request,
            self.runtime.handle(),
            &self.terminal,
        )
        .with_outbox(&self.outbox)?;
        let context = adapter.handler_context();
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_send_text_keys_flush_box();
        dispatcher.dispatch(
            &context,
            &FlockGate(&context),
            "send_keys",
            Some(args.as_object().unwrap().clone()),
        )
    }

    fn raw_provider(&self, keys: &Value, target: &Value) -> Result<Value, DunkError> {
        let request = Self::request("w1:p1");
        let adapter = RequestRuntimeAdapter::new(
            &self.flock,
            &request,
            self.runtime.handle(),
            &self.terminal,
        )
        .with_outbox(&self.outbox)?;
        SendCommandEnv::send_keys_raw(&adapter, target, keys, Some("w1:p1"), false, 0.0)
    }

    fn release_worker(&self) {
        let (released, wake) = &*self.gate;
        *released.lock().unwrap() = true;
        wake.notify_all();
    }

    fn assert_pristine(&self) {
        assert_eq!(self.flock.snapshot()["next_message_id"], 1);
        assert!(self.flock.outbox_snapshot().is_empty());
        assert!(self.flock.message_history.lock().unwrap().is_empty());
        assert!(self.batches.lock().unwrap().is_empty());
        assert!(self.worker_threads.lock().unwrap().is_empty());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.release_worker();
        self.outbox.stop_and_join();
    }
}

fn oracle() -> Vec<Value> {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../tests/fixtures/send_keys_raw_oracle.json"
    )))
    .unwrap()
}

#[test]
fn raw_send_keys_dispatcher_and_canonical_provider_match_python_oracle() {
    for case in oracle() {
        let fixture = Fixture::new();
        let keys: Value = serde_json::from_str(case["json"].as_str().unwrap()).unwrap();
        if case.get("error").is_some() {
            // Dispatcher rejects missing/null/empty-string required fields before the core;
            // the direct provider still preserves Python's reached-core iterable errors.
            let error = fixture.raw_provider(&keys, &json!("w1:p1")).unwrap_err();
            assert_eq!(error.to_string(), case["error"].as_str().unwrap(), "{case}");
            fixture.assert_pristine();
            continue;
        }
        let result = fixture
            .dispatch(
                json!({"target":"self", "keys":keys,"hold_while_typing":false,"wait_s":0}),
                "w1:p1",
            )
            .unwrap();
        assert_eq!(result["status"], "sent", "{result}");
        assert_eq!(result["text"], case["text"]);
        assert_eq!(result["source"], "send_keys");
        assert_eq!(result["message_id"], "m1");
        assert_eq!(result["pane_id"], "w1:p1");
        let batches = fixture.batches.lock().unwrap();
        let delivered: Vec<String> = batches.iter().flat_map(|(_, keys)| keys.clone()).collect();
        assert_eq!(json!(delivered), case["keys"]);
        assert!(
            !delivered.iter().any(|key| key == "Enter"),
            "no implicit Enter"
        );
        assert!(fixture.flock.outbox_snapshot().is_empty());
    }
}

#[test]
fn raw_send_keys_queued_worker_preserves_arguments_and_provenance() {
    let fixture = Fixture::new();
    let whitespace: String = (0..=0x10ffff)
        .filter_map(char::from_u32)
        .filter(|ch| ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}'))
        .collect();
    assert_eq!(whitespace.chars().count(), 29);
    let result = fixture.dispatch(json!({"target":"self", "keys":["ctrl+c Enter",whitespace,false,{"z":1,"a":2}],"wait_s":0}), "w1:p1").unwrap();
    assert_eq!(result["status"], "queued");
    assert!(fixture.batches.lock().unwrap().is_empty());
    let expected = vec![
        "ctrl+c Enter".to_string(),
        "False".to_string(),
        "{'z': 1, 'a': 2}".to_string(),
    ];
    let queued = fixture.flock.outbox_snapshot();
    assert_eq!(queued["w1:p1"][0].text, expected.join(" "));
    let wire = crate::flock::sends::message_snapshot(&queued["w1:p1"][0]);
    assert!(wire.get("key_arguments").is_none());
    fixture.release_worker();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture
            .flock
            .message_history
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.id == "m1" && m.status == "sent")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "canonical worker did not deliver"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fixture
            .batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, keys)| keys.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(fixture
        .worker_threads
        .lock()
        .unwrap()
        .iter()
        .all(|id| *id != std::thread::current().id()));
    assert!(!fixture.worker_threads.lock().unwrap().is_empty());
}

#[test]
fn raw_send_keys_errors_preserve_auth_target_wait_and_allocation_order() {
    let fixture = Fixture::new();
    for (args, pane, expected) in [
        (
            json!({"target":"w2:p2","keys":false,"wait_s":"bad"}),
            "w1:p1",
            "send_keys to another pane requires a superuser",
        ),
        (
            json!({"target":"self","keys":false,"wait_s":"bad"}),
            "w1:p1",
            "could not convert string to float",
        ),
        (
            json!({"target":"missing","keys":false,"desk_token":"old"}),
            "w1:p1",
            "desk_token is retired",
        ),
        (
            json!({"target":"self","keys":false}),
            "w3:p3",
            "send_keys is not permitted for role desk",
        ),
        (
            json!({"target":"self","keys":false}),
            "w1:p1",
            "'bool' object is not iterable",
        ),
        (
            json!({"target":"self","keys":0}),
            "w1:p1",
            "'int' object is not iterable",
        ),
        (
            json!({"target":"self","keys":null}),
            "w1:p1",
            "missing required argument(s): keys",
        ),
        (
            json!({"target":"self","keys":""}),
            "w1:p1",
            "missing required argument(s): keys",
        ),
        (
            json!({"target":"self"}),
            "w1:p1",
            "missing required argument(s): keys",
        ),
        (
            json!({"target":"self","keys":[]}),
            "w1:p1",
            "keys are required",
        ),
    ] {
        let error = fixture
            .dispatch(args.clone(), pane)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{args}: {error}");
        fixture.assert_pristine();
    }
    let invalid_target = fixture
        .raw_provider(&json!(false), &json!("missing"))
        .unwrap_err()
        .to_string();
    assert!(!invalid_target.contains("iterable"));
    fixture.assert_pristine();
}
