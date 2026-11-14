//! t4704: `task block --waiting-on`: a block that says what the task waits for.

use super::*;
use crate::flock::panes::Stores;
use crate::tasks::block_kind::kind_of;
use serde_json::Value;

const OWNER: &str = "%1";

struct NoPanes;

impl PaneInfoEnv for NoPanes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

async fn fixture() -> (tempfile::TempDir, Flock, Arc<Tasks>) {
    let dir = crate::testsupport::dir("task-block-kind");
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
        .register(OWNER, "owner", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (dir, flock, tasks)
}

async fn new_task(flock: &Flock, title: &str) -> Task {
    flock
        .task_add(
            title,
            None,
            None,
            TaskAddArgs::default(),
            Some(OWNER),
            &NoPanes,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn each_waiting_on_kind_is_stored_as_its_reason_prefix() {
    let (_dir, flock, _tasks) = fixture().await;
    for (waiting_on, why, reason, kind) in [
        (
            "owner",
            Some("which port?"),
            "needs-owner: which port?",
            "owner",
        ),
        (
            "evidence",
            Some("exact-head CI"),
            "waiting-evidence: exact-head CI",
            "evidence",
        ),
        (
            "release",
            Some("next daemon"),
            "waiting-release: next daemon",
            "release",
        ),
        ("numbers", None, "waiting-numbers:", "numbers"),
        (
            "pr:owner/repo#42",
            Some("review"),
            "waiting-pr:owner/repo#42: review",
            "pr",
        ),
    ] {
        let task = new_task(&flock, waiting_on).await;
        let blocked = flock
            .task_block_waiting_on(&task.id, waiting_on, why, Some(OWNER))
            .await
            .unwrap();
        assert_eq!(blocked.status, "blocked", "{waiting_on}");
        assert_eq!(
            blocked.block_reason.as_deref(),
            Some(reason),
            "{waiting_on}"
        );
        assert_eq!(
            kind_of(blocked.block_reason.as_deref()),
            kind,
            "{waiting_on}"
        );
    }
}

#[tokio::test]
async fn waiting_on_the_owner_is_the_needs_owner_block_the_owner_report_lists() {
    let (_dir, flock, _tasks) = fixture().await;
    let asked = new_task(&flock, "a question").await;
    let other = new_task(&flock, "waits for a release").await;
    flock
        .task_block_waiting_on(&asked.id, "owner", Some("pick one"), Some(OWNER))
        .await
        .unwrap();
    flock
        .task_block_waiting_on(&other.id, "release", None, Some(OWNER))
        .await
        .unwrap();
    // `task list --needs-owner` is unchanged: it lists the owner wait and nothing else.
    let (_, listed) = flock
        .task_list(None, None, None, false, 0, Some(OWNER), None, true)
        .await
        .unwrap();
    let ids: Vec<&str> = listed.iter().map(|row| row.task.id.as_str()).collect();
    assert_eq!(ids, [asked.id.as_str()]);
}

#[tokio::test]
async fn waiting_on_a_task_is_the_dependency_block_and_completion_reopens_it() {
    let (_dir, flock, tasks) = fixture().await;
    let first = new_task(&flock, "do this first").await;
    let second = new_task(&flock, "then this").await;
    let blocked = flock
        .task_block_waiting_on(&second.id, &format!("task:{}", first.id), None, Some(OWNER))
        .await
        .unwrap();
    assert_eq!(
        blocked.block_reason.as_deref(),
        Some(format!("blocked-by:{}", first.id).as_str())
    );
    assert_eq!(kind_of(blocked.block_reason.as_deref()), "task");
    // A reason beside a task wait is refused: the reason is the task.
    let refused = flock
        .task_block_waiting_on(
            &second.id,
            &format!("task:{}", first.id),
            Some("and also because"),
            Some(OWNER),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("takes no reason"), "{refused}");
    // An unknown task and the task itself are refused by the store.
    for bad in ["task:t999".to_string(), format!("task:{}", second.id)] {
        assert!(flock
            .task_block_waiting_on(&second.id, &bad, None, Some(OWNER))
            .await
            .is_err());
    }
    // Completing the first task returns the second to the queue.
    tasks.complete(&first.id).await.unwrap();
    assert_eq!(tasks.get(&second.id).await.unwrap().status, "open");
}

#[tokio::test]
async fn the_other_typed_waits_are_not_reopened_by_a_completion_and_unblock_is_manual() {
    let (_dir, flock, tasks) = fixture().await;
    let first = new_task(&flock, "first").await;
    let parked = new_task(&flock, "parked").await;
    // A dependency on `first`, but the block itself is a release wait: the owner's kind of
    // block, which a completion must not open.
    tasks
        .update(
            &parked.id,
            crate::tasks::FieldEdit {
                depends_on: Some(vec![first.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    flock
        .task_block_waiting_on(&parked.id, "release", Some("next daemon"), Some(OWNER))
        .await
        .unwrap();
    tasks.complete(&first.id).await.unwrap();
    assert_eq!(tasks.get(&parked.id).await.unwrap().status, "blocked");
    let opened = flock.task_unblock(&parked.id, Some(OWNER)).await.unwrap();
    assert_eq!(opened.status, "open");
    assert_eq!(opened.block_reason, None);
}

#[tokio::test]
async fn a_malformed_waiting_on_blocks_nothing() {
    let (_dir, flock, tasks) = fixture().await;
    let task = new_task(&flock, "stays open").await;
    for bad in ["review", "pr:repo", "task:", ""] {
        let refused = flock
            .task_block_waiting_on(&task.id, bad, None, Some(OWNER))
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("waiting_on must be"), "{bad}: {refused}");
    }
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");
}
