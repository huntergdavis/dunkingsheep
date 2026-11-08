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
    make_services_with_env(tag, terminal_factory, metrics, Env::default())
}

fn make_services_with_env(
    tag: &str,
    terminal_factory: LoopTerminalFactory,
    metrics: Option<Arc<MetricsStore>>,
    env: Env,
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
    // A desk pane, for the desk clamp path (t3539): its inbox read is clamped by the dispatcher.
    roles
        .register("w3:p3", "desk", None, None, None, None, None, None)
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
fn inbox_status_preserves_raw_sqlite_scalar_binding_and_python_repr() {
    let (_dir, services) = make_services("status-raw-id", factory(None), None);
    let store = services.flock().stores().inbox.unwrap();
    let id = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("status body"),
            ..Default::default()
        })
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    let found = dispatcher
        .dispatch("inbox_status", &json!({"message_id": id}), &ctx)
        .unwrap();
    assert_eq!(found["message_id"], id);

    for (raw, expected) in [
        (
            r#"{"message_id":[]}"#,
            "Error binding parameter 1: type 'list' is not supported",
        ),
        (
            r#"{"message_id":{}}"#,
            "Error binding parameter 1: type 'dict' is not supported",
        ),
        (r#"{"message_id":7}"#, "no message with id 7"),
        (r#"{"message_id":"7"}"#, "no message with id '7'"),
        (
            r#"{"message_id":0.10000000000000001}"#,
            "no message with id 0.1",
        ),
        (r#"{"message_id":1e400}"#, "no message with id inf"),
    ] {
        let args: serde_json::Value = serde_json::from_str(raw).unwrap();
        let error = dispatcher
            .dispatch("inbox_status", &args, &ctx)
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{raw}: {error}");
    }
    dispatcher.close();
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
fn inbox_registration_inventory_has_exactly_thirteen_unique_handlers() {
    let h = handlers();
    let names = h.registration_attempts();
    assert_eq!(names.len(), 13);
    let unique = names
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        unique,
        std::collections::BTreeSet::from([
            "inbox_send",
            "inbox_send_agent_only",
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
    let mut pending = crate::flock::sends::Message::new(
        "numeric-subject-notice",
        "w1:p1",
        "worker",
        "old notice",
        "test",
    );
    pending.is_notice = true;
    crate::flock::sends::outbox_lock(&services.flock().outbox)
        .insert("w1:p1".into(), vec![pending]);
    let result = dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":"numeric subject","subject":9,"notify":true}),
            &ctx,
        )
        .unwrap();
    assert_eq!(result["subject"], "9");
    assert_eq!(result["notified"], true);
    {
        let outbox = crate::flock::sends::outbox_lock(&services.flock().outbox);
        assert!(outbox["w1:p1"][0].text.contains(": 9."));
    }
    let falsey = dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":"falsey subject","subject":0,"notify":true}),
            &ctx,
        )
        .unwrap();
    assert_eq!(falsey["subject"], "0");
    assert_eq!(falsey["notified"], true);
    let outbox = crate::flock::sends::outbox_lock(&services.flock().outbox);
    let notice = &outbox["w1:p1"][0];
    assert!(
        !notice.text.contains(": 0."),
        "falsey numeric subject is omitted after the next fold"
    );
    assert_eq!(
        crate::inbox::count(store.read_pool(), "w1:p1", &[], crate::clock::now_s())
            .unwrap()
            .total,
        before + 2
    );
    dispatcher.close();
}

#[test]
fn inbox_send_notice_subject_rounds_raw_json_float_like_python() {
    use crate::flock::sends::{outbox_lock, Message};

    let (_dir, services) = make_services("float-notice-subject", factory(None), None);
    let mut pending = Message::new("old-float-notice", "w1:p1", "worker", "old", "test");
    pending.is_notice = true;
    outbox_lock(&services.flock().outbox).insert("w1:p1".into(), vec![pending]);

    let dispatcher = services.dispatcher(handlers());
    let args: serde_json::Value = serde_json::from_str(
        r#"{"to":"w1:p1","body":"float subject","subject":0.10000000000000001}"#,
    )
    .unwrap();
    let sent = dispatcher
        .dispatch("inbox_send", &args, &request("w1:p1"))
        .unwrap();
    assert_eq!(sent["notified"], true);
    {
        let outbox = outbox_lock(&services.flock().outbox);
        let notice = &outbox["w1:p1"][0].text;
        assert!(notice.contains(": 0.1."), "{notice}");
        assert!(!notice.contains("0.10000000000000001"), "{notice}");
    }
    let overflow_subject: serde_json::Value =
        serde_json::from_str(r#"{"to":"w1:p1","body":"overflow subject","subject":1e400}"#)
            .unwrap();
    let sent = dispatcher
        .dispatch("inbox_send", &overflow_subject, &request("w1:p1"))
        .unwrap();
    assert_eq!(sent["notified"], true);
    assert_eq!(sent["subject"], "Inf");
    {
        let outbox = outbox_lock(&services.flock().outbox);
        assert!(outbox["w1:p1"][0].text.contains(": inf."));
    }
    dispatcher.close();
}

#[test]
fn inbox_read_task_failure_precedes_mark_and_receipt() {
    let (_dir, services) = make_services("task-failure", factory(None), None);
    let mut stores = services.flock().stores();
    let store = stores.inbox.clone().unwrap();
    let id = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("leave unread"),
            ..Default::default()
        })
        .unwrap();
    stores.tasks = None;
    services.flock().attach_stores(stores);
    let dispatcher = services.dispatcher(handlers());
    assert!(dispatcher
        .dispatch("inbox_read", &json!({}), &request("w1:p1"))
        .unwrap_err()
        .to_string()
        .contains("task store is not attached"));
    assert!(store.get_message(&id).unwrap().read_at.is_none());
    assert!(store.recover_delivery("w1:p1").unwrap().is_empty());
    let read = dispatcher
        .dispatch(
            "inbox_read",
            &json!({"include_tasks":false}),
            &request("w1:p1"),
        )
        .unwrap();
    assert_eq!(read["marked_read"], 1);
    dispatcher.close();
}

#[test]
fn a_desk_inbox_read_never_marks_a_message_its_response_leaves_out() {
    // t3539 review (PR #826): the dispatcher clamps a desk response again, so the response must fit as
    // sent, and every message it carries (and only those) must be read. Sweep the last message's size
    // across the cap, with and without a task queue large enough to take the cap.
    use crate::roles::{py_dumps, DESK_RESULT_BYTES};
    let (_dir, services) = make_services("desk-sweep", factory(None), None);
    let stores = services.flock().stores();
    let inbox = stores.inbox.clone().unwrap();
    let tasks = stores.tasks.clone().unwrap();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w3:p3");
    for with_queues in [false, true] {
        if with_queues {
            for k in 0..40 {
                runtime
                    .block_on(tasks.add(
                        "global",
                        &format!("queued work {k} {}", "q".repeat(120)),
                        "",
                    ))
                    .unwrap();
            }
        }
        for last in (3400..=4300).step_by(12) {
            let mut sent = Vec::new();
            for body in [300, 300, last] {
                let id = inbox
                    .send(&crate::inbox_store::Outgoing {
                        mailbox: "w3:p3",
                        body: Some("x".repeat(body).as_str()),
                        ..Default::default()
                    })
                    .unwrap();
                sent.push(id);
            }
            let read = dispatcher.dispatch("inbox_read", &json!({}), &ctx).unwrap();
            let size = py_dumps(&read).len();
            let carried: Vec<String> = read
                .get("messages")
                .and_then(|rows| rows.as_array())
                .map(|rows| {
                    rows.iter()
                        .map(|row| row["message_id"].as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            // A response that carries messages is within the cap as sent. When nothing fits, the
            // capped `head` shape goes out instead (its head is cap-length by design, so it is larger);
            // then no message may be marked read.
            if !carried.is_empty() {
                assert!(
                    size <= DESK_RESULT_BYTES,
                    "queues={with_queues} last={last}: {size} bytes as sent, carrying {carried:?}"
                );
            }
            // What the response says about a message it carries matches the store: read, with a read time.
            for row in read
                .get("messages")
                .and_then(|rows| rows.as_array())
                .into_iter()
                .flatten()
            {
                assert_eq!(
                    json!("read"),
                    row["state"],
                    "carried rows show the read state"
                );
                assert!(!row["read_at"].is_null(), "carried rows show the read time");
            }
            // The capped shape has no marked_read key: nothing was carried, so nothing was marked.
            let reported = read.get("marked_read").cloned().unwrap_or(json!(0));
            assert_eq!(
                json!(carried.len()),
                reported,
                "queues={with_queues} last={last}: the count reported is the count carried"
            );
            for id in &sent {
                let row = inbox.get_message(id).unwrap();
                if carried.contains(id) {
                    assert!(row.read_at.is_some(), "carried {id} must be read");
                } else {
                    assert!(
                        row.read_at.is_none(),
                        "{id} is not in the response, so it must stay unread (queues={with_queues} last={last})"
                    );
                }
            }
        }
    }
    dispatcher.close();
}

#[test]
fn an_inbox_read_leaves_an_unowned_global_task_unowned_and_still_lists_it() {
    // t3554: an inbox read never assigns a task, and the read still returns the task list.
    let (_dir, services) = make_services("read-no-claim", factory(None), None);
    let stores = services.flock().stores();
    let inbox = stores.inbox.clone().unwrap();
    inbox
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("mail for a lane"),
            ..Default::default()
        })
        .unwrap();
    let tasks = stores.tasks.clone().unwrap();
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let task = runtime
        .block_on(tasks.add(
            "global",
            "unowned work",
            "a read of mail must not hand this to a lane",
        ))
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let read = dispatcher
        .dispatch("inbox_read", &json!({}), &request("w1:p1"))
        .unwrap();
    let listed = read["tasks"]["global"]
        .as_array()
        .expect("the read still returns the task list")
        .iter()
        .any(|row| row["id"] == task.id);
    assert!(listed, "the unowned global task is listed: {read}");
    let after = runtime.block_on(tasks.get(&task.id)).unwrap();
    assert_eq!("open", after.status);
    let assignees: Vec<String> = serde_json::from_str(&after.assignees).unwrap();
    assert!(assignees.is_empty(), "the read assigned {assignees:?}");
    assert!(runtime
        .block_on(tasks.leases_for_pane("w1:p1", None))
        .unwrap()
        .is_empty());
    dispatcher.close();
}

#[test]
fn inbox_send_folds_actual_queued_notice_and_returns_pre_notify_snapshot() {
    use crate::flock::sends::outbox_lock;
    let (_dir, services) = make_services("fold", factory(None), None);
    let mut notice =
        crate::flock::sends::Message::new("notice-test", "w1:p1", "worker", "old", "test");
    notice.is_notice = true;
    outbox_lock(&services.flock().outbox).insert("w1:p1".into(), vec![notice]);
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    let mut ids = Vec::new();
    for body in ["one", "two"] {
        let sent = dispatcher
            .dispatch("inbox_send", &json!({"to":"w1:p1","body":body}), &ctx)
            .unwrap();
        assert_eq!(sent["notified"], true);
        assert!(sent["notify_state"].is_null());
        let id = sent["message_id"].as_str().unwrap().to_owned();
        let status = dispatcher
            .dispatch("inbox_status", &json!({"message_id":id}), &ctx)
            .unwrap();
        assert_eq!(status["notify"]["state"], "queued");
        assert_eq!(status["notify"]["reason"], "folded into notice-test");
        ids.push(id);
    }
    {
        let outbox = outbox_lock(&services.flock().outbox);
        assert_eq!(outbox["w1:p1"].len(), 1);
        assert_eq!(outbox["w1:p1"][0].notify_for, ids);
        assert!(outbox["w1:p1"][0].text.contains("2 unread"));
    }
    dispatcher.close();
}

#[test]
fn inbox_quiet_and_process_mailbox_override_are_observed_without_global_env_races() {
    const CHILD: &str = "DS_INBOX_API_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "server_runtime::inbox_api::tests::inbox_quiet_and_process_mailbox_override_are_observed_without_global_env_races", "--nocapture"])
            .env(CHILD, "1")
            .env("DUNKINGSHEEP_INBOX_SILENT_MAILBOXES", "w1:p1")
            .env("DUNKINGSHEEP_MAILBOX", "w1:p1")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (_dir, services) = make_services_with_env(
        "quiet",
        factory(None),
        None,
        Env::from_pairs(&[("DUNKINGSHEEP_MAILBOX", "w2:p2")]),
    );
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    let sent = dispatcher
        .dispatch(
            "inbox_send",
            &json!({"to":"w1:p1","body":"quiet","subject":9}),
            &ctx,
        )
        .unwrap();
    assert_eq!(sent["notified"], false);
    assert!(crate::flock::sends::outbox_lock(&services.flock().outbox).is_empty());
    dispatcher.close();
    // Overrides are fallback sources only: the registered caller must be absent
    // from the backend to expose process versus captured configuration.
    let absent: LoopTerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.panes.clear();
        Box::new(terminal)
    });
    let (_fallback_dir, fallback) = make_services_with_env(
        "override",
        absent,
        None,
        Env::from_pairs(&[("DUNKINGSHEEP_MAILBOX", "w2:p2")]),
    );
    fallback
        .flock()
        .stores()
        .inbox
        .unwrap()
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("fallback"),
            ..Default::default()
        })
        .unwrap();
    let dispatcher = fallback.dispatcher(handlers());
    let direct = dispatcher.dispatch("inbox_list", &json!({}), &ctx).unwrap();
    assert_eq!(direct["mailbox"], "w1:p1");
    assert_eq!(direct["messages"].as_array().unwrap().len(), 1);
    let cached = dispatcher
        .dispatch("inbox_count", &json!({"fresh":true}), &ctx)
        .unwrap();
    assert_eq!(cached["mailbox"], "w2:p2");
    assert_eq!(cached["unread"], 0);
    dispatcher.close();
}

#[test]
fn inbox_archive_accepts_python_iso_forms_and_state_precedence() {
    let (_dir, services) = make_services("iso", factory(None), None);
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");
    // Values checked against dunk_api._archive_before / datetime.fromisoformat.
    for (text, expected) in [
        ("2024-01-02T03:04:05Z", 1704164645.0),
        ("20240102X030405+00:00", 1704164645.0),
        ("2024-W01-2🦀03:04:05+00", 1704164645.0),
        ("2024W012T03:04:05,25+00:00", 1704164645.25),
        ("2024-01-02T03+01", 1704160800.0),
        ("2024-01-02T03:04:05+01:02:03.5", 1704160921.5),
    ] {
        let result = dispatcher
            .dispatch(
                "inbox_archive",
                &json!({"mailbox":"w1:p1","before":text,"state":null}),
                &ctx,
            )
            .unwrap();
        assert_eq!(result["before"], expected, "{text}");
        assert_eq!(result["states"], json!(["queued", "acked"]));
    }
    let error = dispatcher
        .dispatch(
            "inbox_archive",
            &json!({"mailbox":"w1:p1","before":"bad"}),
            &ctx,
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("before must be an ISO date/time or a unix timestamp: 'bad'"));
    dispatcher.close();
}

#[test]
fn inbox_default_provider_refuses_without_invoking_callback() {
    use crate::api::CommandEnv;
    use crate::flock::panes::PaneInfoEnv;
    use std::sync::Mutex;
    struct Missing(Mutex<Box<dyn crate::terminal::Terminal>>);
    impl PaneInfoEnv for Missing {
        fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }
        fn get_pane(&self, _: &str) -> Option<serde_json::Value> {
            None
        }
    }
    impl CommandEnv for Missing {
        fn terminal(&self) -> &Mutex<Box<dyn crate::terminal::Terminal>> {
            &self.0
        }
        fn now_s(&self) -> f64 {
            0.0
        }
    }
    let missing = Missing(Mutex::new(Box::new(FakeTerminal::new())));
    assert!(missing
        .with_inbox_env(&mut |_| panic!("no inbox provider"))
        .unwrap_err()
        .to_string()
        .contains("inbox command adapter is not wired"));
}

#[test]
fn inbox_archive_python_normalizes_skipped_local_time() {
    const CHILD: &str = "DS_INBOX_DST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "server_runtime::inbox_api::tests::inbox_archive_python_normalizes_skipped_local_time", "--nocapture"])
            .env(CHILD, "1")
            .env("TZ", "America/Los_Angeles")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (_dir, services) = make_services("dst-gap", factory(None), None);
    let dispatcher = services.dispatcher(handlers());
    let result = dispatcher
        .dispatch(
            "inbox_archive",
            &json!({"mailbox":"w1:p1","before":"2024-03-10T02:30:00"}),
            &request("w1:p1"),
        )
        .unwrap();
    // Python datetime.fromisoformat(...).timestamp() normalizes the gap with
    // the pre-transition PST offset (UTC 10:30), rather than rejecting it.
    assert_eq!(result["before"], 1710066600.0);
    dispatcher.close();
}

#[test]
fn inbox_fetch_rejects_truthy_non_path_before_any_write() {
    let (dir, services) = make_services("bad-fetch-dest", factory(None), None);
    let store = services.flock().stores().inbox.unwrap();
    let attachment = dir.path().join("fetch.txt");
    std::fs::write(&attachment, b"payload").unwrap();
    let id = store
        .send(&crate::inbox_store::Outgoing {
            mailbox: "w1:p1",
            body: Some("with attachment"),
            attachments: &[attachment],
            ..Default::default()
        })
        .unwrap();
    let dispatcher = services.dispatcher(handlers());
    let accidental = std::env::current_dir().unwrap().join("True");
    assert!(!accidental.exists());
    let error = dispatcher
        .dispatch(
            "inbox_fetch",
            &json!({"attachment_id":format!("{id}-1"),"dest":true}),
            &request("w1:p1"),
        )
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("expected str, bytes or os.PathLike object, not bool"));
    assert!(
        !accidental.exists(),
        "truthy invalid path must not create destination"
    );
    let false_id = dispatcher
        .dispatch(
            "inbox_fetch",
            &json!({"attachment_id":false}),
            &request("w1:p1"),
        )
        .unwrap_err();
    assert!(false_id.to_string().contains("no attachment with id False"));
    for (raw, expected) in [
        (r#"{"attachment_id":1e-7}"#, "no attachment with id 1e-07"),
        (r#"{"attachment_id":1.00}"#, "no attachment with id 1.0"),
        (
            r#"{"attachment_id":0.10000000000000001}"#,
            "no attachment with id 0.1",
        ),
        (
            r#"{"attachment_id":1.0000000000000001}"#,
            "no attachment with id 1.0",
        ),
        (r#"{"attachment_id":1e400}"#, "no attachment with id inf"),
    ] {
        let args: serde_json::Value = serde_json::from_str(raw).unwrap();
        let error = dispatcher
            .dispatch("inbox_fetch", &args, &request("w1:p1"))
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{raw}: {error}");
    }
    let unsupported = dispatcher
        .dispatch(
            "inbox_fetch",
            &json!({"attachment_id":[],"dest":true}),
            &request("w1:p1"),
        )
        .unwrap_err();
    assert!(unsupported
        .to_string()
        .contains("Error binding parameter 1: type 'list' is not supported"));
    let missing = dispatcher
        .dispatch(
            "inbox_fetch",
            &json!({"attachment_id":"missing","dest":true}),
            &request("w1:p1"),
        )
        .unwrap_err();
    assert!(missing
        .to_string()
        .contains("no attachment with id 'missing'"));
    let blob_dir = dir.path().join("bad-fetch-dest-blobs");
    let blob = std::fs::read_dir(&blob_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(blob).unwrap();
    let missing_blob = dispatcher
        .dispatch(
            "inbox_fetch",
            &json!({"attachment_id":format!("{id}-1"),"dest":true}),
            &request("w1:p1"),
        )
        .unwrap_err();
    assert!(missing_blob
        .to_string()
        .contains(&format!("attachment {id}-1 is missing from the store")));
    dispatcher.close();
}

#[test]
fn inbox_archive_falsey_mailbox_uses_python_required_mailbox_error() {
    let (_dir, services) = make_services("empty-archive-mailbox", factory(None), None);
    let dispatcher = services.dispatcher(handlers());
    for mailbox in [json!(false), json!(0)] {
        let error = dispatcher
            .dispatch(
                "inbox_archive",
                &json!({"mailbox":mailbox}),
                &request("w1:p1"),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("a mailbox is required"),
            "{error}"
        );
    }
    let error = dispatcher
        .dispatch("inbox_archive", &json!({"mailbox":""}), &request("w1:p1"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing required argument(s): mailbox"),
        "{error}"
    );
    dispatcher.close();
}
