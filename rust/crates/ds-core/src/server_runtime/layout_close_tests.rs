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
    make_services_persistent(fail_layout, available, factory, false)
}
fn make_services_persistent(
    fail_layout: Option<&str>,
    available: bool,
    factory: Option<LoopTerminalFactory>,
    persist: bool,
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
        terminal.start_agent_pid = Some(None);
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
        persist_enabled: persist,
        stores,
        roles,
        legacy_superusers: None,
        release_version: "test".into(),
        state_file: dir.path().join("state.json"),
        executable,
        env,
        observability: Some(RuntimeObservability {
            state_dir: dir.path().join("observability"),
            memory_db: persist.then(|| dir.path().join("memory.db")),
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
    handlers.register_layout_close();
    super::super::dispatcher::RuntimeDispatcher::new(Arc::clone(services), handlers)
}

#[test]
fn h_close_matches_python() {
    let (_dir, services, events) = make_services(None, true);
    let pane = json!({"pane_id":"w1:p1"});
    let row = services
        .add_dunk(
            pane.as_object(),
            &crate::flock::dunks::AddArgs {
                start: true,
                interval: Some("10".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let id = row["id"].as_str().unwrap();
    let out = dispatcher(&services)
        .dispatch(
            "close",
            &json!({"kind":" PANE ","target":{"pane_id":"w1:p1"}}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(
        out,
        json!({"kind":"pane","id":"w1:p1","closed_panes":["w1:p1"],"stopped_dunks":[id]})
    );
    let row = services.flock.dunks.get(id).unwrap();
    assert_eq!(row["running"], false);
    assert_eq!(row["status"], "Target closed");
    assert!(row["next_send_at"].is_null());
    assert_eq!(services.flock.last_message(), "Closed pane π - work");
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e[0] == "close")
            .count(),
        1
    );
}
#[test]
fn layout_close_own_container_guards_and_backend_failure_precede_halt() {
    for kind in ["pane", "tab", "workspace"] {
        let (_dir, services, events) = make_services(None, true);
        let err = dispatcher(&services)
            .dispatch(
                "close",
                &json!({"kind":kind,"target":"self","force":"false"}),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert!(err
            .to_string()
            .starts_with(&format!("refusing to close the {kind} this caller runs in")));
        assert!(events.lock().unwrap().is_empty());
        let out = dispatcher(&services)
            .dispatch(
                "close",
                &json!({"kind":kind,"target":"self","force":true}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(out["kind"], kind);
    }
    let (_dir, services, _) = make_services(Some("fixture refusal"), true);
    let pane = json!({"pane_id":"w1:p1"});
    let row = services
        .add_dunk(
            pane.as_object(),
            &crate::flock::dunks::AddArgs {
                start: true,
                interval: Some("10".into()),
                ..Default::default()
            },
        )
        .unwrap();
    services.flock.set_last_message("before");
    let err = dispatcher(&services)
        .dispatch(
            "close",
            &json!({"kind":"pane","target":"w1:p1"}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the terminal backend could not close pane w1:p1: fixture refusal"
    );
    assert_eq!(
        services
            .flock
            .dunks
            .get(row["id"].as_str().unwrap())
            .unwrap()["running"],
        true
    );
    assert_eq!(services.flock.last_message(), "before");
}
fn herdr_services(code: i32, out: &str, err: &str) -> LayoutFixture {
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let out = out.to_owned();
    let err = err.to_owned();
    let factory: LoopTerminalFactory = Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.backend_name = "herdr".into();
        terminal.herdr_output = (code, out.clone(), err.clone());
        terminal.raw_events = observed.clone();
        Box::new(terminal)
    });
    let (dir, services, _) = make_services_with_factory(None, true, Some(factory));
    (dir, services, events)
}
#[test]
fn h_herdr_matches_python() {
    let (_dir, services, events) = herdr_services(0, r#"{"result":{"x":1}}"#, "ignored");
    let out = dispatcher(&services)
        .dispatch(
            "herdr",
            &json!({"command":"herdr tab focus self"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(
        out,
        json!({"ok":true,"argv":["tab","focus","w2"],"result":{"x":1},"text":r#"{"result":{"x":1}}"#,"error":null,"exit_code":0})
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![json!(["herdr", ["tab", "focus", "w2"], 30])]
    );
}
#[test]
fn h_herdr_help_matches_python() {
    let (_dir, services, events) = herdr_services(1, "", "herdr pane commands:\n");
    let out = dispatcher(&services)
        .dispatch(
            "herdr_help",
            &json!({"topic":" pane\u{001c}list  "}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(
        out,
        json!({"topic":"pane list","text":"herdr pane commands:\n"})
    );
    assert_eq!(
        *events.lock().unwrap(),
        vec![json!(["herdr", ["pane"], 10])]
    );
}
#[test]
fn layout_passthrough_guards_precede_backend_and_self_lookup() {
    let (_dir, services, events) = herdr_services(0, "{}", "");
    let d = dispatcher(&services);
    for command in [
        "server stop self",
        "update",
        "channel set",
        "session",
        "completion",
        "agent attach",
        "integration",
        "pane send-text self",
        "pane send-keys self Enter",
        "pane run self",
        "agent send self",
        "tab focus --remote self",
        "workspace --session self",
    ] {
        assert!(
            d.dispatch("herdr", &json!({"command":command}), &request("w2:p2"))
                .is_err(),
            "{command}"
        );
    }
    assert!(events.lock().unwrap().is_empty());
    let (_dir, services, events) = make_services(None, true);
    for command in [json!("'"), json!([]), json!(["pane", "send-keys"])] {
        let err = dispatcher(&services)
            .dispatch("herdr", &json!({"command":command}), &request("w2:p2"))
            .unwrap_err();
        assert_eq!(err.to_string(),"the herdr passthrough is not available with the tmux terminal backend; use the dedicated workspace/tab/pane commands instead");
    }
    assert!(dispatcher(&services)
        .dispatch("herdr_help", &json!({}), &request("w2:p2"))
        .unwrap_err()
        .to_string()
        .contains("herdr help is not available with the tmux"));
    assert!(events.lock().unwrap().is_empty());
}
#[test]
fn layout_passthrough_json_usage_and_raw_argument_order() {
    for (code, out, err, error, result) in [
        (1, "Usage: herdr pane", "", Value::Null, Value::Null),
        (1, "herdr pane commands:", "", Value::Null, Value::Null),
        (
            0,
            "not json",
            r#"{"error":{"message":"bad","code":"E"}}"#,
            json!("bad"),
            Value::Null,
        ),
        (1, "", "", json!("herdr exited 1"), Value::Null),
        (
            0,
            r#"{"error":{"code":false},"result":7}"#,
            "",
            json!(false),
            Value::Null,
        ),
        (0, r#"{"result":7}"#, "", Value::Null, json!(7)),
    ] {
        let (_dir, services, events) = herdr_services(code, out, err);
        let value = dispatcher(&services)
            .dispatch(
                "herdr",
                &json!({"command":["api",null,false,0,{"x":true}]}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(value["error"], error);
        assert_eq!(value["ok"], error.is_null());
        assert_eq!(value["result"], result);
        assert_eq!(
            events.lock().unwrap()[0],
            json!(["herdr", ["api", "None", "False", "0", "{'x': True}"], 30])
        );
    }
    let (_dir, services, events) = herdr_services(0, "", "");
    for (command, expected) in [
        (json!(false), "command is required"),
        (json!(0), "command is required"),
        (json!([]), "command is required"),
        (json!("'"), "No closing quotation"),
        (json!("x\\"), "No escaped character"),
    ] {
        assert!(dispatcher(&services)
            .dispatch("herdr", &json!({"command":command}), &request("w2:p2"))
            .unwrap_err()
            .to_string()
            .contains(expected));
    }
    assert!(dispatcher(&services)
        .dispatch("herdr_help", &json!({}), &request("w2:p2"))
        .unwrap_err()
        .to_string()
        .contains("herdr printed no help for '--help' (exit 0)"));
    assert_eq!(events.lock().unwrap().len(), 1);
}
#[test]
fn layout_close_persists_stopped_rows_and_save_error_precedes_last_message() {
    for fail in [false, true] {
        let (dir, services, events) = make_services_persistent(None, true, None, true);
        let pane = json!({"pane_id":"w1:p1"});
        let row = services
            .add_dunk(
                pane.as_object(),
                &crate::flock::dunks::AddArgs {
                    start: true,
                    interval: Some("10".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let id = row["id"].as_str().unwrap();
        services.flock.set_last_message("before close");
        if fail {
            std::fs::remove_file(dir.path().join("state.json")).unwrap();
            std::fs::create_dir(dir.path().join("state.json")).unwrap();
        }
        let out = dispatcher(&services).dispatch(
            "close",
            &json!({"kind":"pane","target":"w1:p1"}),
            &request("w2:p2"),
        );
        assert_eq!(out.is_err(), fail);
        assert_eq!(
            services.flock.dunks.get(id).unwrap()["status"],
            "Target closed"
        );
        assert!(services.flock.dunks.get(id).unwrap()["next_send_at"].is_null());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e[0] == "close")
                .count(),
            1
        );
        if fail {
            assert_eq!(services.flock.last_message(), "before close");
        } else {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap())
                    .unwrap();
            assert_eq!(saved["dunkers"][0]["running"], false);
            assert_eq!(saved["dunkers"][0]["status"], "Target closed");
        }
        services.shutdown();
    }
}
#[test]
fn layout_close_dispatch_gates_and_raw_kind_errors_do_not_mutate() {
    let (_dir, services, events) = make_services(None, true);
    let d = dispatcher(&services);
    for value in [
        Value::Null,
        json!(""),
        json!(false),
        json!(0),
        json!([]),
        json!({}),
        json!(["pane"]),
    ] {
        assert!(d
            .dispatch(
                "close",
                &json!({"kind":value,"target":"absent"}),
                &request("w2:p2")
            )
            .is_err());
    }
    let err = d
        .dispatch("close", &json!({}), &request("w1:p1"))
        .unwrap_err();
    assert!(!err.to_string().contains("missing required"), "{err}");
    assert!(events.lock().unwrap().is_empty());
}
