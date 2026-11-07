//! Parity with the Python push sweep for a pid-bound reviewer (dunk_core.py `_push_tasks`,
//! live 2026-10-30): tests/test_push_on_idle.py::test_a_pid_bound_reviewer_on_an_allowed_provider_is_fed
//! and tests/test_dunk_roles.py::test_may_review_trusts_the_resolved_role_of_a_pid_bound_pane.
//!
//! The sweep must hand `may_review` the role `caller_role` resolved from the live pane record.
//! A bare registry resolution (no live record) says "worker" for a pid-bound lane, so a sweep
//! that re-resolved would skip the reviewer as `provider_may_not_review`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::Env;
use crate::error::DunkError;
use crate::flock::loop_env::LoopFuture;
use crate::flock::loops_push::PushEnv;
use crate::flock::panes::Stores;
use crate::flock::Flock;
use crate::roles::Roles;
use crate::server_runtime::LoopRuntimeAdapter;
use crate::tasks::{Task, Tasks};
use crate::terminal::fakes::FakeTerminal;

/// The sweep's view of the adapter, with the pane list pinned so the test controls it.
struct PinnedEnv<'a> {
    adapter: &'a LoopRuntimeAdapter,
    panes: Vec<Value>,
}

impl PushEnv for PinnedEnv<'_> {
    fn list_panes(&self) -> LoopFuture<'_, Result<Vec<Value>, DunkError>> {
        let panes = self.panes.clone();
        Box::pin(async move { Ok(panes) })
    }

    fn caller_role(&self, pane_id: &str, pane_info: Option<&Value>) -> String {
        self.adapter.helper_pushenv_caller_role(pane_id, pane_info)
    }

    fn registered_lanes(&self) -> Option<BTreeSet<String>> {
        self.adapter.helper_pushenv_registered_lanes()
    }

    fn may_review(&self, pane_id: &str, role: &str) -> bool {
        self.adapter.helper_pushenv_may_review(pane_id, role)
    }

    fn provider_of(&self, pane_id: &str) -> Option<String> {
        self.adapter.helper_pushenv_provider_of(pane_id)
    }

    fn lane_agent_alive(&self, _: &str) -> Option<bool> {
        // The Python test sets `herdr.agent_child_pids = lambda pid: None`: the process table
        // cannot say, which never blocks a push. Pinned here because the pid 2202 of this
        // fixture does not exist, so the real /proc read would report "no agent" (dead).
        // Liveness itself is covered by push_parity_tests.rs.
        None
    }

    fn unread_mail<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, bool> {
        self.adapter.helper_pushenv_unread_mail(pane_id)
    }

    fn announce_task<'a>(&'a self, _: &'a str, _: &'a Task, _: f64) -> LoopFuture<'a, ()> {
        Box::pin(async move {})
    }
}

/// `file_review` as in flock/loops_push_tests.rs: a review-kind task, set through a second
/// connection because `Tasks.writer` is private to the tasks module.
async fn file_review(tasks: &Tasks, title: &str) -> Task {
    let task = tasks.add("global", title, "").await.unwrap();
    let conn = rusqlite::Connection::open(tasks.path()).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute(
        "UPDATE tasks SET kind = 'review', author = 'review-sync' WHERE id = ?",
        rusqlite::params![task.id],
    )
    .unwrap();
    task
}

#[tokio::test]
async fn parity_pid_bound_pi_reviewer_is_fed_a_review_not_work() {
    let dir = crate::testsupport::dir("push-parity-pid-bound-reviewer");
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    ));
    let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Stores::default()
    });
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("roles store");
    // tests/test_push_on_idle.py sets reviewer_providers = [claude, codex, pi] for this lane.
    roles.reviewer_providers = vec!["claude".into(), "codex".into(), "pi".into()];
    roles
        .register(
            "w2:p2",
            "reviewer",
            Some(2202),
            None,
            Some("pi"),
            None,
            None,
            None,
        )
        .expect("register pid-bound pi reviewer");
    flock.attach_roles(roles, None);

    let adapter = LoopRuntimeAdapter::new(
        Arc::clone(&flock),
        Arc::new(|| Box::new(FakeTerminal::new())),
    );
    let panes = adapter
        .helper_pushenv_list_panes()
        .await
        .expect("list live fake panes");
    let pane_map: BTreeMap<String, Value> = panes
        .iter()
        .filter_map(|pane| {
            pane.get("pane_id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), pane.clone()))
        })
        .collect();
    let env = PinnedEnv {
        adapter: &adapter,
        panes: panes.clone(),
    };

    // The live record resolves the reviewer role...
    assert_eq!(env.caller_role("w2:p2", pane_map.get("w2:p2")), "reviewer");
    // ...while a bare resolution (no live record) demotes it, as Python's
    // test_may_review_trusts_the_resolved_role_of_a_pid_bound_pane asserts.
    assert!(!flock
        .with_roles(|roles, _| roles.may_review("w2:p2", None))
        .unwrap());
    // The sweep's trait call carries the resolved role, so the reviewer is allowed...
    assert!(env.may_review("w2:p2", "reviewer"));
    // ...and a caller resolved to any other role is not (Python: `role != "reviewer"`).
    assert!(!env.may_review("w2:p2", "worker"));

    let work = tasks.add("global", "ordinary work", "").await.unwrap();
    let review = file_review(&tasks, "review the pid-bound pi lane").await;
    flock.push_tasks(&env).await;

    assert_eq!(
        tasks.get(&review.id).await.unwrap().status,
        "in_progress",
        "the pid-bound pi reviewer is fed the review"
    );
    assert_eq!(
        tasks.get(&work.id).await.unwrap().status,
        "open",
        "a reviewer lane is never pushed ordinary work"
    );
}
