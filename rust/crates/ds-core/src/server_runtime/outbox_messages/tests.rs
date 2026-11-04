use crate::config::Env;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::flock::sends::Message;
use crate::metrics::MetricsStore;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::tasks::Tasks;
use crate::terminal::fakes::FakeTerminal;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::runtime::Builder;

fn make_services(
    tag: &str,
    terminal_factory: LoopTerminalFactory,
    metrics: Option<Arc<MetricsStore>>,
) -> (tempfile::TempDir, Arc<RuntimeServices>) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(dir.path().join(format!("{tag}-tasks.db"))))
            .unwrap(),
    );
    runtime.block_on(tasks.allocation_init()).unwrap();
    let inbox = Arc::new(
        crate::inbox_store::Store::open(
            dir.path().join(format!("{tag}-inbox.db")),
            dir.path().join(format!("{tag}-blobs")),
        )
        .unwrap(),
    );
    let env = Env::default();
    let mut roles = Roles::new(
        &env,
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    roles
        .register(
            "w1:p1",
            "worker",
            None,
            None,
            Some("codex"),
            None,
            None,
            None,
        )
        .unwrap();
    roles
        .register("w2:p2", "owner", None, None, None, None, None, None)
        .unwrap();
    let executable = dir.path().join(format!("{tag}-daemon"));
    std::fs::write(&executable, b"test daemon identity").unwrap();
    let stores = Stores {
        tasks: Some(tasks),
        inbox: Some(inbox),
        metrics,
        ..Stores::default()
    };
    let observability = stores.metrics.is_none().then(|| RuntimeObservability {
        state_dir: dir.path().join(format!("{tag}-state")),
        memory_db: None,
    });
    let services = RuntimeServices::new(RuntimeServiceInputs {
        launches_file: dir.path().join(format!("{tag}-launches.json")),
        lane_registry_file: dir.path().join(format!("{tag}-lanes.json")),
        persist_enabled: true,
        stores,
        roles,
        legacy_superusers: None,
        release_version: "test-release".into(),
        state_file: dir.path().join(format!("{tag}-whole-state.json")),
        executable,
        env,
        observability,
        runtime,
        terminal_factory,
    })
    .unwrap();
    (dir, services)
}

fn request(pane: &str) -> RequestCtx {
    RequestCtx {
        caller: Some(crate::proc::Caller::new(
            Some(123),
            Some("start".into()),
            Some("codex".into()),
            Some(pane.into()),
            "agent",
        )),
        self_pane_id: Some(pane.into()),
        socket_peer: Some(true),
        ..RequestCtx::default()
    }
}

fn put(services: &RuntimeServices, id: &str, pane: &str, at: f64) -> Message {
    let mut message = Message::new(id, pane, "target", "hello", "send_text");
    message.created_at = at;
    services
        .flock()
        .outbox
        .lock()
        .unwrap()
        .entry(pane.into())
        .or_default()
        .push(message.clone());
    message
}

#[test]
fn h_list_messages_matches_python() {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let factory: LoopTerminalFactory = {
        let reads = Arc::clone(&reads);
        Arc::new(move || {
            let mut terminal = FakeTerminal::new();
            terminal.reads = Arc::clone(&reads);
            terminal
                .screens
                .insert("w1:p1".into(), "❯ foreign draft".into());
            terminal.screens.insert("w2:p2".into(), "❯ hello".into());
            terminal.unreadable.insert("w3:p3".into());
            Box::new(terminal)
        })
    };
    let (_dir, services) = make_services("list", factory, None);
    put(&services, "m2", "w1:p1", 20.0);
    put(&services, "m1", "w1:p1", 10.0);
    put(&services, "m3", "w2:p2", 30.0);
    put(&services, "m4", "w3:p3", 40.0);
    services.flock().remember_typed("w2:p2", "hello");
    let mut old = Message::new("old", "private", "private target", "secret", "send_text");
    old.status = "sent".into();
    old.finished_at = Some(1.0);
    services.flock().remember(&old);
    let mut newer = old.clone();
    newer.id = "newer".into();
    services.flock().remember(&newer);
    let before = services.flock().outbox_snapshot();
    let dispatcher = services.production_dispatcher();
    let got = dispatcher
        .dispatch("list_messages", &json!({}), &request("w1:p1"))
        .unwrap();
    let queued = got["queued"].as_array().unwrap();
    assert_eq!(
        queued
            .iter()
            .map(|v| v["message_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["m1", "m2", "m3", "m4"]
    );
    assert_eq!(queued[0]["blocked_by"], "input box contains a draft");
    assert_eq!(queued[1]["blocked_by"], "input box contains a draft");
    assert_eq!(queued[2]["blocked_by"], "guarded send pending");
    assert_eq!(queued[3]["blocked_by"], Value::Null);
    assert_eq!(got["recent"], json!([newer.to_dict(), old.to_dict()]));
    assert_eq!(
        queued[0].as_object().unwrap().len(),
        old.to_dict().as_object().unwrap().len() + 1
    );
    for pane in ["w1:p1", "w2:p2", "w3:p3"] {
        assert_eq!(
            reads
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _, _)| id == pane)
                .count(),
            1
        );
    }
    assert_eq!(before, services.flock().outbox_snapshot());
    services.shutdown();
}

#[test]
fn h_get_message_matches_python() {
    let (_dir, services) = make_services("get", Arc::new(|| Box::new(FakeTerminal::new())), None);
    let message = put(&services, "m1", "private", 10.0);
    let mut old = message.clone();
    old.status = "sent".into();
    services.flock().remember(&old);
    let dispatcher = services.production_dispatcher();
    let ctx = request("w1:p1");
    let got = dispatcher
        .dispatch("get_message", &json!({"message_id":"m1"}), &ctx)
        .unwrap();
    assert_eq!(got["status"], "queued");
    assert_eq!(got["pane_id"], "private");
    assert_eq!(
        got.as_object().unwrap().len(),
        message.to_dict().as_object().unwrap().len()
    );
    for (id, repr) in [
        (json!(false), "False"),
        (json!(0), "0"),
        (json!(["m1", 1]), "['m1', 1]"),
        (json!({"x":true}), "{'x': True}"),
    ] {
        assert_eq!(
            dispatcher
                .dispatch("get_message", &json!({"message_id":id}), &ctx)
                .unwrap_err()
                .to_string(),
            format!("no message with id {repr}")
        );
    }
    // dunk_api.dispatch:2997 rejects null/empty before the raw-ID producer.
    for id in [Value::Null, json!("")] {
        assert_eq!(
            dispatcher
                .dispatch(
                    "get_message",
                    &json!({"message_id": id, "extra": true}),
                    &ctx
                )
                .unwrap_err()
                .to_string(),
            "get_message: missing required argument(s): message_id"
        );
    }
    assert_eq!(
        services
            .flock()
            .get_message(&Value::Null)
            .unwrap_err()
            .to_string(),
        "no message with id None"
    );
    assert_eq!(
        services
            .flock()
            .get_message(&json!(""))
            .unwrap_err()
            .to_string(),
        "no message with id ''"
    );
    assert_eq!(
        dispatcher
            .dispatch("get_message", &json!({}), &ctx)
            .unwrap_err()
            .to_string(),
        "get_message: missing required argument(s): message_id"
    );
    services.flock().outbox.lock().unwrap().clear();
    assert_eq!(
        dispatcher
            .dispatch("get_message", &json!({"message_id":"m1"}), &ctx)
            .unwrap(),
        old.to_dict()
    );
    services.shutdown();
}

#[test]
fn h_cancel_message_matches_python() {
    let (_dir, services) =
        make_services("cancel", Arc::new(|| Box::new(FakeTerminal::new())), None);
    let message = put(&services, "m1", "private", 10.0);
    let dispatcher = services.production_dispatcher();
    let worker = request("w1:p1");
    assert!(dispatcher
        .dispatch("cancel_message", &json!({"message_id":"m1"}), &worker)
        .is_err());
    assert!(!message.done.is_set());
    let owner = request("w2:p2");
    let got = dispatcher
        .dispatch("cancel_message", &json!({"message_id":"m1"}), &owner)
        .unwrap();
    assert_eq!(got["status"], "cancelled");
    assert_eq!(got["ok"], false);
    assert_eq!(got["queued"], false);
    assert_eq!(got["waiting_s"], Value::Null);
    assert!(got["finished_at"].is_number());
    assert!(message.done.is_set());
    assert!(services.flock().outbox_snapshot()["private"].is_empty());
    assert_eq!(
        services.flock().decided.lock().unwrap().by_id["m1"].status,
        "cancelled"
    );
    assert_eq!(
        dispatcher
            .dispatch("get_message", &json!({"message_id":"m1"}), &worker)
            .unwrap(),
        got
    );
    assert_eq!(
        dispatcher
            .dispatch("cancel_message", &json!({"message_id":"m1"}), &owner)
            .unwrap_err()
            .to_string(),
        "message m1 already cancelled"
    );
    assert_eq!(
        dispatcher
            .dispatch("cancel_message", &json!({"message_id":[]}), &owner)
            .unwrap_err()
            .to_string(),
        "no queued message with id []"
    );
    let snapshot = services.flock().snapshot();
    assert_eq!(snapshot["outbox"], json!([]));
    assert_eq!(snapshot["decided"]["m1"]["status"], "cancelled");
    services.shutdown();
}

use crate::terminal::{AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace};
use std::collections::BTreeMap;
use std::sync::{mpsc, Condvar};
use std::time::Duration;

#[derive(Debug)]
struct BarrierTerminal {
    inner: FakeTerminal,
    entered: mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}
impl Terminal for BarrierTerminal {
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

    fn get_pane(&self, pane_id: &str) -> Option<Pane> {
        self.inner.get_pane(pane_id)
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

    fn try_list_panes_grouped(&self) -> Result<BTreeMap<String, Vec<Pane>>, TerminalError> {
        self.inner.try_list_panes_grouped()
    }

    fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
        self.inner.agent_child_pids(pane_pid)
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
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        self.inner.create_tab(workspace_id, label, cwd, focus)
    }

    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError> {
        self.inner.split_pane(pane_id, direction, cwd)
    }

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        self.inner.start_agent(spec)
    }

    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<(), TerminalError> {
        self.inner.rename(kind, target_id, label)
    }

    fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        self.inner.close(kind, target_id)
    }

    fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        self.inner.focus(kind, target_id)
    }

    fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
        self.inner.run_command(pane_id, command)
    }

    fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        self.entered.send(()).unwrap();
        let (mutex, changed) = &*self.release;
        let released = mutex.lock().unwrap();
        let (released, timeout) = changed
            .wait_timeout_while(released, Duration::from_secs(15), |ready| !*ready)
            .unwrap();
        assert!(
            *released && !timeout.timed_out(),
            "worker barrier not released"
        );
        self.inner.send_text(pane_id, text)
    }

    fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        self.inner.send_text_and_enter(pane_id, text)
    }

    fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
        self.inner.send_keys(pane_id, keys)
    }

    fn read_pane(
        &self,
        pane_id: &str,
        lines: i64,
        ansi: bool,
    ) -> Result<Option<String>, TerminalError> {
        self.inner.read_pane(pane_id, lines, ansi)
    }

    fn agent_status(&self, pane_id: &str) -> Option<String> {
        self.inner.agent_status(pane_id)
    }

    fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
        self.inner.resize(pane_id, cols, rows)
    }
}

#[test]
fn outbox_cancel_inflight_worker_preserves_replacement_lifetime() {
    use crate::flock::sends::OutboxWake;
    let (entered_tx, entered_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let factory: LoopTerminalFactory = {
        let release = Arc::clone(&release);
        Arc::new(move || {
            Box::new(BarrierTerminal {
                inner: FakeTerminal::new(),
                entered: entered_tx.clone(),
                release: Arc::clone(&release),
            })
        })
    };
    let (_dir, services) = make_services("inflight", factory, None);
    let store = services.flock().stores().inbox.unwrap();
    let pool = crate::db::ReadPool::open(store.path(), 1).unwrap();
    let receipt = |body: &str| {
        let id = store
            .send(&crate::inbox_store::Outgoing {
                mailbox: "worker",
                body: Some(body),
                ..Default::default()
            })
            .unwrap();
        store
            .record_notify(&id, "queued", Some("awaiting notice"), None, None)
            .unwrap();
        id
    };
    let original_receipt = receipt("original");
    let replacement_receipt = receipt("replacement");
    let mut message = put(&services, "same", "w1:p1", 10.0);
    message.is_notice = true;
    message.notify_for = vec![original_receipt.clone()];
    message.hold_while_typing = false;
    message.press_enter = false;
    services
        .flock()
        .outbox
        .lock()
        .unwrap()
        .insert("w1:p1".into(), vec![message.clone()]);
    services
        .outbox
        .pool
        .ensure_worker(services.flock(), "w1:p1")
        .unwrap();
    let reached = entered_rx.recv_timeout(Duration::from_secs(10));
    if reached.is_err() {
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        services.shutdown();
        panic!("worker did not reach guarded terminal call: {reached:?}");
    }
    let dispatcher = services.production_dispatcher();
    let cancelled = dispatcher
        .dispatch(
            "cancel_message",
            &json!({"message_id":"same"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert!(message.done.is_set());
    let mut replacement = Message::new("same", "w3:p3", "replacement", "new body", "new source");
    replacement.is_notice = true;
    replacement.notify_for = vec![replacement_receipt.clone()];
    replacement.created_at = message.created_at;
    replacement.attempt_generation = Some("replacement-generation".into());
    services
        .flock()
        .outbox
        .lock()
        .unwrap()
        .insert("w3:p3".into(), vec![replacement.clone()]);
    // The same-ID replacement belongs to another pane and has no worker. Shutdown joins
    // the original callback outside the queue lock.
    let shutdown_services = Arc::clone(&services);
    let shutdown = std::thread::spawn(move || shutdown_services.shutdown());
    *release.0.lock().unwrap() = true;
    release.1.notify_all();
    shutdown.join().unwrap();
    let queued = services.flock().outbox_snapshot();
    assert_eq!(queued["w3:p3"][0].text, "new body");
    assert_eq!(
        queued["w3:p3"][0].attempt_generation.as_deref(),
        Some("replacement-generation")
    );
    assert!(!replacement.done.is_set());
    let history = services.flock().message_history.lock().unwrap();
    assert!(history
        .iter()
        .any(|row| row.id == "same" && row.status == "sent"));
    assert!(history.iter().all(|row| row.text != "new body"));
    assert_eq!(
        crate::inbox::status(&pool, &original_receipt, crate::clock::now_s())
            .unwrap()
            .notify_state
            .as_deref(),
        Some("sent")
    );
    assert_eq!(
        crate::inbox::status(&pool, &replacement_receipt, crate::clock::now_s())
            .unwrap()
            .notify_state
            .as_deref(),
        Some("queued")
    );
}

#[test]
fn outbox_cancel_real_filesystem_error_keeps_committed_memory() {
    let (dir, services) =
        make_services("io-error", Arc::new(|| Box::new(FakeTerminal::new())), None);
    let message = put(&services, "m", "w1:p1", 1.0);
    // Atomic replacement of an existing directory fails with a real filesystem error.
    let path = dir.path().join("io-error-whole-state.json");
    if path.is_file() {
        std::fs::remove_file(&path).unwrap();
    }
    std::fs::create_dir(&path).unwrap();
    let dispatcher = services.production_dispatcher();
    let got = dispatcher
        .dispatch(
            "cancel_message",
            &json!({"message_id":"m"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(got["status"], "cancelled");
    assert!(message.done.is_set());
    assert!(path.is_dir());
    assert_eq!(
        services.flock().decided.lock().unwrap().by_id["m"].status,
        "cancelled"
    );
    services.shutdown();
}

#[test]
fn outbox_message_provider_requires_canonical_flock() {
    use crate::api::CommandEnv;
    use crate::server_runtime::RequestRuntimeAdapter;
    let (dir, services) =
        make_services("provider", Arc::new(|| Box::new(FakeTerminal::new())), None);
    let request = request("w2:p2");
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let adapter = RequestRuntimeAdapter::new(
        services.flock(),
        &request,
        services.runtime.handle(),
        &terminal,
    );
    assert_eq!(
        adapter
            .with_outbox_commands(&mut |env| env.list_messages())
            .unwrap_err()
            .to_string(),
        "request outbox service is not attached"
    );
    let other = crate::flock::Flock::new(
        dir.path().join("other-launches"),
        dir.path().join("other-lanes"),
        false,
    );
    let adapter =
        RequestRuntimeAdapter::new(&other, &request, services.runtime.handle(), &terminal);
    assert_eq!(
        adapter
            .with_outbox(&services.outbox)
            .err()
            .unwrap()
            .to_string(),
        "request outbox flock mismatch"
    );
    services.shutdown();
}
