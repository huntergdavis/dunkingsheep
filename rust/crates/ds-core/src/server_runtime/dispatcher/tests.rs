use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

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
    handlers.register_status_whoami_turn_start();
    handlers
}

fn dunk_handlers() -> api::Dispatcher {
    let mut handlers = api::Dispatcher::new();
    handlers.register_stop_all_fire_list_dunks();
    handlers
}

fn factory(fail_grouped_panes: Option<TerminalError>) -> LoopTerminalFactory {
    Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.fail_grouped_panes = fail_grouped_panes.clone();
        Box::new(terminal)
    })
}

fn thread_tracking_factory(threads: Arc<Mutex<Vec<ThreadId>>>) -> LoopTerminalFactory {
    Arc::new(move || {
        threads.lock().unwrap().push(std::thread::current().id());
        Box::new(FakeTerminal::new())
    })
}

fn add_dunk(
    services: &RuntimeServices,
    text: &str,
    start: bool,
    interval: &str,
    max_sends: Option<&str>,
) {
    let pane = json!({"pane_id": "w1:p1", "tab_label": "1"});
    services
        .add_dunk(
            pane.as_object(),
            &crate::flock::dunks::AddArgs {
                text: Some(text.into()),
                interval: Some(interval.into()),
                start,
                max_sends: max_sends.map(str::to_owned),
                hold_while_typing: Some("false".into()),
                ..Default::default()
            },
        )
        .unwrap();
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

#[test]
fn production_dispatcher_uses_real_handler_registry_and_gate() {
    let (_dir, services) = make_services("dispatch", factory(None), None);
    let dispatcher = services.dispatcher(handlers());
    let ctx = request("w1:p1");

    let identity = dispatcher.dispatch("whoami", &json!({}), &ctx).unwrap();
    assert_eq!(identity["pane_id"], "w1:p1");
    assert_eq!(identity["role"], "worker");
    assert_eq!(identity["caller"]["name"], "codex");
    assert_eq!(
        services.flock().current_caller(),
        None,
        "API guard clears request identity"
    );

    let denied = dispatcher
        .dispatch("create_workspace", &json!({}), &ctx)
        .unwrap_err();
    assert!(denied.to_string().contains("requires a superuser"));

    dispatcher.close();
    let after_close = dispatcher.dispatch("whoami", &json!({}), &ctx).unwrap();
    assert_eq!(after_close["pane_id"], "w1:p1");
}

#[test]
fn dispatcher_close_keeps_real_outbox_workers_until_owner_shutdown() {
    use crate::server_runtime::outbox_runtime::FlockSnapshot;
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    let (worker_started_tx, worker_started_rx) = mpsc::channel();
    let (release_worker_tx, release_worker_rx) = mpsc::sync_channel(0);
    let release_worker_rx = Mutex::new(release_worker_rx);
    let request_thread = std::thread::current().id();
    let terminal_factory: LoopTerminalFactory = Arc::new(move || {
        if std::thread::current().id() != request_thread {
            worker_started_tx.send(()).unwrap();
            release_worker_rx.lock().unwrap().recv().unwrap();
        }
        Box::new(FakeTerminal::new())
    });
    let (_dir, services) = make_services("dispatch-outbox-close", terminal_factory, None);
    let dispatcher = services.dispatcher(handlers());

    dispatcher.close();
    let message = services
        .flock()
        .new_message("w1:p1", "w1:p1", "outbox survives close", "test")
        .unwrap();
    services
        .flock()
        .enqueue(message, services.outbox.pool.as_ref(), &FlockSnapshot)
        .expect("dispatcher close must leave the owner-held outbox accepting work");
    worker_started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("outbox worker did not start after dispatcher close");

    let shutdown_services = Arc::clone(&services);
    let (shutdown_done_tx, shutdown_done_rx) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        shutdown_services.shutdown();
        shutdown_done_tx.send(()).unwrap();
    });
    assert!(
        shutdown_done_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "owner shutdown must join the active worker"
    );

    release_worker_tx.send(()).unwrap();
    shutdown_done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("owner shutdown did not join the outbox worker");
    shutdown.join().unwrap();
}

#[test]
fn dispatcher_fire_dunk_is_synchronous_expands_text_and_keeps_manual_max_sends() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let (_dir, services) = make_services(
        "dispatcher-manual-fire-sync",
        thread_tracking_factory(Arc::clone(&threads)),
        None,
    );
    add_dunk(&services, "hello from {id}", false, "60", Some("1"));
    let dispatcher = services.dispatcher(dunk_handlers());
    let request_thread = std::thread::current().id();
    let result = dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "d1", "countdown_s": 0, "wait": true}),
            &request("w2:p2"),
        )
        .unwrap();

    assert_eq!(result["id"], "d1");
    assert_eq!(result["ok"], true);
    assert_eq!(result["queued"], false);
    let row = services.flock().dunks.get("d1").unwrap();
    assert_eq!(row["send_count"], 1);
    assert_eq!(row["last_sent_text"], "hello from d1");
    assert!(
        services.flock().dunks.get("d1").is_ok(),
        "manual fire never applies max_sends removal"
    );
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .any(|thread| *thread != request_thread));
    services.shutdown();
}

#[test]
fn dispatcher_stop_all_uses_runtime_and_invalidates_shared_dunk_view() {
    let (_dir, services) = make_services("dispatcher-stop-all", factory(None), None);
    add_dunk(&services, "scheduled", true, "60", None);
    let dispatcher = services.dispatcher(dunk_handlers());
    let owner = request("w2:p2");
    let before = dispatcher
        .dispatch("list_dunks", &json!({}), &owner)
        .unwrap();
    assert_eq!(before["dunks"][0]["running"], true);

    let stopped = dispatcher.dispatch("stop_all", &json!({}), &owner).unwrap();
    assert_eq!(stopped["dunks"][0]["running"], false);
    let after = dispatcher
        .dispatch("list_dunks", &json!({}), &owner)
        .unwrap();
    assert_eq!(after["dunks"][0]["running"], false);
    services.shutdown();
}

#[test]
fn dispatcher_fire_wait_false_runs_countdown_and_joins_before_shutdown_teardown() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let (_dir, services) = make_services(
        "dispatcher-manual-fire-async",
        thread_tracking_factory(Arc::clone(&threads)),
        None,
    );
    add_dunk(&services, "async {id}", false, "60", None);
    let dispatcher = services.dispatcher(dunk_handlers());
    let (countdown, step_rx, release_step) = super::super::CountdownGate::channel();
    let (submit, submit_rx, release_submit) = super::super::SubmitGate::channel();
    services.set_manual_fire_test_gates(Some(countdown), Some(submit));
    let request_thread = std::thread::current().id();
    let scheduled = dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "d1", "countdown_s": 2, "wait": false}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(
        scheduled,
        json!({"id": "d1", "ok": null, "message": "test send scheduled"})
    );
    assert_eq!(step_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
    assert_eq!(
        services.flock().dunks.get("d1").unwrap()["status"],
        "Test in 2..."
    );
    release_step.send(()).unwrap();
    assert_eq!(step_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    assert_eq!(
        services.flock().dunks.get("d1").unwrap()["status"],
        "Test in 1..."
    );
    release_step.send(()).unwrap();
    submit_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(services.flock().dunks.get("d1").unwrap()["send_count"], 0);

    let stopping = Arc::clone(&services);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        stopping.shutdown();
        shutdown_tx.send(()).unwrap();
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !services.manual_fire_is_stopped() {
        assert!(
            std::time::Instant::now() < deadline,
            "shutdown did not close manual-fire admission"
        );
        std::thread::yield_now();
    }
    assert!(
        shutdown_rx.try_recv().is_err(),
        "shutdown must wait for the active submit worker"
    );
    release_submit.send(()).unwrap();
    shutdown_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shutdown joins the active manual-fire worker");
    shutdown.join().unwrap();

    let row = services.flock().dunks.get("d1").unwrap();
    assert_eq!(row["send_count"], 1);
    assert_eq!(row["last_sent_text"], "async d1");
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .any(|thread| *thread != request_thread));
    assert_eq!(services.completed_manual_fires().len(), 1);
    services.shutdown(); // repeated shutdown is harmless
}

#[test]
fn dispatcher_shutdown_cancels_manual_countdown_before_worker_terminal_creation() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let (_dir, services) = make_services(
        "dispatcher-manual-fire-cancel",
        thread_tracking_factory(Arc::clone(&threads)),
        None,
    );
    add_dunk(&services, "cancel {id}", false, "60", None);
    let dispatcher = services.dispatcher(dunk_handlers());
    let (countdown, step_rx, release_step) = super::super::CountdownGate::channel();
    services.set_manual_fire_test_gates(Some(countdown), None);
    let request_thread = std::thread::current().id();
    dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "d1", "countdown_s": 1, "wait": false}),
            &request("w2:p2"),
        )
        .unwrap();
    assert_eq!(step_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    assert_eq!(
        services.flock().dunks.get("d1").unwrap()["status"],
        "Test in 1..."
    );

    let stopping = Arc::clone(&services);
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        stopping.shutdown();
        shutdown_tx.send(()).unwrap();
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !services.manual_fire_is_stopped() {
        assert!(
            std::time::Instant::now() < deadline,
            "shutdown did not close manual-fire admission"
        );
        std::thread::yield_now();
    }
    release_step.send(()).unwrap();
    shutdown_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shutdown wakes and joins the countdown worker");
    shutdown.join().unwrap();
    let row = services.flock().dunks.get("d1").unwrap();
    assert_eq!(row["status"], "Test failed");
    assert_eq!(
        row["last_error"],
        "manual dunk fire cancelled during runtime shutdown"
    );
    assert_eq!(services.completed_manual_fires().len(), 1);
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .all(|thread| *thread == request_thread));
}

#[test]
fn dispatcher_manual_fire_retains_captured_lifetime_across_same_id_replacement() {
    let (_dir, services) = make_services("dispatcher-manual-fire-replacement", factory(None), None);
    add_dunk(&services, "old {id}", false, "60", None);
    let old_handle = services.flock().dunks.handle("d1").unwrap();
    let dispatcher = services.dispatcher(dunk_handlers());
    let (countdown, step_rx, release_step) = super::super::CountdownGate::channel();
    services.set_manual_fire_test_gates(Some(countdown), None);
    let dispatching = Arc::clone(&dispatcher);
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let request_ctx = request("w2:p2");
    let request_thread = std::thread::spawn(move || {
        result_tx
            .send(dispatching.dispatch(
                "fire_dunk",
                &json!({"id": "d1", "countdown_s": 1, "wait": true}),
                &request_ctx,
            ))
            .unwrap();
    });
    assert_eq!(step_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);

    services.remove_dunk("d1").unwrap();
    let replacement = json!({
        "next_id": 2,
        "dunkers": [{
            "id": "d1",
            "target_pane_id": "w1:p1",
            "target_tab": "1",
            "text": "replacement {id}",
            "created_at": 3.0,
            "hold_while_typing": false
        }]
    });
    services
        .flock()
        ._load_dunks(replacement.as_object().unwrap());
    release_step.send(()).unwrap();
    let result = result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("captured fire completed")
        .unwrap();
    request_thread.join().unwrap();
    assert_eq!(result["ok"], true);
    let old = crate::flock::dunks::DunkList::handle_row(&old_handle);
    assert_eq!(old["last_sent_text"], "old d1");
    assert_eq!(old["send_count"], 1);
    let replacement = services.flock().dunks.get("d1").unwrap();
    assert_eq!(replacement["text"], "replacement {id}");
    assert_eq!(replacement["send_count"], 0);
    services.shutdown();
}

#[test]
fn dispatcher_fire_argument_errors_precede_lookup_and_invalid_targets_do_not_spawn() {
    let threads = Arc::new(Mutex::new(Vec::new()));
    let (_dir, services) = make_services(
        "dispatcher-manual-fire-errors",
        thread_tracking_factory(Arc::clone(&threads)),
        None,
    );
    let pane = None;
    services
        .add_dunk(
            pane,
            &crate::flock::dunks::AddArgs {
                text: Some("no target".into()),
                start: false,
                ..Default::default()
            },
        )
        .unwrap();
    let dispatcher = services.dispatcher(dunk_handlers());
    let owner = request("w2:p2");
    let bad_integer = dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "not-a-dunk", "countdown_s": "bad"}),
            &owner,
        )
        .unwrap_err();
    assert_eq!(
        bad_integer.to_string(),
        "invalid literal for int() with base 10: 'bad'"
    );
    let missing_id = dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "missing", "countdown_s": 0}),
            &owner,
        )
        .unwrap_err();
    assert_eq!(missing_id.to_string(), "no dunk with id \"missing\"");
    let no_target = dispatcher
        .dispatch("fire_dunk", &json!({"id": "d1", "countdown_s": 0}), &owner)
        .unwrap_err();
    assert_eq!(no_target.to_string(), "d1 has no target");
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .all(|thread| *thread == std::thread::current().id()));
    services.shutdown();
}

#[test]
fn dispatcher_manual_fire_spawn_failure_records_error_without_orphan_worker() {
    let (_dir, services) = make_services("dispatcher-manual-fire-spawn-error", factory(None), None);
    add_dunk(&services, "spawn failure", false, "60", None);
    services.fail_next_manual_fire_spawn();
    let dispatcher = services.dispatcher(dunk_handlers());
    let error = dispatcher
        .dispatch(
            "fire_dunk",
            &json!({"id": "d1", "countdown_s": 0, "wait": true}),
            &request("w2:p2"),
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "could not start manual dunk fire: injected thread spawn failure"
    );
    let row = services.flock().dunks.get("d1").unwrap();
    assert_eq!(row["status"], "Test failed");
    assert_eq!(row["last_error"], "injected thread spawn failure");
    assert!(services.completed_manual_fires().is_empty());
    services.shutdown();
}

#[test]
fn dispatcher_manual_fire_preserves_running_timer_deadline() {
    let (_dir, services) = make_services(
        "dispatcher-manual-fire-timer-preserved",
        factory(None),
        None,
    );
    add_dunk(&services, "timer {id}", true, "60", None);
    let before = services.flock().dunks.get("d1").unwrap();
    let deadline = before["next_send_at"].as_f64().unwrap();
    let dispatcher = services.dispatcher(dunk_handlers());
    let (countdown, step_rx, release_step) = super::super::CountdownGate::channel();
    services.set_manual_fire_test_gates(Some(countdown), None);
    let dispatching = Arc::clone(&dispatcher);
    let request_ctx = request("w2:p2");
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        result_tx
            .send(dispatching.dispatch(
                "fire_dunk",
                &json!({"id":"d1","countdown_s":1,"wait":true}),
                &request_ctx,
            ))
            .unwrap();
    });
    assert_eq!(step_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
    release_step.send(()).unwrap();
    assert_eq!(
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()["ok"],
        true
    );
    worker.join().unwrap();
    let after = services.flock().dunks.get("d1").unwrap();
    assert_eq!(after["running"], true);
    assert_eq!(after["next_send_at"].as_f64().unwrap(), deadline);
    services.stop_dunk("d1").unwrap();
    services.shutdown();
}

#[test]
fn pane_lookup_preserves_absence_and_backend_failure_and_metrics_are_best_effort() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
    let (_flock_dir, services) =
        make_services("pane-info", factory(None), Some(Arc::clone(&metrics)));
    let dispatcher = services.dispatcher(handlers());
    assert_eq!(
        dispatcher.pane_info("w1:p1").unwrap().unwrap().pane_pid,
        Some(1101)
    );
    assert!(dispatcher.pane_info("w9:p9").unwrap().is_none());

    let no_pid_factory: LoopTerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.panes[0].pane_pid = None;
        Box::new(terminal)
    });
    let (_no_pid_dir, no_pid_services) = make_services("pane-no-pid", no_pid_factory, None);
    let no_pid = no_pid_services.dispatcher(handlers());
    assert!(no_pid
        .pane_info("w1:p1")
        .unwrap()
        .unwrap()
        .pane_pid
        .is_none());

    let (_failure_dir, failure_services) = make_services(
        "pane-failure",
        Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes.clear();
            terminal.fail_grouped_panes =
                Some(TerminalError::unavailable("fixture pane listing failed"));
            Box::new(terminal)
        }),
        Some(Arc::clone(&metrics)),
    );
    let failing = failure_services.dispatcher(handlers());
    for _ in 0..2 {
        assert_eq!(
            failing.pane_info("w1:p1").unwrap_err().to_string(),
            "fixture pane listing failed"
        );
    }

    failing.record_identity_mismatch("w1:p1", "pane_not_in_ancestry");
    let events = metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "identity.mismatch");
    assert_eq!(events[0].pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(events[0].source.as_deref(), Some("pane_not_in_ancestry"));

    let (_no_metrics_dir, no_metrics_services) = make_services("no-metrics", factory(None), None);
    let no_metrics = no_metrics_services.dispatcher(handlers());
    no_metrics.record_identity_mismatch("w1:p1", "pane_not_in_ancestry");
}

#[test]
fn pane_lookup_uses_last_good_role_record_after_empty_backend_answer() {
    let empty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory_empty = Arc::clone(&empty);
    let terminal_factory: LoopTerminalFactory = Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        if factory_empty.load(std::sync::atomic::Ordering::Relaxed) {
            terminal.panes.clear();
        }
        Box::new(terminal)
    });
    let (_dir, services) = make_services("pane-info-cache", terminal_factory, None);
    let dispatcher = services.dispatcher(handlers());

    let initial = dispatcher.pane_info("w1:p1").unwrap().unwrap();
    assert_eq!(initial.pane_pid, Some(1101));
    std::thread::sleep(std::time::Duration::from_millis(2_100));
    empty.store(true, std::sync::atomic::Ordering::Relaxed);
    let held = dispatcher.pane_info("w1:p1").unwrap().unwrap();
    assert_eq!(held.pane_pid, Some(1101));
}
