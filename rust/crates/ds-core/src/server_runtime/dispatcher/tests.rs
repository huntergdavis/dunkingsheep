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
    handlers.register_status_whoami_turn_start();
    handlers
}

fn factory(fail_grouped_panes: Option<TerminalError>) -> LoopTerminalFactory {
    Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.fail_grouped_panes = fail_grouped_panes.clone();
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
