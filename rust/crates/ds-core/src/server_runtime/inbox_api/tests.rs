use std::sync::Arc;

use serde_json::json;
use tokio::runtime::Builder;

use crate::api;
use crate::config::Env;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::metrics::MetricsStore;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::tasks::Tasks;
use crate::terminal::{fakes::FakeTerminal, TerminalError};

fn handlers() -> api::Dispatcher {
    let mut handlers = api::Dispatcher::new();
    handlers.register_inbox_messages();
    handlers
}

fn factory(fail_grouped_panes: Option<TerminalError>) -> LoopTerminalFactory {
    Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.fail_grouped_panes = fail_grouped_panes.clone();
        terminal.panes = ["w1:p1", "w2:p2"]
            .into_iter()
            .map(|pane_id| crate::terminal::Pane {
                pane_id: pane_id.into(),
                ..Default::default()
            })
            .collect();
        Box::new(terminal)
    })
}

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
        persist_enabled: false,
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

fn exercise(command: &str) {
    let (dir, services) = make_services(command, factory(None), None);
    let store = services.flock().stores().inbox.unwrap();
    let blob = dir.path().join("evidence.txt");
    std::fs::write(&blob, b"attachment evidence").unwrap();
    let first = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("first body"),
            subject: Some("private subject"),
            attachments: &[blob],
            ..Default::default()
        })
        .unwrap();
    let second = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("second body"),
            ..Default::default()
        })
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    let call =
        |name: &str, args: serde_json::Value| dispatcher.dispatch(name, &args, &ctx).unwrap();
    match command {
        "inbox_send" => {
            let r = call(
                command,
                json!({"to":"w1:p1","body":"sent body","notify":false}),
            );
            assert_eq!(r["body"], "sent body");
            assert_eq!(r["notified"], false);
            assert_eq!(r["attachments"], json!([]));
            assert!(r["read_at"].is_null());
            assert!(dispatcher
                .dispatch(
                    command,
                    &json!({"to":"w1:p1","body":"bad","system_sender":"watchdog"}),
                    &ctx
                )
                .is_err());
        }
        "inbox_count" => {
            let r = call(command, json!({"mailbox":"w1:p1"}));
            assert_eq!(r["unread"], 2);
            assert_eq!(r["attachments_unread"], 1);
        }
        "inbox_list" => {
            assert_eq!(
                call(command, json!({"mailbox":"w1:p1","limit":0}))["messages"],
                json!([])
            );
            let r = call(command, json!({"mailbox":"w1:p1","limit":-1}));
            assert_eq!(r["messages"].as_array().unwrap().len(), 2);
            assert_eq!(r["messages"][0]["message_id"], first);
            assert_eq!(
                r["messages"][0]["attachments"][0]["filename"],
                "evidence.txt"
            );
            assert!(r["messages"][0].get("body").is_none());
        }
        "inbox_read" => {
            let peek = call(
                command,
                json!({"mailbox":"w1:p1","peek":true,"include_tasks":false}),
            );
            assert_eq!(peek["messages"][0]["body"], "first body");
            assert!(peek.get("marked_read").is_none());
            let read = call(
                command,
                json!({"mailbox":"w1:p1","limit":-1,"include_tasks":false}),
            );
            assert_eq!(read["marked_read"], 2);
            assert!(read.get("tasks").is_none());
            assert_eq!(store.recover_delivery("w1:p1").unwrap().len(), 2);
        }
        "inbox_status" => {
            let r = call(command, json!({"message_id":first}));
            assert_eq!(r["state"], "queued");
            assert!(r.get("body").is_none());
            assert!(r.get("subject").is_none());
            assert!(r["notify"].is_object());
            assert_eq!(r["attachments"], 1);
        }
        "inbox_ack" => {
            call(
                "inbox_read",
                json!({"mailbox":"w1:p1","include_tasks":false}),
            );
            let r = call(command, json!({"message_ids":first,"mailbox":"w1:p1"}));
            assert_eq!(r["changed"], json!([first]));
            assert_eq!(r["still_pending"], json!([second]));
        }
        "inbox_cancel" => {
            let r = call(command, json!({"message_ids":first}));
            assert_eq!(r["state"], "cancelled");
            assert_eq!(r["changed"], json!([first]));
        }
        "inbox_delete" => {
            let r = call(command, json!({"message_ids":[first.clone()]}));
            assert_eq!(r, json!({"deleted":[first]}));
            assert!(store.get_message(&first).is_err());
        }
        "inbox_recover" => {
            call(
                "inbox_read",
                json!({"mailbox":"w1:p1","include_tasks":false}),
            );
            let r = call(command, json!({"mailbox":"w1:p1"}));
            assert_eq!(r["messages"].as_array().unwrap().len(), 2);
            assert_eq!(r["messages"][0]["body"], "first body");
        }
        "inbox_archive" => {
            let r = call(command, json!({"mailbox":"w1:p1","state":"queued"}));
            assert_eq!(r["archived"], 2);
            assert_eq!(r["states"], json!(["queued"]));
        }
        "inbox_stats" => {
            let r = call(command, json!({}));
            assert_eq!(r["mailboxes"][0]["unread"], 2);
            assert_eq!(r["mailboxes"][0]["mailbox"], "w1:p1");
        }
        "inbox_fetch" => {
            let dest = dir.path().join("fetched");
            let r = call(
                command,
                json!({"attachment_id":format!("{first}-1"),"dest":dest}),
            );
            assert_eq!(r["filename"], "evidence.txt");
            assert_eq!(
                std::fs::read(dest.join("evidence.txt")).unwrap(),
                b"attachment evidence"
            );
        }
        _ => unreachable!(),
    }
    dispatcher.close();
}

#[test]
fn h_inbox_send_matches_python() {
    exercise("inbox_send");
}

#[test]
fn h_inbox_count_matches_python() {
    exercise("inbox_count");
}

#[test]
fn h_inbox_list_matches_python() {
    exercise("inbox_list");
}

#[test]
fn h_inbox_read_matches_python() {
    exercise("inbox_read");
}

#[test]
fn h_inbox_status_matches_python() {
    exercise("inbox_status");
}

#[test]
fn h_inbox_ack_matches_python() {
    exercise("inbox_ack");
}

#[test]
fn h_inbox_cancel_matches_python() {
    exercise("inbox_cancel");
}

#[test]
fn h_inbox_delete_matches_python() {
    exercise("inbox_delete");
}

#[test]
fn h_inbox_recover_matches_python() {
    exercise("inbox_recover");
}

#[test]
fn h_inbox_archive_matches_python() {
    exercise("inbox_archive");
}

#[test]
fn h_inbox_stats_matches_python() {
    exercise("inbox_stats");
}

#[test]
fn h_inbox_fetch_matches_python() {
    exercise("inbox_fetch");
}

#[test]
fn inbox_registration_inventory_has_exactly_twelve_unique_handlers() {
    let h = handlers();
    let names = h.registration_attempts();
    assert_eq!(names.len(), 12);
    let unique = names
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique,
        std::collections::BTreeSet::from([
            "inbox_send",
            "inbox_count",
            "inbox_list",
            "inbox_read",
            "inbox_status",
            "inbox_ack",
            "inbox_cancel",
            "inbox_delete",
            "inbox_recover",
            "inbox_archive",
            "inbox_stats",
            "inbox_fetch"
        ])
    );
}

#[test]
fn inbox_dispatcher_bad_limits_and_cross_pane_refusal_preserve_unread_rows() {
    let (_dir, services) = make_services("raw-args", factory(None), None);
    let store = services.flock().stores().inbox.unwrap();
    let id = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w2:p2",
            body: Some("private"),
            ..Default::default()
        })
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let worker = request("w1:p1");
    for n in [
        json!([]),
        json!({}),
        json!("not an int"),
        json!("999999999999999999999999999999"),
    ] {
        assert!(dispatcher
            .dispatch("inbox_list", &json!({"mailbox":"w2:p2","limit":n}), &worker)
            .is_err());
    }
    assert!(dispatcher
        .dispatch("inbox_read", &json!({"mailbox":"w2:p2"}), &worker)
        .is_err());
    assert!(dispatcher
        .dispatch(
            "inbox_ack",
            &json!({"message_ids":id,"mailbox":"w2:p2"}),
            &worker
        )
        .is_err());
    assert_eq!(store.get_message(&id).unwrap().summary.state, "queued");
    let owner = request("w2:p2");
    assert_eq!(
        dispatcher
            .dispatch(
                "inbox_read",
                &json!({"mailbox":"w2:p2","peek":true,"include_tasks":false}),
                &owner
            )
            .unwrap()["messages"][0]["body"],
        "private"
    );
    dispatcher.close();
}

#[test]
fn inbox_count_cache_fresh_and_missing_key_fallback_use_actual_store() {
    let (_dir, services) = make_services("cache", factory(None), None);
    let store = services.flock().stores().inbox.unwrap();
    store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("one"),
            ..Default::default()
        })
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    assert_eq!(
        dispatcher
            .dispatch("inbox_count", &json!({"fresh":true}), &ctx)
            .unwrap()["unread"],
        1
    );
    store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("two"),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        dispatcher
            .dispatch("inbox_count", &json!({}), &ctx)
            .unwrap()["unread"],
        1
    );
    assert_eq!(
        dispatcher
            .dispatch("inbox_count", &json!({"fresh":true}), &ctx)
            .unwrap()["unread"],
        2
    );
    assert!(dispatcher
        .dispatch("inbox_count", &json!({}), &RequestCtx::default())
        .is_err());
    dispatcher.close();
}

#[test]
fn inbox_provider_rejects_missing_outbox_and_mismatched_flock() {
    use crate::api::CommandEnv;
    use crate::server_runtime::RequestRuntimeAdapter;
    use std::sync::Mutex;
    let (_dir, services) = make_services("provider", factory(None), None);
    let terminal: Mutex<Box<dyn crate::terminal::Terminal>> =
        Mutex::new(Box::new(FakeTerminal::new()));
    let ctx = RequestCtx::default();
    let adapter =
        RequestRuntimeAdapter::new(services.flock(), &ctx, services.runtime.handle(), &terminal);
    let error = adapter
        .with_inbox_env(&mut |_| panic!("missing provider must not invoke operation"))
        .unwrap_err();
    assert!(error.to_string().contains("outbox service is not attached"));
    let (_other_dir, other) = make_services("other-provider", factory(None), None);
    assert!(
        RequestRuntimeAdapter::new(other.flock(), &ctx, services.runtime.handle(), &terminal)
            .with_outbox(&services.outbox)
            .is_err()
    );
}

#[test]
fn inbox_status_cache_ttl_validates_captured_startup_config() {
    use crate::flock::{status_runtime::StatusState, Flock};
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("executable");
    std::fs::write(&executable, b"fixture").unwrap();
    for (value, valid) in [("17.5", true), ("0", true), ("-1", true), ("bad", false)] {
        let flock = Flock::new(
            dir.path().join(format!("launch-{value}")),
            dir.path().join(format!("lane-{value}")),
            false,
        );
        let state = StatusState::at_start(
            "test",
            dir.path().join("state"),
            &executable,
            Env::from_pairs(&[("DUNKINGSHEEP_VIEW_CACHE_S", value)]),
        );
        assert_eq!(flock.attach_status_state(state).is_ok(), valid);
        if valid {
            assert_eq!(
                flock.status_state().unwrap().view_cache_s,
                value.parse::<f64>().unwrap()
            );
        }
    }
}

#[test]
fn inbox_send_raw_values_preserve_body_subject_and_notification_error_order() {
    let (_dir, services) = make_services("raw-send", factory(None), None);
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    let result = dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":0,"subject":7,"notify":false}),
            &ctx,
        )
        .unwrap();
    assert_eq!(result["body"], "0");
    assert_eq!(result["subject"], "7");
    assert!(dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":"x","system_sender":true}),
            &ctx
        )
        .unwrap_err()
        .to_string()
        .contains("unknown system sender True"));
    let store = services.flock().stores().inbox.unwrap();
    let before = crate::inbox::count(store.read_pool(), "w1:p1", &[], crate::clock::now_s())
        .unwrap()
        .total;
    let error = dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":"committed before notice","subject":9,"notify":true}),
            &ctx,
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("'int' object has no attribute 'strip'"));
    assert_eq!(
        crate::inbox::count(store.read_pool(), "w1:p1", &[], crate::clock::now_s())
            .unwrap()
            .total,
        before + 1
    );
    dispatcher.close();
}
