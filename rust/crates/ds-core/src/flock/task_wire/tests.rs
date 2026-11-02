use super::*;
use crate::{
    flock::panes::Stores,
    tasks::{wire::tests::oracle, Tasks},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct RequestEnv {
    reads: AtomicUsize,
}
impl PaneInfoEnv for RequestEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        vec![
            json!({"pane_id":"%4","pane_pid":444}),
            json!({"pane_id":"%1","pane_pid":111}),
        ]
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

async fn flock() -> (crate::testsupport::Scratch<Flock>, Arc<Tasks>, RequestEnv) {
    let dir = crate::testsupport::dir("flock-task-wire");
    let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut roles = crate::roles::Roles::new(
        &crate::config::Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .unwrap();
    roles
        .register("%4", "worker", Some(444), None, None, None, None, None)
        .unwrap();
    roles
        .register("%1", "owner", Some(111), None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (
        crate::testsupport::Scratch::new(flock, dir),
        tasks,
        RequestEnv {
            reads: AtomicUsize::new(0),
        },
    )
}

#[tokio::test]
async fn flock_task_wire_add_uses_request_environment_roles_defaults_and_full_row() {
    let (flock, tasks, env) = flock().await;
    let low = flock
        .task_add_wire(
            "default",
            None,
            None,
            TaskAddArgs::default(),
            Some("%4"),
            &env,
        )
        .await
        .unwrap();
    assert_eq!(
        env.reads.load(Ordering::SeqCst),
        1,
        "the supplied live PaneInfoEnv must reach caller_role"
    );
    assert_eq!(low.typed.priority, "low");
    assert_eq!(low.wire["created_by"], "%4");
    assert_eq!(low.wire["assignees"], json!([]));
    assert_eq!(low.wire["children"], json!([]));
    assert_eq!(
        low.wire,
        oracle(
            tasks.path(),
            "get",
            json!({"task_id":low.typed.id,"pane_id":"%4"})
        )["ok"]
    );
    let high = flock
        .task_add_wire(
            "explicit",
            Some("self"),
            Some("body"),
            TaskAddArgs {
                priority: Some("high"),
                parent_id: Some(&low.typed.id),
                assignees: &["me"],
                ..Default::default()
            },
            Some("%4"),
            &env,
        )
        .await
        .unwrap();
    assert_eq!(high.typed.priority, "high");
    assert_eq!(high.wire["scope"], "%4");
    assert_eq!(high.wire["assignees"], json!(["%4"]));
    assert_eq!(high.wire["parent"]["id"], low.typed.id);
    assert_eq!(
        high.wire,
        oracle(
            tasks.path(),
            "get",
            json!({"task_id":high.typed.id,"pane_id":"%4"})
        )["ok"]
    );
    let read = flock
        .task_read_wire_with_notes(&high.typed.id, Some("%4"))
        .await
        .unwrap();
    assert_eq!(
        read.wire,
        oracle(
            tasks.path(),
            "task_read",
            json!({"task_id":high.typed.id,"self_pane_id":"%4"})
        )["ok"]
    );
    let refused = flock
        .task_add_wire(
            "delegate",
            None,
            None,
            TaskAddArgs {
                assignees: &["%9"],
                ..Default::default()
            },
            Some("%4"),
            &env,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(refused,"task add may assign only to yourself for role worker; handing work to another lane is the owner's (lanes pull with task next)");
    let delegated = flock
        .task_add_wire(
            "delegate",
            None,
            None,
            TaskAddArgs {
                assignees: &["%9"],
                kind: Some("review"),
                author: Some("%1"),
                repo: Some("fixture/repo"),
                pr: Some("41"),
                head: Some("head"),
                ..Default::default()
            },
            Some("%1"),
            &env,
        )
        .await
        .unwrap();
    assert_eq!(
        env.reads.load(Ordering::SeqCst),
        2,
        "request pane selects the owner registration"
    );
    assert_eq!(delegated.wire["assignees"], json!(["%9"]));
    assert_eq!(delegated.wire["repo"], "fixture/repo");
    assert_eq!(delegated.wire["author"], "%1");
    assert_eq!(delegated.wire["head"], "head");
    assert_eq!(delegated.wire["pr"], 41);
    assert_eq!(
        delegated.wire,
        oracle(
            tasks.path(),
            "get",
            json!({"task_id":delegated.typed.id,"pane_id":"%1"})
        )["ok"]
    );
}

#[tokio::test]
async fn flock_task_wire_list_matches_python_default_zero_negative_and_scope_privacy() {
    let (flock, tasks, env) = flock().await;
    // Enough rows to distinguish the 200 default from unbounded and negative slicing.
    for n in 0..205 {
        tasks.add("global", &format!("task {n}"), "").await.unwrap();
    }
    let private = flock
        .task_add_wire(
            "private",
            Some("self"),
            None,
            TaskAddArgs::default(),
            Some("%4"),
            &env,
        )
        .await
        .unwrap();
    tasks.add("%99", "hidden", "").await.unwrap();
    for limit in [None, Some(0), Some(-1), Some(-999), Some(1), Some(201)] {
        for scope in [None, Some("all"), Some("global"), Some("self")] {
            let (explicit, rows) = flock
                .task_list_wire(scope, None, None, true, limit, Some("%4"), None, false)
                .await
                .unwrap();
            let actual = json!({"scope":explicit.as_deref().unwrap_or("visible"),"pane_id":"%4","tasks":rows.iter().map(|r| &r.wire).collect::<Vec<_>>()});
            assert_eq!(
                actual,
                oracle(
                    tasks.path(),
                    "task_list",
                    json!({"scope":scope,"limit":limit,"self_pane_id":"%4"})
                )["ok"],
                "scope={scope:?}, limit={limit:?}"
            );
            if limit.is_none() || limit == Some(0) {
                assert_eq!(rows.len(), 200);
            }
        }
    }
    assert!(
        tasks
            .list_visible_wire(None, None, None, true, Some(0), Some("%4"), None, false)
            .await
            .unwrap()
            .is_empty(),
        "store0 is [], while Flock0 is 200"
    );
    for pane in [Some("%1"), Some("%99")] {
        let error = flock
            .task_read_wire_with_notes(&private.typed.id, pane)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            oracle(
                tasks.path(),
                "task_read",
                json!({"task_id":private.typed.id,"self_pane_id":pane})
            )["error"]
        );
    }
    let error = flock
        .task_list_wire(
            Some("%99"),
            Some("bad"),
            None,
            true,
            None,
            Some("%4"),
            None,
            false,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        oracle(
            tasks.path(),
            "task_list",
            json!({"scope":"%99","status":"bad","self_pane_id":"%4"})
        )["error"],
        "scope refusal precedes filter validation"
    );
    assert!(flock
        .task_read_wire_with_notes(&private.typed.id, None)
        .await
        .is_ok());
    assert_eq!(
        flock
            .task_read_wire_with_notes("missing", None)
            .await
            .unwrap_err()
            .to_string(),
        tasks.get("missing").await.unwrap_err().to_string()
    );
}

#[tokio::test]
async fn flock_task_wire_wrappers_require_attached_store() {
    let dir = crate::testsupport::dir("flock-task-wire-no-store");
    let flock = Flock::new(dir.path().join("l.json"), dir.path().join("r.json"), false);
    let env = RequestEnv {
        reads: AtomicUsize::new(0),
    };
    let error = "the task store is not attached";
    assert_eq!(
        flock
            .task_read_wire_with_notes("t1", None)
            .await
            .unwrap_err()
            .to_string(),
        error
    );
    assert_eq!(
        flock
            .task_list_wire(None, None, None, true, None, None, None, false)
            .await
            .unwrap_err()
            .to_string(),
        error
    );
    assert_eq!(
        flock
            .task_add_wire("x", None, None, TaskAddArgs::default(), None, &env)
            .await
            .unwrap_err()
            .to_string(),
        error
    );
}
