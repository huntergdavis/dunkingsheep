//! Source snapshots from dunk_api.h_owner_push/poll/take and dunk_owner.OwnerPush.
//! Reuses the accepted t2987 fake terminal forwarding pattern; all stores are temporary.
use super::*;
use crate::api::Dispatcher;
use crate::config::Env;
use crate::flock::panes::Stores;
use crate::roles::Roles;
use crate::server::{Dispatcher as RuntimeDispatcher, RequestCtx};
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::tasks::Tasks;
use crate::terminal::{
    fakes::FakeTerminal, AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Observations {
    keys: Vec<(String, Vec<String>)>,
    threads: Vec<std::thread::ThreadId>,
}
#[derive(Debug)]
struct ObservedTerminal {
    inner: FakeTerminal,
    observations: Arc<Mutex<Observations>>,
    created_on: std::thread::ThreadId,
}
impl Drop for ObservedTerminal {
    fn drop(&mut self) {
        assert_eq!(self.created_on, std::thread::current().id());
        assert!(
            self.inner.sends.is_empty(),
            "owner queue must not type text"
        );
        assert!(
            self.inner.submits.is_empty(),
            "owner queue must not submit text"
        );
        assert!(
            self.inner.keys_sent.is_empty(),
            "owner queue must not send keys"
        );
        let mut observed = self.observations.lock().unwrap();
        observed.keys.extend(self.inner.keys_sent.clone());
        observed.threads.push(self.created_on);
    }
}
impl Terminal for ObservedTerminal {
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

struct Fixture {
    dispatcher: Arc<dyn RuntimeDispatcher>,
    _services: Arc<RuntimeServices>,
    path: PathBuf,
    system_token: String,
    web_token: String,
    observations: Arc<Mutex<Observations>>,
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let env = Env::default();
        let mut roles = Roles::new(
            &env,
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
        // Credentials are generated only into this fixture's private temporary directory.
        let system_token = roles.issue_token("system", Some("fixture"), None).unwrap();
        let web_token = roles.issue_token("web", Some("fixture"), None).unwrap();
        let executable = dir.path().join("test-daemon");
        std::fs::write(&executable, b"fixture identity").unwrap();
        let observations = Arc::new(Mutex::new(Observations::default()));
        let observed = Arc::clone(&observations);
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "fixture".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("metrics"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(move || {
                let mut terminal = FakeTerminal::default();
                for id in ["1", "7", "True", "1.5", "1e-07"] {
                    let mut pane = terminal.panes[0].clone();
                    pane.pane_id = id.into();
                    terminal.panes.push(pane);
                }
                for pane in &mut terminal.panes {
                    pane.pane_pid = None;
                }
                Box::new(ObservedTerminal {
                    inner: terminal,
                    observations: Arc::clone(&observed),
                    created_on: std::thread::current().id(),
                })
            }),
        })
        .unwrap();
        let path = if mode == "blocked" {
            let parent = dir.path().join("blocked");
            std::fs::write(&parent, b"not a directory").unwrap();
            parent.join("owner.db")
        } else {
            dir.path().join("owner.db")
        };
        if mode != "unconfigured" {
            services.attach_owner_push_path(path.clone()).unwrap();
        }
        let mut handlers = Dispatcher::new();
        handlers.register_owner_push_poll_take();
        assert_eq!(
            handlers.registration_attempts(),
            ["owner_push", "owner_poll", "owner_take"]
        );
        handlers.register_owner_stats_receipts_conversation();
        handlers.register_clear_box_delivery_capability();
        let dispatcher = services.dispatcher(handlers);
        Self {
            dispatcher,
            _services: services,
            path,
            system_token,
            web_token,
            observations,
            dir,
        }
    }
    fn ctx(&self, role: &str, pane: Option<&str>) -> RequestCtx {
        RequestCtx {
            self_pane_id: pane.map(str::to_owned),
            socket_peer: Some(true),
            service_token: match role {
                "system" => Some(self.system_token.clone()),
                "web" => Some(self.web_token.clone()),
                _ => None,
            },
            ..RequestCtx::default()
        }
    }
    fn call(
        &self,
        command: &str,
        args: Value,
        role: &str,
        pane: Option<&str>,
    ) -> Result<Value, DunkError> {
        self.dispatcher
            .dispatch(command, &args, &self.ctx(role, pane))
    }
    fn owner(&self, command: &str, args: Value) -> Result<Value, DunkError> {
        self.call(command, args, "owner", Some("w2:p2"))
    }
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).unwrap()
    }
    fn no_delivery_side_effects(&self) {
        let db = rusqlite::Connection::open(self.dir.path().join("inbox.db")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM messages", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let observed = self.observations.lock().unwrap();
        assert!(!observed.threads.is_empty());
        assert!(observed
            .threads
            .iter()
            .all(|thread| *thread == std::thread::current().id()));
        assert!(observed.keys.is_empty());
    }
}
fn error(result: Result<Value, DunkError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn h_owner_push_matches_python() {
    let fixture = Fixture::new("configured");
    assert!(!fixture.path.exists());
    for (role, pane) in [
        ("anonymous", None),
        ("worker", Some("w1:p1")),
        ("desk", Some("w3:p3")),
    ] {
        assert!(error(fixture.call("owner_push", json!({}), role, pane))
            .contains("not permitted for role"));
    }
    assert!(!fixture.path.exists());
    assert_eq!(
        error(fixture.owner("owner_push", json!({}))),
        "owner_push: missing required argument(s): target, body"
    );
    assert_eq!(
        error(fixture.owner("owner_push", json!({"target":"w1:p1","body":"x","extra":1}))),
        "owner_push: unknown argument(s): extra"
    );
    assert!(!fixture.path.exists());
    for target in [json!(false), json!(0), json!([]), json!({}), json!("  ")] {
        assert_eq!(
            error(fixture.owner("owner_push", json!({"target":target,"body":true}))),
            "a delivery target is required (a pane id like %9 or a pinned lane name)"
        );
    }
    assert!(!fixture.path.exists());
    let result = fixture
        .owner(
            "owner_push",
            json!({"target":"  w1:p1  ","body":"\u{001c} brief \u{001f}","subject":""}),
        )
        .unwrap();
    assert_eq!(
        result,
        json!({"id":1,"pane_id":"w1:p1","at":result["at"],"outcome":"stored","expect":"the lane's own harness drains it (owner_take); no inbox row, nothing typed into the pane"})
    );
    assert!(result["at"].as_f64().unwrap() > 0.0);
    let db = fixture.db();
    let row: (String, String, String, Option<f64>) = db
        .query_row(
            "SELECT target,subject,body,taken_at FROM owner_push WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(row, ("  w1:p1  ".into(), "".into(), "brief".into(), None));
    for body in [
        json!(false),
        json!(0),
        json!([]),
        json!({}),
        json!(" \u{001c}"),
    ] {
        assert_eq!(
            error(fixture.owner(
                "owner_push",
                json!({"target":"w1:p1","body":body,"subject":[]})
            )),
            "an owner message needs a body"
        );
    }
    for (body, kind) in [
        (json!(true), "bool"),
        (json!(1), "int"),
        (json!(1.5), "float"),
        (json!([1]), "list"),
        (json!({"a":1}), "dict"),
    ] {
        assert_eq!(
            error(fixture.owner("owner_push", json!({"target":"w1:p1","body":body}))),
            format!("'{kind}' object has no attribute 'strip'")
        );
    }
    for subject in [json!([]), json!({})] {
        assert!(error(fixture.owner(
            "owner_push",
            json!({"target":"w1:p1","body":"ok","subject":subject})
        ))
        .contains("Error binding parameter 3"));
    }
    for (target, subject, stored_target, stored_subject) in [
        (json!(true), json!(true), "1", "1"),
        (json!(7), json!(1.5), "7", "1.5"),
    ] {
        let pushed = fixture
            .call(
                "owner_push",
                json!({"target":target,"body":"scalar","subject":subject}),
                "web",
                None,
            )
            .unwrap();
        let stored: (String, String) = db
            .query_row(
                "SELECT target,subject FROM owner_push WHERE id=?1",
                [pushed["id"].as_i64().unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (stored_target.into(), stored_subject.into()));
    }
    assert!(error(fixture.owner(
        "owner_push",
        json!({"target":"w1:p1","body":"large","subject":u64::MAX})
    ))
    .contains("Python int too large to convert to SQLite INTEGER"));
    let small: Value = serde_json::from_str("1e-7").unwrap();
    let pushed = fixture
        .owner("owner_push", json!({"target":small,"body":"float target"}))
        .unwrap();
    assert_eq!(pushed["pane_id"], "1e-07");
    let stats = fixture.owner("owner_stats", json!({})).unwrap();
    assert_eq!(stats["stats"]["pushed"], 4);
    let infinity: Value = serde_json::from_str("1e999").unwrap();
    let pushed = fixture
        .owner(
            "owner_push",
            json!({"target":"w1:p1","body":"float overflow","subject":infinity}),
        )
        .unwrap();
    let stored: String = db
        .query_row(
            "SELECT subject FROM owner_push WHERE id=?1",
            [pushed["id"].as_i64().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "Inf");
    let huge: Value = serde_json::from_str("99999999999999999999999999999999999").unwrap();
    assert_eq!(
        error(fixture.owner("owner_push", json!({"target":"w1:p1","body":huge}))),
        "'int' object has no attribute 'strip'"
    );
    fixture.no_delivery_side_effects();
}

#[test]
fn h_owner_poll_matches_python() {
    let fixture = Fixture::new("configured");
    assert!(
        error(fixture.call("owner_poll", json!({}), "worker", Some("w1:p1")))
            .contains("not permitted for role worker")
    );
    assert_eq!(
        error(fixture.call("owner_poll", json!({"pane_id":true}), "system", None)),
        "'bool' object has no attribute 'strip'"
    );
    assert_eq!(error(fixture.call("owner_poll",json!({}),"system",None)),"owner_poll needs a caller pane: a heartbeat without an identity is a declaration, not an observation");
    assert_eq!(error(fixture.call("owner_poll",json!({"pane_id":"  other\n'pane  "}),"system",Some("w1:p1"))),"owner_poll for \"other\\n'pane\" refused: this call comes from 'w1:p1'. Only a lane may report its own polling.");
    assert!(!fixture.path.exists());
    for (pane, kind) in [
        (json!([1]), "list"),
        (json!({"x":1}), "dict"),
        (json!(1.5), "float"),
    ] {
        assert_eq!(
            error(fixture.call(
                "owner_poll",
                json!({"pane_id":pane}),
                "system",
                Some("w1:p1")
            )),
            format!("'{kind}' object has no attribute 'strip'")
        );
    }
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        let polled = fixture
            .call(
                "owner_poll",
                json!({"pane_id":value,"by":value}),
                "system",
                Some("w1:p1"),
            )
            .unwrap();
        assert_eq!(
            polled,
            json!({"pane_id":"w1:p1","at":polled["at"],"by":"harness"})
        );
    }
    let result = fixture
        .call(
            "owner_poll",
            json!({"pane_id":" w3:p3 ","by":true}),
            "desk",
            Some("w3:p3"),
        )
        .unwrap();
    assert_eq!(result["by"], true);
    let stored: String = fixture
        .db()
        .query_row("SELECT by FROM owner_poll WHERE pane_id='w3:p3'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stored, "1");
    let cap = fixture
        .owner("delivery_capability", json!({"target":"w1:p1"}))
        .unwrap();
    assert_eq!(cap["drains"], true);
    assert_eq!(cap["send"], "owner_push");
    let before: f64 = fixture
        .db()
        .query_row("SELECT at FROM owner_poll WHERE pane_id='w1:p1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    for by in [json!([1]), json!({"x":1})] {
        assert!(
            error(fixture.call("owner_poll", json!({"by":by}), "system", Some("w1:p1")))
                .contains("Error binding parameter 3")
        );
    }
    let after: f64 = fixture
        .db()
        .query_row("SELECT at FROM owner_poll WHERE pane_id='w1:p1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT count(*) FROM owner_push", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    fixture.no_delivery_side_effects();
}

#[test]
fn h_owner_take_matches_python() {
    let fixture = Fixture::new("configured");
    assert_eq!(
        error(fixture.call("owner_take", json!({}), "system", None)),
        "owner_take needs a pane"
    );
    assert!(!fixture.path.exists());
    for i in 0..21 {
        fixture
            .owner(
                "owner_push",
                json!({"target":"w1:p1","body":format!("message {i}")}),
            )
            .unwrap();
    }
    fixture
        .db()
        .execute(
            "UPDATE owner_push SET taken_by='previous attribution' WHERE id=1",
            [],
        )
        .unwrap();
    for by in [json!([1]), json!({"x":1})] {
        assert!(error(fixture.call(
            "owner_take",
            json!({"pane_id":"w1:p1","by":by}),
            "system",
            None
        ))
        .contains("Error binding parameter 2"));
    }
    assert_eq!(
        fixture
            .db()
            .query_row(
                "SELECT count(*) FROM owner_push WHERE taken_at IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        21
    );
    let taken = fixture
        .call("owner_take", json!({"by":true}), "system", Some("w1:p1"))
        .unwrap();
    assert_eq!(taken["pane_id"], "w1:p1");
    let rows = taken["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 20);
    assert_eq!(
        rows[0],
        json!({"id":1,"pane_id":"w1:p1","target":"w1:p1","subject":null,"body":"message 0","created_at":rows[0]["created_at"],"taken_at":null,"taken_by":"previous attribution"})
    );
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["body"], format!("message {i}"));
        assert!(row["taken_at"].is_null());
    }
    let committed: (i64, String) = fixture
        .db()
        .query_row(
            "SELECT count(*),min(taken_by) FROM owner_push WHERE taken_at IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(committed, (20, "1".into()));
    let receipt = fixture
        .owner("owner_receipts", json!({"push_ids":[1]}))
        .unwrap();
    assert!(receipt["receipts"][0]["taken_at"].as_f64().unwrap() > 0.0);
    let cap = fixture
        .owner("delivery_capability", json!({"target":"w1:p1"}))
        .unwrap();
    assert_eq!(cap["drains"], false, "take must not record a heartbeat");
    let rest = fixture
        .call(
            "owner_take",
            json!({"pane_id":"w1:p1","by":""}),
            "desk",
            Some("w3:p3"),
        )
        .unwrap();
    assert_eq!(rest["messages"].as_array().unwrap().len(), 1);
    assert_eq!(rest["messages"][0]["body"], "message 20");
    assert_eq!(
        fixture
            .db()
            .query_row("SELECT taken_by FROM owner_push WHERE id=21", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        "w1:p1"
    );
    // Python never binds by when SELECT found no rows, even for an unsupported value.
    let empty = fixture
        .call(
            "owner_take",
            json!({"pane_id":"w1:p1","by":[1]}),
            "system",
            None,
        )
        .unwrap();
    assert_eq!(empty, json!({"pane_id":"w1:p1","messages":[]}));
    for pane in [json!([1]), json!({"a":1})] {
        assert!(error(fixture.owner("owner_take", json!({"pane_id":pane})))
            .contains("Error binding parameter 1"));
    }
    fixture
        .owner("owner_push", json!({"target":7,"body":"numeric pane"}))
        .unwrap();
    let numeric = fixture
        .call("owner_take", json!({"pane_id":7}), "system", None)
        .unwrap();
    assert_eq!(numeric["pane_id"], 7);
    assert_eq!(numeric["messages"][0]["pane_id"], "7");
    assert_eq!(
        fixture
            .db()
            .query_row(
                "SELECT taken_by FROM owner_push WHERE pane_id='7'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "7"
    );
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        fixture
            .owner("owner_push", json!({"target":"w2:p2","body":"fallback"}))
            .unwrap();
        let taken = fixture
            .owner("owner_take", json!({"pane_id":value,"by":value}))
            .unwrap();
        assert_eq!(taken["pane_id"], "w2:p2");
        assert_eq!(taken["messages"].as_array().unwrap().len(), 1);
        assert!(taken["messages"][0]["taken_at"].is_null());
    }
    let names: i64 = fixture
        .db()
        .query_row(
            "SELECT count(*) FROM owner_push WHERE pane_id='w2:p2' AND taken_by='w2:p2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(names, 6);
    fixture.no_delivery_side_effects();
}

#[test]
fn owner_handler_error_order_lazy_retry_and_missing_provider() {
    let fixture = Fixture::new("blocked");
    let invalid = error(fixture.owner("owner_push", json!({"target":"not-a-lane","body":true})));
    assert!(invalid.contains("reason=unknown_name"), "{invalid}");
    let invalid = error(fixture.owner("owner_push", json!({"target":"w99:p99","body":true})));
    assert!(invalid.contains("reason=dead_lane"), "{invalid}");
    fixture
        ._services
        .flock()
        .remember_lane("w99:p99", "fixture-ghost", true);
    let invalid = error(fixture.owner("owner_push", json!({"target":"fixture-ghost","body":true})));
    assert!(invalid.contains("reason=unhoused_lane"), "{invalid}");
    let open_error = error(fixture.owner("owner_push", json!({"target":"w1:p1","body":true})));
    assert!(
        !open_error.contains("strip"),
        "store opening precedes body strip: {open_error}"
    );
    let poll = error(fixture.call(
        "owner_poll",
        json!({"pane_id":"another"}),
        "system",
        Some("w1:p1"),
    ));
    assert!(poll.starts_with("owner_poll for"));
    assert_eq!(
        error(fixture.call("owner_take", json!({}), "system", None)),
        "owner_take needs a pane"
    );
    std::fs::remove_file(fixture.path.parent().unwrap()).unwrap();
    let result = fixture
        .owner(
            "owner_push",
            json!({"target":"w1:p1","body":"after repair"}),
        )
        .unwrap();
    assert_eq!(result["outcome"], "stored");
    assert!(fixture.path.is_file());
    let missing = Fixture::new("unconfigured");
    for (cmd, args) in [
        ("owner_push", json!({"target":"w1:p1","body":"x"})),
        ("owner_poll", json!({})),
        ("owner_take", json!({})),
    ] {
        assert_eq!(
            error(missing.owner(cmd, args)),
            "runtime owner push path is not wired"
        );
    }
    assert!(!missing.path.exists());
    fixture.no_delivery_side_effects();
}
