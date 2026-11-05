use super::*;

use crate::api;
use crate::config::Env;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::inbox_store::Outgoing;
use crate::proc::Caller;
use crate::roles::Roles;
use crate::server::{Dispatcher, RequestCtx};
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::tasks::Tasks;
use crate::terminal::fakes::FakeTerminal;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::runtime::Builder;

fn services() -> (tempfile::TempDir, Arc<RuntimeServices>) {
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
        .register(
            "w2:p2",
            "owner",
            None,
            None,
            Some("codex"),
            None,
            None,
            None,
        )
        .unwrap();
    let executable = dir.path().join("daemon");
    std::fs::write(&executable, b"runtime-control-test").unwrap();
    let stores = Stores {
        tasks: Some(tasks),
        inbox: Some(inbox),
        ..Stores::default()
    };
    let terminal_factory: LoopTerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.statuses.insert("w1:p1".into(), "idle".into());
        terminal
            .screens
            .insert("w1:p1".into(), "› Ask Codex to do anything\n".into());
        Box::new(terminal)
    });
    let services = RuntimeServices::new(RuntimeServiceInputs {
        launches_file: dir.path().join("launches.json"),
        lane_registry_file: dir.path().join("lanes.json"),
        persist_enabled: true,
        stores,
        roles,
        legacy_superusers: None,
        release_version: "test".into(),
        state_file: dir.path().join("state.json"),
        executable,
        env,
        observability: Some(RuntimeObservability {
            state_dir: dir.path().join("observability"),
            memory_db: Some(dir.path().join("memory.db")),
        }),
        runtime,
        terminal_factory,
    })
    .unwrap();
    (dir, services)
}

fn request(pane: &str) -> RequestCtx {
    RequestCtx {
        caller: Some(Caller::new(
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

fn dispatcher(services: &Arc<RuntimeServices>) -> Arc<dyn Dispatcher> {
    let mut handlers = api::Dispatcher::new();
    handlers.register_runtime_control();
    services.dispatcher(handlers)
}

#[test]
fn production_control_queue_is_lazy_role_checked_and_uses_the_shared_sqlite_queue() {
    let (dir, services) = services();
    let dispatcher = dispatcher(&services);
    let queue_path = dir.path().join("dispatch.db");

    let denied = dispatcher.dispatch(
        "dispatch",
        &json!({"action":"add","brief":"must not open"}),
        &request("w1:p1"),
    );
    assert!(denied.is_err());
    assert!(
        !queue_path.exists(),
        "role refusal must happen before Queue::open"
    );

    let added = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"add","brief":"  real queue item  ","task_id":"t8","lane":null}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(added["brief"], "real queue item");
    assert_eq!(added["task_id"], "t8");
    assert!(queue_path.exists());

    let listed = dispatcher
        .dispatch("dispatch", &json!({"action":"list"}), &request("w1:p1"))
        .unwrap();
    assert_eq!(listed["pending"], 1);
    assert_eq!(listed["items"][0]["id"], added["id"]);
    let removed = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"remove","id":added["id"]}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(removed["removed"], added["id"]);
    services.shutdown_checked().unwrap();
}

#[test]
fn production_allocation_status_reads_shared_cache_without_running_a_tick() {
    let (_dir, services) = services();
    let cached = json!({"allocation":"prior result"});
    services.allocator.seed_cache(cached.clone());
    let response = dispatcher(&services)
        .dispatch("allocation", &json!({"action":"status"}), &request("w1:p1"))
        .unwrap();
    assert_eq!(response["last_tick"], cached);
    assert_eq!(services.allocator.last_result(), cached);
    services.shutdown_checked().unwrap();
}

#[test]
fn production_dispatch_tick_sends_real_standby_mail_with_request_identity() {
    let (_dir, services) = services();
    let dispatcher = dispatcher(&services);
    dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"add","brief":"real inbox delivery","task_id":"t99"}),
            &request("w2:p2"),
        )
        .unwrap();

    let dry = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"tick","lanes":"w1:p1","dry_run":true}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(dry["dispatched_count"], 0);
    let inbox = services.flock().stores().inbox.unwrap();
    assert!(inbox.read("w1:p1", 10, true, false).unwrap().is_empty());

    let result = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"tick","lanes":"w1:p1"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(result["dispatched_count"], 1);
    assert_eq!(result["dispatched"][0]["pane_id"], "w1:p1");
    assert_eq!(result["dispatched"][0]["sent"], true);
    let messages = inbox.read("w1:p1", 10, true, false).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].summary.subject.as_deref(), Some("standby"));
    assert_eq!(messages[0].summary.sender.as_deref(), Some("w2:p2"));
    assert!(messages[0].body.contains("real inbox delivery"));
    services.shutdown_checked().unwrap();
}

#[test]
fn production_dispatch_tick_honors_fresh_mail_then_records_expired_mail() {
    let (dir, services) = services();
    let dispatcher = dispatcher(&services);
    dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"add","brief":"queued behind fresh mail"}),
            &request("w2:p2"),
        )
        .unwrap();
    let inbox = services.flock().stores().inbox.unwrap();
    inbox
        .send(&Outgoing {
            mailbox: "w1:p1",
            body: Some("fresh assignment"),
            sender: Some("w2:p2"),
            ..Outgoing::default()
        })
        .unwrap();

    let fresh = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"tick","lanes":"w1:p1"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(fresh["dispatched_count"], 0);
    assert_eq!(fresh["has_mail"], 1);

    let connection = rusqlite::Connection::open(dir.path().join("inbox.db")).unwrap();
    connection
        .execute(
            "UPDATE messages SET created_at = ? WHERE mailbox = ?",
            rusqlite::params![crate::clock::now_s() - 601.0, "w1:p1"],
        )
        .unwrap();
    let expired = dispatcher
        .dispatch(
            "dispatch",
            &json!({"action":"tick","lanes":"w1:p1"}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(expired["dispatched_count"], 1);
    assert_eq!(expired["unread_mail_expired"], json!(["w1:p1"]));
    services.shutdown_checked().unwrap();
}

#[test]
fn production_dispatch_tick_propagates_unread_store_failure() {
    let (dir, services) = services();
    let dispatcher = dispatcher(&services);
    let connection = rusqlite::Connection::open(dir.path().join("inbox.db")).unwrap();
    connection.execute("DROP TABLE messages", []).unwrap();
    let result = dispatcher.dispatch(
        "dispatch",
        &json!({"action":"tick","lanes":"w1:p1"}),
        &request("w2:p2"),
    );
    assert!(
        result.is_err(),
        "real unread lookup errors must abort the tick"
    );
    services.shutdown_checked().unwrap();
}

#[test]
fn production_shutdown_stops_and_persists_canonical_timer_state_before_reply() {
    let (dir, services) = services();
    let pane = json!({"pane_id":"w1:p1"});
    let row = services
        .add_dunk(
            pane.as_object(),
            &crate::flock::dunks::AddArgs {
                start: true,
                interval: Some("3600".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let response = dispatcher(&services)
        .dispatch("shutdown", &json!({"stop_all":true}), &request("w2:p2"))
        .unwrap();
    assert_eq!(response, json!({"shutting_down":true,"stopped_all":true}));
    let stopped = services
        .flock
        .dunks
        .get(row["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(stopped["running"], false);
    assert_eq!(stopped["next_send_at"], Value::Null);
    let snapshot: Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("state.json")).expect("shutdown persists whole Flock"),
    )
    .unwrap();
    let persisted = snapshot["dunkers"].as_array().unwrap();
    assert!(persisted
        .iter()
        .any(|row| { row["id"] == stopped["id"] && row["running"] == false }));
    services.shutdown_checked().unwrap();
}
