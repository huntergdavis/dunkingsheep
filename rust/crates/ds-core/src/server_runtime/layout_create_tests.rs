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
fn make_services(fail_layout: Option<&str>, available: bool) -> LayoutFixture {
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
    let env = Env::default();
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
        terminal.fail_layout = fail_layout.clone();
        terminal.available = available;
        Box::new(terminal)
    });
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
    handlers.register_layout_create();
    super::super::dispatcher::RuntimeDispatcher::new(Arc::clone(services), handlers)
}
#[test]
fn h_create_workspace_matches_python() {
    let (_dir, services, events) = make_services(None, true);
    let dispatch = dispatcher(&services);
    let result = dispatch
        .dispatch(
            "create_workspace",
            &json!({"label":["raw",false],"cwd":42,"command":"echo safe","mailbox":" lane "}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(result["ran"], json!("echo safe"));
    let pane = result["pane_id"].as_str().unwrap();
    assert_eq!(
        services.flock.last_message(),
        "Created workspace ['raw', False]"
    );
    assert_eq!(
        services.flock.sent_text_by_pane.lock().unwrap()[pane],
        vec![json!("echo safe")]
    );
    assert_eq!(
        crate::flock::Flock::row_identity(services.flock.lane_registry(false).unwrap().get(pane)),
        (Some("lane".into()), true)
    );
    let events = events.lock().unwrap();
    assert!(events.contains(&json!(["workspace", ["raw", false], 42, false])));
    assert!(events.contains(&json!(["typed", pane, "echo safe"])));
    assert!(events
        .iter()
        .any(|event| event[0] == "keys" && event[1] == pane && event[2] == json!(["Enter"])));
}
#[test]
fn h_create_tab_matches_python() {
    let (_dir, services, events) = make_services(None, true);
    let dispatch = dispatcher(&services);
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!([]),
        json!({}),
        json!(""),
    ] {
        let out = dispatch
            .dispatch(
                "create_tab",
                &json!({"workspace":value,"label":value,"cwd":value,"command":value}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(out["tab"]["workspace_id"], "s2");
        assert_eq!(out["ran"], Value::Null);
        assert_eq!(services.flock.last_message(), "Created tab");
    }
    assert!(!events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event[0] == "send_raw"));
    let error = dispatch
        .dispatch(
            "create_tab",
            &json!({"workspace":"missing"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no workspace matches 'missing' on the current backend"
    );
}
#[test]
fn h_split_pane_matches_python() {
    let (_dir, services, events) = make_services(None, true);
    let dispatch = dispatcher(&services);
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!([]),
        json!({}),
        json!("DOWN"),
    ] {
        let out = dispatch
            .dispatch(
                "split_pane",
                &json!({"target":{"pane_id":"w1:p1"},"direction":value,"command":"echo safe"}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(out["ran"], "echo safe");
        assert_eq!(services.flock.last_message(), "Created pane");
    }
    let before = events.lock().unwrap().len();
    for value in [json!(1), json!(["down"]), json!({"x":1})] {
        let error = dispatch
            .dispatch(
                "split_pane",
                &json!({"target":{},"direction":value}),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "direction must be 'right' or 'down'");
    }
    assert_eq!(before, events.lock().unwrap().len());
}
#[test]
fn layout_create_raw_failure_partial_state_and_role_order() {
    let (_dir, services, events) = make_services(None, true);
    let dispatch = dispatcher(&services);
    for (command, error) in [
        (json!(1), "object of type 'int' has no len()"),
        (json!({"x":1}), "slice(0, 1, None)"),
    ] {
        services.flock.set_last_message("before");
        let error_seen = dispatch
            .dispatch(
                "create_workspace",
                &json!({"command":command,"mailbox":"must not write","role":"worker"}),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert_eq!(error_seen.to_string(), error);
        assert_eq!(services.flock.last_message(), "before");
        assert!(services.flock.lane_registry(false).unwrap().is_empty());
        assert!(services.flock.sent_text_by_pane.lock().unwrap().is_empty());
    }
    assert!(!events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event[0] == "typed" || event[0] == "keys"));
    let error = dispatch
        .dispatch(
            "create_workspace",
            &json!({"label":"created","mailbox":"lane","role":"invalid"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("role must be one of"), "{error}");
    assert_eq!(services.flock.last_message(), "Created workspace created");
    assert!(!services.flock.lane_registry(false).unwrap().is_empty());
}
#[test]
fn layout_create_backend_failure_and_submit_refusal_are_distinct() {
    let (_dir, services, _) = make_services(Some("backend refused"), true);
    services.flock.set_last_message("before");
    let error = dispatcher(&services)
        .dispatch(
            "create_workspace",
            &json!({"label":"x","command":1}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the terminal backend could not create the workspace x: backend refused"
    );
    assert_eq!(services.flock.last_message(), "before");
    let (_dir, services, _) = make_services(None, false);
    let out = dispatcher(&services)
        .dispatch(
            "create_workspace",
            &json!({"command":"echo"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert!(out["error"]
        .as_str()
        .unwrap()
        .starts_with("created, but running the command failed:"));
    assert_eq!(services.flock.last_message(), "Created workspace");
}
#[test]
fn layout_create_dispatch_permission_precedes_required_and_direction_checks() {
    let (_dir, services, events) = make_services(None, true);
    let dispatch = dispatcher(&services);
    let error = dispatch
        .dispatch(
            "split_pane",
            &json!({"direction":123}),
            &RequestCtx::default(),
        )
        .unwrap_err();
    assert!(!error.to_string().contains("direction must"));
    assert!(events.lock().unwrap().is_empty());
    let error = dispatch
        .dispatch("split_pane", &json!({"direction":123}), &request("w2:p2"))
        .unwrap_err();
    assert!(error.to_string().contains("target"), "{error}");
}

#[test]
fn layout_create_raw_list_confirmation_error_keeps_typed_history_only() {
    let (_dir, services, events) = make_services(None, true);
    services.flock.set_last_message("before");
    let error = dispatcher(&services)
        .dispatch(
            "create_workspace",
            &json!({"command":["echo",false],"mailbox":"must not write"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "'list' object has no attribute 'encode'");
    assert_eq!(services.flock.last_message(), "before");
    assert!(services.flock.lane_registry(false).unwrap().is_empty());
    assert_eq!(
        services.flock.sent_text_by_pane.lock().unwrap()["w101:p1"],
        vec![json!(["echo", false])]
    );
    let events = events.lock().unwrap();
    assert!(events.contains(&json!(["typed", "w101:p1", "['echo', False]"])));
    assert!(events.contains(&json!(["keys", "w101:p1", ["Enter"]])));
}
