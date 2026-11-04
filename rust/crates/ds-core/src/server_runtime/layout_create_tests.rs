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
        // This fixture uses coherent session IDs across the canned pane/tab/workspace rows.
        for pane in &mut terminal.panes {
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
        assert_eq!(out["tab"]["workspace_id"], "w2");
        assert_eq!(out["ran"], Value::Null);
        assert_eq!(services.flock.last_message(), "Created tab");
    }
    assert!(!events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event[0] == "send_raw"));
    let own = dispatch
        .dispatch(
            "create_tab",
            &json!({"workspace":"self"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(own["tab"]["workspace_id"], "w2");

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

#[test]
fn layout_create_role_success_persists_python_raw_string_fields() {
    let (dir, services, events) = make_services(None, true);
    let out=dispatcher(&services).dispatch("create_workspace",&json!({"label":"created","mailbox":["lane",false],"role":"worker","provider":false,"model":[]}),&request("w2:p2")).unwrap();
    assert_eq!(out["role"]["provider"], "False");
    assert_eq!(out["role"]["model"], "[]");
    assert_eq!(out["role"]["label"], "['lane', False]");
    let pane = out["pane_id"].as_str().unwrap();
    assert_eq!(
        services.flock.last_message(),
        format!("Registered {pane} as worker")
    );
    let roles: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("roles.json")).unwrap()).unwrap();
    assert_eq!(roles["lanes"][pane]["role"], "worker");
    assert!(events
        .lock()
        .unwrap()
        .contains(&json!(["rename", "pane", pane, "['lane', False]"])));
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event[0] == "workspace"));
}

#[test]
fn layout_create_registry_parent_file_is_thrown_after_creation_before_writeback() {
    use crate::terminal::{
        tmux::TmuxBackend,
        tmux_registry::{RegistryError, RegistryOperation},
        Terminal,
    };
    use std::os::unix::fs::PermissionsExt;
    let disk = tempfile::tempdir().unwrap();
    let script = disk.path().join("tmux");
    std::fs::write(&script,r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n' "$@" >> calls
case "$1" in
new-session) printf '%s' "$4" > created ;;
list-sessions) printf '$1\037%s\0370\n' "$(cat created 2>/dev/null)" ;;
list-windows) printf '@1\037$1\037tab\0370\0371\n' ;;
list-panes)
  printf 'w2:p2\037$old\037@old\037/work\037bash\037owner\0371\037owner\037owner\0370\0370\0372202\03780\03724\n'
  printf '%%1\037$1\037@1\037/work\037bash\037new\0371\037new\037tab\0370\0370\0371\03780\03724\n'
  ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let parent = disk.path().join("parent");
    std::fs::write(&parent, b"file").unwrap();
    let registry = parent.join("registry.json");
    let mut terminal = TmuxBackend::with_registry(&script, &registry);
    let error = terminal
        .create_workspace_raw(&json!("created"), &Value::Null, false)
        .unwrap_err();
    assert_eq!(error.category(), "FileExistsError");
    match &error {
        RegistryError::Io {
            operation,
            path,
            destination,
            error,
        } => {
            assert_eq!(*operation, RegistryOperation::CreateDirectory);
            assert_eq!(path, &parent);
            assert!(destination.is_none());
            assert_eq!(error.raw_os_error(), Some(nix::libc::EEXIST));
        }
        other => panic!("expected real filesystem error: {other:?}"),
    }
    let expected = error.to_string();
    assert_eq!(
        expected,
        format!(
            "[Errno {}] File exists: {}",
            nix::libc::EEXIST,
            api::python_value_repr(&json!(parent.to_string_lossy()))
        )
    );
    let factory: LoopTerminalFactory =
        Arc::new(move || Box::new(TmuxBackend::with_registry(&script, &registry)));
    let (dir, services, _) = make_services_with_factory(None, true, Some(factory));
    services.flock.set_last_message("before");
    let error = dispatcher(&services)
        .dispatch(
            "create_workspace",
            &json!({
                "label": "created",
                "command": "echo must-not-type",
                "mailbox": "must not write",
                "role": "worker"
            }),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), expected);
    assert_eq!(
        std::fs::read_to_string(disk.path().join("created")).unwrap(),
        "created"
    );
    assert!(!std::fs::read_to_string(disk.path().join("calls"))
        .unwrap()
        .contains("send-keys"));
    assert_eq!(services.flock.last_message(), "before");
    assert!(services.flock.sent_text_by_pane.lock().unwrap().is_empty());
    assert!(services.flock.lane_registry(false).unwrap().is_empty());
    let roles: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("roles.json")).unwrap()).unwrap();
    assert!(roles["lanes"].get("%1").is_none());
}
