//! Ports of the rule tests of tests/test_dunk_task_model.py and tests/test_dunk_tasks.py that
//! need only the pure rules (`task_scope`, `task_assignees`, `check_task_add`,
//! `require_task_visible`) and the store-backed wrappers. The tests that go through
//! `task_add`'s store write, `task_next` and `task_complete` are NOT ported here.

use super::*;
use crate::flock::panes::Stores;

const OWNER: &str = "w2:p1";
const WORKER: &str = "w2:p2";
const OTHER: &str = "w9:p9";

fn refused(result: Result<(), DunkError>) -> String {
    result.expect_err("expected a refusal").to_string()
}

fn owners(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

async fn flock_with_store(tag: &str) -> (Flock, Arc<Tasks>) {
    let dir = std::env::temp_dir().join(format!("ds-ct-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    let tasks = Arc::new(Tasks::open(&path).await.unwrap());
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        inbox: None,
    });
    (flock, tasks)
}

// -- _task_scope ---------------------------------------------------------------------

#[test]
fn an_empty_or_all_scope_is_the_default() {
    for scope in [None, Some(""), Some("  "), Some("all")] {
        assert_eq!(
            "global",
            Flock::task_scope(scope, Some(WORKER), "global").unwrap()
        );
    }
    assert_eq!("visible", Flock::task_scope(None, None, "visible").unwrap());
}

#[test]
fn self_names_the_callers_pane_and_needs_one() {
    assert_eq!(
        WORKER,
        Flock::task_scope(Some("self"), Some(WORKER), "global").unwrap()
    );
    let error = Flock::task_scope(Some("self"), None, "global")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("scope 'self' requires a known caller pane"),
        "{error}"
    );
}

#[test]
fn scope_is_private_and_global_is_visible() {
    assert_eq!(
        "global",
        Flock::task_scope(Some("global"), Some(WORKER), "x").unwrap()
    );
    assert_eq!(
        WORKER,
        Flock::task_scope(Some(WORKER), Some(WORKER), "x").unwrap()
    );
    let error = Flock::task_scope(Some(OTHER), Some(WORKER), "x")
        .unwrap_err()
        .to_string();
    assert_eq!(
        format!("per-pane tasks are private to {OTHER}; caller runs in {WORKER}"),
        error
    );
    // A caller with no pane may name any pane for inspection.
    assert_eq!(OTHER, Flock::task_scope(Some(OTHER), None, "x").unwrap());
}

#[test]
fn a_tmux_pane_id_is_a_valid_scope_and_garbage_is_not() {
    assert_eq!("%4", Flock::task_scope(Some("%4"), None, "x").unwrap());
    assert!(Flock::task_scope(Some("not a scope"), None, "x").is_err());
}

// -- _task_assignees -----------------------------------------------------------------

#[test]
fn self_and_me_expand_to_the_callers_pane_and_blanks_are_dropped() {
    assert_eq!(
        owners(&[WORKER, OTHER, WORKER]),
        Flock::task_assignees(&["self", " w9:p9 ", "me", ""], Some(WORKER)).unwrap()
    );
    assert!(Flock::task_assignees(&[], Some(WORKER)).unwrap().is_empty());
    let error = Flock::task_assignees(&["me"], None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("self-assign requires a known caller pane"),
        "{error}"
    );
}

#[test]
fn a_task_with_no_assignee_has_no_default_owner() {
    assert!(Flock::default_task_owner(Some(WORKER)).is_empty());
    assert!(Flock::default_task_owner(None).is_empty());
}

// -- task_add's refusals --------------------------------------------------------------

#[test]
fn a_worker_may_only_self_assign() {
    let error = refused(Flock::check_task_add(
        "global",
        &owners(&[OTHER]),
        "worker",
        None,
        None,
        Some(WORKER),
    ));
    assert!(error.contains("only to yourself"), "{error}");
    assert!(Flock::check_task_add(
        "global",
        &owners(&[WORKER]),
        "worker",
        None,
        None,
        Some(WORKER)
    )
    .is_ok());
}

#[test]
fn pane_caller_assigning_another_lane_on_global_needs_the_owner_role() {
    let args =
        |role| Flock::check_task_add("global", &owners(&[OTHER]), role, None, None, Some(OWNER));
    assert!(refused(args("worker")).contains("only to yourself"));
    assert!(args("owner").is_ok());
}

#[test]
fn anonymous_caller_cannot_create_in_another_panes_private_queue() {
    let error = refused(Flock::check_task_add(
        "w2:p2",
        &[],
        "anonymous",
        None,
        None,
        None,
    ));
    assert!(
        error.contains("--scope global"),
        "the workaround is named: {error}"
    );
    // The legitimate path: a no-pane caller creating a GLOBAL task and assigning a lane.
    assert!(
        Flock::check_task_add("global", &owners(&[WORKER]), "anonymous", None, None, None).is_ok()
    );
}

#[test]
fn a_free_model_cannot_file_reviews_but_the_system_and_the_owner_can() {
    for role in ["worker", "reviewer", "desk", "planner"] {
        let error = refused(Flock::check_task_add(
            "global",
            &[],
            role,
            Some("review"),
            None,
            Some(WORKER),
        ));
        assert!(
            error.contains("filed by the review timer") || error.contains("kind=work only"),
            "{role}: {error}"
        );
    }
    for role in ["system", "owner", "web"] {
        assert!(
            Flock::check_task_add("global", &[], role, Some("review"), None, None).is_ok(),
            "{role}"
        );
    }
}

#[test]
fn desk_and_planner_file_work_only_and_lanes_file_open_tasks_only() {
    for role in ["desk", "planner"] {
        let error = refused(Flock::check_task_add(
            "global",
            &[],
            role,
            Some("verify"),
            None,
            Some(WORKER),
        ));
        assert_eq!(format!("role {role} files kind=work only"), error);
    }
    let error = refused(Flock::check_task_add(
        "global",
        &[],
        "worker",
        None,
        Some("done"),
        Some(WORKER),
    ));
    assert_eq!(
        "role worker files open tasks; status moves through next/complete/block",
        error
    );
    for status in [None, Some(""), Some("open")] {
        assert!(Flock::check_task_add("global", &[], "worker", None, status, Some(WORKER)).is_ok());
    }
    assert!(
        Flock::check_task_add("global", &[], "owner", None, Some("blocked"), Some(OWNER)).is_ok()
    );
}

// -- _require_task_visible and the wrappers -------------------------------------------

#[tokio::test]
async fn a_private_task_is_not_visible_to_another_pane() {
    let (flock, tasks) = flock_with_store("visible").await;
    let private = tasks.add(WORKER, "mine", "").await.unwrap();
    let global = tasks.add("global", "fleet", "").await.unwrap();
    assert!(Flock::require_task_visible(&private, Some(WORKER)).is_ok());
    assert!(
        Flock::require_task_visible(&private, None).is_ok(),
        "no pane sees all"
    );
    let error = Flock::require_task_visible(&private, Some(OTHER))
        .unwrap_err()
        .to_string();
    assert_eq!(
        format!(
            "task {} belongs to another pane's private queue ({WORKER})",
            private.id
        ),
        error
    );
    assert!(Flock::require_task_visible(&global, Some(OTHER)).is_ok());
    assert!(flock
        .task_block(&private.id, None, Some(OTHER))
        .await
        .is_err());
    assert_eq!(
        "open",
        tasks.get(&private.id).await.unwrap().status,
        "nothing changed"
    );
}

#[tokio::test]
async fn block_unblock_cancel_and_delete_go_through_the_store() {
    let (flock, tasks) = flock_with_store("lifecycle").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    let blocked = flock
        .task_block(&task.id, Some("needs-owner: which?"), Some(WORKER))
        .await
        .unwrap();
    assert_eq!("blocked", blocked.status);
    assert_eq!(
        Some("needs-owner: which?".to_string()),
        blocked.block_reason
    );
    assert_eq!(
        "open",
        flock
            .task_unblock(&task.id, Some(WORKER))
            .await
            .unwrap()
            .status
    );
    assert_eq!(
        "cancelled",
        flock
            .task_cancel(&task.id, Some("no"), Some(WORKER))
            .await
            .unwrap()
            .status
    );
    let gone = tasks.add("global", "gone", "").await.unwrap();
    assert_eq!(
        gone.id,
        flock.task_delete(&gone.id, Some(WORKER)).await.unwrap()
    );
    assert!(tasks.get(&gone.id).await.is_err());
}

#[tokio::test]
async fn assign_expands_self_and_refuses_a_pane_less_self() {
    let (flock, tasks) = flock_with_store("assign").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    let assigned = flock
        .task_assign(&task.id, &["self"], "add", Some(WORKER))
        .await
        .unwrap();
    assert!(
        assigned.assignees.contains(WORKER),
        "{}",
        assigned.assignees
    );
    let error = flock
        .task_assign(&task.id, &["me"], "add", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("self-assign requires a known caller pane"),
        "{error}"
    );
}

#[tokio::test]
async fn note_records_an_event_without_a_visibility_check() {
    let (flock, tasks) = flock_with_store("note").await;
    let private = tasks.add(WORKER, "mine", "").await.unwrap();
    // The Python does not require the task visible for a note.
    let before = tasks.events(&private.id).await;
    flock
        .task_note(&private.id, None, Some("evidence"), Some(OTHER))
        .await
        .unwrap();
    assert_eq!(
        before + 1,
        tasks.events(&private.id).await,
        "exactly one note event"
    );
}

#[tokio::test]
async fn a_missing_store_is_a_named_refusal() {
    let dir = std::env::temp_dir().join(format!("ds-ct-nostore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let flock = Flock::new(dir.join("l.json"), dir.join("r.json"), false);
    let error = flock
        .task_unblock("t1", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("task store is not attached"), "{error}");
}
