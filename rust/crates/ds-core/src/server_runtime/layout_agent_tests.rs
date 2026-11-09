use super::*;
use crate::api;
use crate::config::Env;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::roles::Roles;
use crate::server::{Dispatcher, RequestCtx};
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::tasks::Tasks;
use crate::terminal::fakes::FakeTerminal;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::runtime::Builder;

type LayoutFixture = (
    tempfile::TempDir,
    Arc<RuntimeServices>,
    Arc<Mutex<Vec<Value>>>,
);
struct FakeAgent(std::process::Child);
impl Drop for FakeAgent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fake_agent(dir: &tempfile::TempDir) -> FakeAgent {
    let path = dir.path().join("codex");
    std::os::unix::fs::symlink("/usr/bin/sleep", &path).unwrap();
    FakeAgent(std::process::Command::new(path).arg("60").spawn().unwrap())
}
fn make_services(fail_layout: Option<&str>, available: bool) -> LayoutFixture {
    make_services_with_factory(fail_layout, available, None)
}
fn make_services_with_factory(
    fail_layout: Option<&str>,
    available: bool,
    factory: Option<LoopTerminalFactory>,
) -> LayoutFixture {
    let dir = tempfile::tempdir().unwrap();
    let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
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
    let env = Env::from_pairs(&[("DUNKINGSHEEP_AGENT_VERIFY_S", "0.01")]);
    let mut roles = Roles::new(
        &env,
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
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
    let executable = dir.path().join("daemon");
    std::fs::write(&executable, b"test daemon identity").unwrap();
    let stores = Stores {
        tasks: Some(tasks),
        inbox: Some(inbox),
        ..Stores::default()
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let fail_layout = fail_layout.map(str::to_owned);
    let terminal_factory: LoopTerminalFactory = Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.raw_events = Arc::clone(&observed);
        terminal.show_created_tab_pane = true;
        terminal.start_agent_pid = Some(Some(i64::from(std::process::id())));
        // This fixture uses coherent session IDs across the canned pane/tab/workspace rows.
        for pane in &mut terminal.panes {
            pane.pane_pid = Some(i64::from(std::process::id()));
            pane.session_id = terminal
                .tabs
                .iter()
                .find(|tab| tab.tab_id == pane.window_id)
                .unwrap()
                .workspace_id
                .clone();
        }
        terminal.fail_layout = fail_layout.clone();
        terminal.available = available;
        Box::new(terminal)
    });
    let terminal_factory = factory.unwrap_or(terminal_factory);
    let services = RuntimeServices::new(RuntimeServiceInputs {
        launches_file: dir.path().join("launches.json"),
        lane_registry_file: dir.path().join("lanes.json"),
        persist_enabled: false,
        stores,
        roles,
        legacy_superusers: None,
        release_version: "test".into(),
        state_file: dir.path().join("state.json"),
        executable,
        env,
        observability: Some(RuntimeObservability {
            state_dir: dir.path().join("observability"),
            memory_db: None,
        }),
        runtime,
        terminal_factory,
    })
    .unwrap();
    (dir, services, events)
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
fn dispatcher(services: &Arc<RuntimeServices>) -> super::super::dispatcher::RuntimeDispatcher {
    let mut handlers = api::Dispatcher::new();
    handlers.register_layout_agent();
    handlers.register_lanes();
    super::super::dispatcher::RuntimeDispatcher::new(Arc::clone(services), handlers)
}

#[test]
fn h_start_agent_records_explicit_global_membership() {
    let (dir, services, events) = make_services(None, true);
    let _agent = fake_agent(&dir);
    let out=dispatcher(&services).dispatch("start_agent",&json!({"global":true,"name":" probe ","command":"codex --model 'test model'","workspace":"self","cwd":["raw",false],"focus":"false"}),&request("w2:p2")).unwrap();
    assert_eq!(out["name"], "probe");
    assert_eq!(out["argv"], json!(["codex", "--model", "test model"]));
    assert_eq!(out["pane_id"], "w1:p-probe");
    assert!(out["child_pid"].is_number());
    assert_eq!(out["role"], Value::Null);
    assert_eq!(out["membership"]["scope"], "global");
    assert_eq!(services.flock.last_message(), "Started agent probe");
    let launch = services.flock.launch("w1:p-probe").unwrap();
    assert_eq!(launch["cwd"], json!(["raw", false]));
    assert_eq!(launch["command"], "codex --model 'test model'");
    assert!(events.lock().unwrap().contains(&json!([
        "start_agent",
        "probe",
        ["codex", "--model", "test model"],
        "w2",
        null,
        null,
        "['raw', False]",
        false
    ])));
}
#[test]
fn h_rename_matches_python() {
    let (_dir, services, events) = make_services(None, true);
    services.flock.set_last_message("before");
    let out = dispatcher(&services)
        .dispatch(
            "rename",
            &json!({"kind":" PANE ","target":{"pane_id":"w1:p1"},"label":["raw",false]}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(out["kind"], "pane");
    assert_eq!(out["id"], "w1:p1");
    assert_eq!(out["label"], "['raw', False]");
    assert_eq!(out["pane"]["label"], "['raw', False]");
    assert_eq!(services.flock.last_message(), "before");
    assert!(events
        .lock()
        .unwrap()
        .contains(&json!(["rename", "pane", "w1:p1", "['raw', False]"])));
}
#[test]
fn h_focus_matches_python() {
    let (_dir, services, _) = make_services(None, true);
    services.flock.set_last_message("before");
    let out = dispatcher(&services)
        .dispatch(
            "focus",
            &json!({"kind":"workspace","target":"self"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(out, json!({"kind":"workspace","id":"w2","focused":true}));
    assert_eq!(services.flock.last_message(), "before");
}
#[test]
fn layout_agent_conversion_and_error_order() {
    let (_dir, services, events) = make_services(None, true);
    let d = dispatcher(&services);
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        let error = d
            .dispatch(
                "start_agent",
                &json!({"name":value,"command":"'","split":"bad"}),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            if value.is_null() || value == "" {
                "start_agent: missing required argument(s): name"
            } else {
                "agent name is required"
            }
        );
        let error = d
            .dispatch(
                "rename",
                &json!({"kind":"bad","target":"missing","label":value}),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            if value.is_null() || value == "" {
                "rename: missing required argument(s): label"
            } else {
                "label is required"
            }
        );
    }
    assert_eq!(
        d.dispatch(
            "start_agent",
            &json!({"name":"probe","command":"'","tab":"missing"}),
            &request("w2:p2")
        )
        .unwrap_err()
        .to_string(),
        "No closing quotation"
    );
    assert!(events.lock().unwrap().is_empty());
    let error = d
        .dispatch(
            "start_agent",
            &json!({"global":true,"name":"probe","command":"codex","split":["right"]}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "split must be 'right' or 'down'");
    assert!(events.lock().unwrap().iter().any(|e| e[0] == "workspace"));
    assert!(services.flock.launch("w1:p-probe").is_none());
}
#[test]
fn layout_agent_role_failure_keeps_launch_before_last_message() {
    let (dir, services, _) = make_services(None, true);
    let _agent = fake_agent(&dir);
    services.flock.set_last_message("before");
    let error = dispatcher(&services)
        .dispatch(
            "start_agent",
            &json!({"global":true,"name":"probe","command":"codex","workspace":"self","role":"invalid"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("role"), "{error}");
    assert!(services.flock.launch("w1:p-probe").is_some());
    assert_eq!(services.flock.last_message(), "before");
}
#[test]
fn layout_agent_dispatch_permission_precedes_required_arguments() {
    let (_dir, services, events) = make_services(None, true);
    for command in ["start_agent", "rename", "focus"] {
        let error = dispatcher(&services)
            .dispatch(command, &json!({}), &request("w1:p1"))
            .unwrap_err();
        assert!(
            error.to_string().contains("permission")
                || error.to_string().contains("role")
                || error.to_string().contains("operator"),
            "{error}"
        );
    }
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn layout_agent_role_success_preserves_raw_fields_and_final_message() {
    let (dir, services, _) = make_services(None, true);
    let _agent = fake_agent(&dir);
    let out=dispatcher(&services).dispatch("start_agent",&json!({"global":true,"name":"probe","command":"codex","workspace":"self","role":"worker","provider":false,"model":[]}),&request("w2:p2")).unwrap();
    assert_eq!(out["role"]["provider"], "False");
    assert_eq!(out["role"]["model"], "[]");
    assert_eq!(out["role"]["label"], "probe");
    assert_eq!(services.flock.last_message(), "Started agent probe");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("roles.json")).unwrap()).unwrap();
    assert_eq!(saved["lanes"]["w1:p-probe"]["role"], "worker");
}
#[test]
fn layout_agent_verification_failure_precedes_launch_and_role_writeback() {
    let factory: LoopTerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.start_agent_pid = Some(None);
        terminal
            .screens
            .insert("w1:p-probe".into(), "[agent exited rc=127]".into());
        Box::new(terminal)
    });
    let (dir, services, _) = make_services_with_factory(None, true, Some(factory));
    services.flock.set_last_message("before");
    let error = dispatcher(&services)
        .dispatch(
            "start_agent",
            &json!({"global":true,"name":"probe","command":"codex","workspace":"w2","role":"worker"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent 'probe' exited immediately in pane w1:p-probe: '[agent exited rc=127]'"
    );
    assert!(services.flock.launch("w1:p-probe").is_none());
    assert_eq!(services.flock.last_message(), "before");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("roles.json")).unwrap()).unwrap();
    assert!(saved["lanes"].get("w1:p-probe").is_none());
}

#[test]
fn layout_agent_bridge_uses_same_terminal_and_releases_each_lock() {
    use crate::api::CommandEnv;
    use crate::flock::{status_runtime::StatusState, Flock};
    use crate::terminal::Terminal;
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let executable = dir.path().join("daemon");
    std::fs::write(&executable, b"fixture").unwrap();
    flock
        .attach_status_state(StatusState::at_start(
            "test",
            dir.path().join("state"),
            executable,
            Env::from_pairs(&[("DUNKINGSHEEP_AGENT_VERIFY_S", "0.01")]),
        ))
        .unwrap();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let request = RequestCtx::default();
    let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
    adapter
        .with_layout_agent(&mut |layout, panes, verifier, _, window| {
            assert_eq!(window, 0.01);
            assert!(terminal.try_lock().is_ok());
            let started = layout
                .start_agent_raw(
                    "probe",
                    &["codex".into()],
                    Some("w2"),
                    None,
                    None,
                    None,
                    false,
                )
                .unwrap()
                .unwrap();
            let pane = started["pane_id"].as_str().unwrap();
            assert!(terminal.try_lock().is_ok());
            assert_eq!(panes.get_pane(pane).unwrap()["pane_id"], pane);
            assert_eq!(verifier.get_pane(pane).unwrap()["pane_id"], pane);
            verifier.read_pane(pane, 20);
            assert!(terminal.try_lock().is_ok());
            Ok(json!({}))
        })
        .unwrap();
}
#[test]
fn layout_agent_raw_numbers_and_composites_preserve_python_strings() {
    let (dir, services, _) = make_services(None, true);
    let _agent = fake_agent(&dir);
    for (raw, expected) in [
        ("true", "True"),
        ("1e999", "inf"),
        ("-1e999", "-inf"),
        ("9007199254740993", "9007199254740993"),
        ("[1,false]", "[1, False]"),
    ] {
        let value: Value = serde_json::from_str(raw).unwrap();
        let out = dispatcher(&services)
            .dispatch(
                "rename",
                &json!({"kind":"pane","target":"w1:p1","label":value}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(out["label"], expected);
    }
    let out = dispatcher(&services)
        .dispatch(
            "start_agent",
            &json!({"global":true,"name":"\u{1c}probe\u{1f}","command":"codex","workspace":"self"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(out["name"], "probe");
}

#[test]
fn layout_rename_runtime_returns_pre_save_item_without_rereading() {
    use crate::terminal::tmux::TmuxBackend;
    use std::os::unix::fs::PermissionsExt;
    for previously_managed in [false, true] {
        let disk = tempfile::tempdir().unwrap();
        let script = disk.path().join("tmux");
        std::fs::write(&script,r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
case "$1" in
select-pane) printf '%s' "$5" > title ;;
list-panes)
 printf 'w2:p2__SEP__$old__SEP__@old__SEP__/work__SEP__bash__SEP__owner__SEP__1__SEP__owner__SEP__owner__SEP__0__SEP__0__SEP__2202__SEP__80__SEP__24\n'
 printf '%%2__SEP__$1__SEP__@2__SEP__/work__SEP__codex__SEP__%s__SEP__1__SEP__work__SEP__tab__SEP__0__SEP__0__SEP__1__SEP__80__SEP__24\n' "$(cat title 2>/dev/null)" ;;
esac
"#.replace("__SEP__", crate::terminal::parse::pane_separator())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = disk.path().join("registry.json");
        let initial = if previously_managed {
            json!({"panes":{"%2":{"name":"old-agent"}},"tabs":{},"workspaces":{}})
        } else {
            json!({"panes":{},"tabs":{},"workspaces":{}})
        };
        std::fs::write(&path, initial.to_string()).unwrap();
        let backend = TmuxBackend::with_registry(&script, &path);
        let factory: LoopTerminalFactory = Arc::new(move || Box::new(backend.clone()));
        let (_dir, services, _) = make_services_with_factory(None, true, Some(factory));
        let out = dispatcher(&services)
            .dispatch(
                "rename",
                &json!({"kind":"pane","target":{"pane_id":"%2"},"label":"new-label"}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(
            out["pane"]["label"],
            if previously_managed {
                "old-agent"
            } else {
                "new-label"
            }
        );
        assert_eq!(out["pane"]["terminal_title"], "new-label");
        assert_eq!(out["pane"]["managed"], previously_managed);
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["panes"]["%2"]["label"], "new-label");
        assert!(saved["panes"]["%2"].get("name").is_none());
    }
}

#[test]
fn start_agent_without_scope_or_live_parent_membership_refuses_before_creation() {
    let (_dir, services, events) = make_services(None, true);
    let error = dispatcher(&services)
        .dispatch(
            "start_agent",
            &json!({"name":"probe","command":"codex"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("lane project") || error.to_string().contains("project"), "{error}");
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn owner_registers_live_project_and_child_inherits_it() {
    let (dir, services, _) = make_services(None, true);
    let _agent = fake_agent(&dir);
    let d = dispatcher(&services);
    let caller = request("w2:p2");
    let registered = d.dispatch("lanes", &json!({"action":"project","pane":"w2:p2","project":"poolrad"}), &caller).unwrap();
    assert_eq!(registered["scope"], "project");
    assert_eq!(registered["project"], "poolrad");
    let launched = d.dispatch("start_agent", &json!({"name":"probe","command":"codex","workspace":"self"}), &caller).unwrap();
    assert_eq!(launched["membership"]["project"], "poolrad");
    let queried = d.dispatch("lanes", &json!({"action":"project","pane":"w1:p-probe"}), &caller).unwrap();
    assert_eq!(queried["scope"], "project");
    assert_eq!(queried["project"], "poolrad");
}
