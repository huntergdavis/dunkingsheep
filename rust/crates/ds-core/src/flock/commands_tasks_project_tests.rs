//! t4701: a new task takes its creator lane's project.

use super::*;
use crate::flock::panes::Stores;
use crate::tasks::lane_project::ProjectScope;
use serde_json::{json, Value};

const WORKER: &str = "%1";
const PLANNER: &str = "%2";
const OWNER: &str = "%3";
const UNBOUND: &str = "%4";

/// Every pane runs as `pid`.
struct Panes(i64);

impl PaneInfoEnv for Panes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, id: &str) -> Option<Value> {
        Some(json!({"pane_id": id, "pane_pid": self.0}))
    }
}

fn live() -> Panes {
    Panes(i64::from(std::process::id()))
}

/// Worker in project alpha, planner in project beta, owner seat global, one unbound worker.
async fn fixture() -> (tempfile::TempDir, Flock, Arc<Tasks>) {
    let dir = crate::testsupport::dir("task-project");
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
    for (pane, role) in [
        (WORKER, "worker"),
        (PLANNER, "planner"),
        (OWNER, "owner"),
        (UNBOUND, "worker"),
    ] {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .unwrap();
    }
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    for (pane, scope) in [
        (WORKER, ProjectScope::Project("alpha".into())),
        (PLANNER, ProjectScope::Project("beta".into())),
        (OWNER, ProjectScope::Global),
    ] {
        flock.lane_project_set(&live(), pane, scope).unwrap();
    }
    (dir, flock, tasks)
}

async fn add(
    flock: &Flock,
    tasks: &Tasks,
    pane: &str,
    env: &Panes,
    opts: TaskAddArgs<'_>,
    project: TaskProject<'_>,
) -> (String, Option<String>) {
    let task = flock
        .task_add_with_project("a task", None, None, opts, Some(pane), env, None, project)
        .await
        .unwrap();
    let project = tasks.task_project(&task.id).await.unwrap();
    (task.id, project)
}

#[tokio::test]
async fn a_worker_and_a_planner_file_tasks_under_their_own_project() {
    let (_dir, flock, tasks) = fixture().await;
    let none = TaskAddArgs::default();
    let (_, worker) = add(&flock, &tasks, WORKER, &live(), none, TaskProject::Creator).await;
    assert_eq!(worker.as_deref(), Some("alpha"));
    let (_, planner) = add(&flock, &tasks, PLANNER, &live(), none, TaskProject::Creator).await;
    assert_eq!(planner.as_deref(), Some("beta"));
    // The plain `task_add` every caller uses takes the creator's project too.
    let plain = flock
        .task_add("plain", None, None, none, Some(WORKER), &live())
        .await
        .unwrap();
    assert_eq!(
        tasks.task_project(&plain.id).await.unwrap().as_deref(),
        Some("alpha")
    );
}

#[tokio::test]
async fn a_global_member_keeps_filing_unmapped_tasks_even_under_a_mapped_parent() {
    let (_dir, flock, tasks) = fixture().await;
    let none = TaskAddArgs::default();
    let (_, project) = add(&flock, &tasks, OWNER, &live(), none, TaskProject::Creator).await;
    assert_eq!(project, None);
    let (parent, _) = add(&flock, &tasks, WORKER, &live(), none, TaskProject::Creator).await;
    let child = TaskAddArgs {
        parent_id: Some(&parent),
        ..Default::default()
    };
    let (_, project) = add(&flock, &tasks, OWNER, &live(), child, TaskProject::Creator).await;
    assert_eq!(project, None);
}

#[tokio::test]
async fn an_explicit_project_or_no_project_overrides_the_creators() {
    let (_dir, flock, tasks) = fixture().await;
    let none = TaskAddArgs::default();
    let (_, named) = add(
        &flock,
        &tasks,
        WORKER,
        &live(),
        none,
        TaskProject::Named("gamma"),
    )
    .await;
    assert_eq!(named.as_deref(), Some("gamma"));
    let (_, cleared) = add(&flock, &tasks, WORKER, &live(), none, TaskProject::None).await;
    assert_eq!(cleared, None);
    // The owner seat names a project the same way.
    let (_, owner) = add(
        &flock,
        &tasks,
        OWNER,
        &live(),
        none,
        TaskProject::Named("alpha"),
    )
    .await;
    assert_eq!(owner.as_deref(), Some("alpha"));
    for bad in ["", " alpha", "alpha "] {
        let refused = flock
            .task_add_with_project(
                "bad",
                None,
                None,
                none,
                Some(WORKER),
                &live(),
                None,
                TaskProject::Named(bad),
            )
            .await
            .expect_err("a malformed project name is refused");
        assert!(refused.to_string().contains("task project"), "{refused}");
    }
}

#[tokio::test]
async fn a_caller_without_a_project_takes_the_parents() {
    let (_dir, flock, tasks) = fixture().await;
    let none = TaskAddArgs::default();
    let (orphan, project) = add(&flock, &tasks, UNBOUND, &live(), none, TaskProject::Creator).await;
    assert_eq!(project, None, "no membership and no parent");
    let (parent, _) = add(&flock, &tasks, WORKER, &live(), none, TaskProject::Creator).await;
    let child = TaskAddArgs {
        parent_id: Some(&parent),
        ..Default::default()
    };
    let (_, project) = add(
        &flock,
        &tasks,
        UNBOUND,
        &live(),
        child,
        TaskProject::Creator,
    )
    .await;
    assert_eq!(project.as_deref(), Some("alpha"));
    // A parent with no project gives none.
    let child = TaskAddArgs {
        parent_id: Some(&orphan),
        ..Default::default()
    };
    let (_, project) = add(
        &flock,
        &tasks,
        UNBOUND,
        &live(),
        child,
        TaskProject::Creator,
    )
    .await;
    assert_eq!(project, None);
    // The creator's own project wins over the parent's.
    let child = TaskAddArgs {
        parent_id: Some(&parent),
        ..Default::default()
    };
    let (_, project) = add(
        &flock,
        &tasks,
        PLANNER,
        &live(),
        child,
        TaskProject::Creator,
    )
    .await;
    assert_eq!(project.as_deref(), Some("beta"));
    // `--no-project` also stops the parent's project.
    let (_, project) = add(&flock, &tasks, UNBOUND, &live(), child, TaskProject::None).await;
    assert_eq!(project, None);
}

#[tokio::test]
async fn a_stale_membership_gives_no_project() {
    let (_dir, flock, tasks) = fixture().await;
    let none = TaskAddArgs::default();
    // The pane id now belongs to another process (pid 1): the membership row is not its.
    let (_, project) = add(
        &flock,
        &tasks,
        WORKER,
        &Panes(1),
        none,
        TaskProject::Creator,
    )
    .await;
    assert_eq!(project, None);
    // A pane whose process cannot be read at all.
    let (_, project) = add(
        &flock,
        &tasks,
        WORKER,
        &Panes(i64::MAX),
        none,
        TaskProject::Creator,
    )
    .await;
    assert_eq!(project, None);
    // The same pane on its registered process still gets its project.
    let (_, project) = add(&flock, &tasks, WORKER, &live(), none, TaskProject::Creator).await;
    assert_eq!(project.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn a_private_task_gets_no_project_row() {
    let (_dir, flock, tasks) = fixture().await;
    let task = flock
        .task_add_with_project(
            "mine",
            Some("self"),
            None,
            TaskAddArgs::default(),
            Some(WORKER),
            &live(),
            None,
            TaskProject::Creator,
        )
        .await
        .unwrap();
    assert_eq!(task.scope, WORKER);
    assert_eq!(tasks.task_project(&task.id).await.unwrap(), None);
}
