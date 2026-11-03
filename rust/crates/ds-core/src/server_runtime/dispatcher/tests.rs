use std::sync::Arc;

use serde_json::json;
use tokio::runtime::{Builder, Handle};

use super::{RuntimeDispatcher, TerminalFactory};
use crate::api;
use crate::config::Env;
use crate::flock::{panes::Stores, Flock};
use crate::metrics::MetricsStore;
use crate::proc::Caller;
use crate::roles::Roles;
use crate::server::{Dispatcher, RequestCtx};
use crate::terminal::{fakes::FakeTerminal, TerminalError};

fn handlers() -> api::Dispatcher {
    let mut handlers = api::Dispatcher::new();
    handlers.register_status_whoami_turn_start();
    handlers
}

fn factory(fail_grouped_panes: Option<TerminalError>) -> TerminalFactory {
    Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.fail_grouped_panes = fail_grouped_panes.clone();
        Box::new(terminal)
    })
}

fn make_flock(tag: &str, metrics: Option<Arc<MetricsStore>>) -> (tempfile::TempDir, Arc<Flock>) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join(format!("{tag}-launches.json")),
        dir.path().join(format!("{tag}-lanes.json")),
        false,
    ));
    let mut roles = Roles::new(
        &Env::default(),
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
    flock.attach_roles(roles, None);
    if let Some(metrics) = metrics {
        flock.attach_stores(Stores {
            metrics: Some(metrics),
            ..Stores::default()
        });
    }
    (dir, flock)
}

fn make_dispatcher(
    flock: Arc<Flock>,
    runtime: &Handle,
    terminal_factory: TerminalFactory,
) -> RuntimeDispatcher {
    RuntimeDispatcher::new(flock, runtime.clone(), terminal_factory, handlers())
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

#[test]
fn production_dispatcher_uses_real_handler_registry_and_gate() {
    let (_dir, flock) = make_flock("dispatch", None);
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let dispatcher = make_dispatcher(Arc::clone(&flock), runtime.handle(), factory(None));
    let ctx = request("w1:p1");

    let identity = dispatcher.dispatch("whoami", &json!({}), &ctx).unwrap();
    assert_eq!(identity["pane_id"], "w1:p1");
    assert_eq!(identity["role"], "worker");
    assert_eq!(identity["caller"]["name"], "codex");
    assert_eq!(
        flock.current_caller(),
        None,
        "API guard clears request identity"
    );

    let denied = dispatcher
        .dispatch("create_workspace", &json!({}), &ctx)
        .unwrap_err();
    assert!(denied.to_string().contains("requires a superuser"));
}

#[test]
fn pane_lookup_preserves_absence_and_backend_failure_and_metrics_are_best_effort() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
    let (_flock_dir, flock) = make_flock("pane-info", Some(Arc::clone(&metrics)));
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let dispatcher = make_dispatcher(Arc::clone(&flock), runtime.handle(), factory(None));
    assert_eq!(
        dispatcher.pane_info("w1:p1").unwrap().unwrap().pane_pid,
        Some(1101)
    );
    assert!(dispatcher.pane_info("w9:p9").unwrap().is_none());

    let no_pid_factory: TerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.panes[0].pane_pid = None;
        Box::new(terminal)
    });
    let (_no_pid_dir, no_pid_flock) = make_flock("pane-no-pid", None);
    let no_pid = make_dispatcher(Arc::clone(&no_pid_flock), runtime.handle(), no_pid_factory);
    assert!(no_pid
        .pane_info("w1:p1")
        .unwrap()
        .unwrap()
        .pane_pid
        .is_none());

    let (_failure_dir, failure_flock) = make_flock("pane-failure", Some(Arc::clone(&metrics)));
    let failing_factory: TerminalFactory = Arc::new(|| {
        let mut terminal = FakeTerminal::new();
        terminal.panes.clear();
        terminal.fail_grouped_panes =
            Some(TerminalError::unavailable("fixture pane listing failed"));
        Box::new(terminal)
    });
    let failing = make_dispatcher(
        Arc::clone(&failure_flock),
        runtime.handle(),
        failing_factory,
    );
    assert_eq!(
        failing.pane_info("w1:p1").unwrap_err().to_string(),
        "fixture pane listing failed"
    );

    failing.record_identity_mismatch("w1:p1", "pane_not_in_ancestry");
    let events = metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "identity.mismatch");
    assert_eq!(events[0].pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(events[0].source.as_deref(), Some("pane_not_in_ancestry"));

    let (_no_metrics_dir, no_metrics_flock) = make_flock("no-metrics", None);
    let no_metrics = make_dispatcher(
        Arc::clone(&no_metrics_flock),
        runtime.handle(),
        factory(None),
    );
    no_metrics.record_identity_mismatch("w1:p1", "pane_not_in_ancestry");
}

#[test]
fn pane_lookup_uses_last_good_role_record_after_empty_backend_answer() {
    let (_dir, flock) = make_flock("pane-info-cache", None);
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let empty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory_empty = Arc::clone(&empty);
    let terminal_factory: TerminalFactory = Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        if factory_empty.load(std::sync::atomic::Ordering::Relaxed) {
            terminal.panes.clear();
        }
        Box::new(terminal)
    });
    let dispatcher = make_dispatcher(Arc::clone(&flock), runtime.handle(), terminal_factory);

    let initial = dispatcher.pane_info_at("w1:p1", 100.0).unwrap().unwrap();
    assert_eq!(initial.pane_pid, Some(1101));
    empty.store(true, std::sync::atomic::Ordering::Relaxed);
    let held = dispatcher.pane_info_at("w1:p1", 103.0).unwrap().unwrap();
    assert_eq!(held.pane_pid, Some(1101));
}
