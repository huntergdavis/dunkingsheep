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
use crate::terminal::{fakes::FakeTerminal, Terminal, TerminalError};

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

fn flock(tag: &str, metrics: Option<Arc<MetricsStore>>) -> (tempfile::TempDir, Arc<Flock>) {
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
    let (_dir, flock) = flock("dispatch", None);
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
    assert!(denied
        .to_string()
        .contains("create_workspace is not permitted for role worker"));
}

#[test]
fn pane_lookup_preserves_absence_and_backend_failure_and_metrics_are_best_effort() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
    let (_flock_dir, flock) = flock("pane-info", Some(Arc::clone(&metrics)));
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
    let no_pid = make_dispatcher(Arc::clone(&flock), runtime.handle(), no_pid_factory);
    assert!(no_pid
        .pane_info("w1:p1")
        .unwrap()
        .unwrap()
        .pane_pid
        .is_none());

    let failing = make_dispatcher(
        Arc::clone(&flock),
        runtime.handle(),
        factory(Some(TerminalError::unavailable(
            "fixture pane listing failed",
        ))),
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

    let (_no_metrics_dir, no_metrics_flock) = flock("no-metrics", None);
    let no_metrics = make_dispatcher(
        Arc::clone(&no_metrics_flock),
        runtime.handle(),
        factory(None),
    );
    no_metrics.record_identity_mismatch("w1:p1", "pane_not_in_ancestry");
}
