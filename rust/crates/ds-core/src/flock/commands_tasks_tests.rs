//! Ports of the rule tests of tests/test_dunk_task_model.py and tests/test_dunk_tasks.py that
//! need only the pure rules (`task_scope`, `task_assignees`, `check_task_add`,
//! `require_task_visible`) and the store-backed wrappers. The tests that go through
//! `task_add`'s store write, `task_next` and `task_complete` are NOT ported here.

use super::*;
use crate::flock::panes::Stores;

const OWNER: &str = "w2:p1";
const WORKER: &str = "w2:p2";
const OTHER: &str = "w9:p9";

fn refused<T: std::fmt::Debug>(result: Result<T, DunkError>) -> String {
    result.expect_err("expected a refusal").to_string()
}

fn owners(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

async fn flock_with_store(tag: &str) -> (crate::testsupport::Scratch<Flock>, Arc<Tasks>) {
    let scratch = crate::testsupport::dir(&format!("ct-{tag}"));
    let dir = scratch.path().to_path_buf();
    let tasks = Arc::new(Tasks::open(dir.join("tasks.db")).await.unwrap());
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (crate::testsupport::Scratch::new(flock, scratch), tasks)
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
        &[OTHER],
        "worker",
        None,
        None,
        Some(WORKER),
    ));
    assert!(error.contains("only to yourself"), "{error}");
    assert!(Flock::check_task_add("global", &[WORKER], "worker", None, None, Some(WORKER)).is_ok());
}

#[test]
fn pane_caller_assigning_another_lane_on_global_needs_the_owner_role() {
    let args = |role| Flock::check_task_add("global", &[OTHER], role, None, None, Some(OWNER));
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
    assert!(Flock::check_task_add("global", &[WORKER], "anonymous", None, None, None).is_ok());
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
            .task
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
    let scratch_dir = crate::testsupport::dir("ct-nostore");
    let dir = scratch_dir.path().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    let flock = Flock::new(dir.join("l.json"), dir.join("r.json"), false);
    let error = flock
        .task_unblock("t1", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("task store is not attached"), "{error}");
}

#[tokio::test]
async fn a_reasonless_block_or_cancel_stores_null_not_an_empty_string() {
    // t1314: `task block <id>` with no --reason is the common call; Python stores NULL.
    let (flock, tasks) = flock_with_store("null-reason").await;
    let blocked = tasks.add("global", "b", "").await.unwrap();
    let got = flock
        .task_block(&blocked.id, None, Some(WORKER))
        .await
        .unwrap();
    assert_eq!("blocked", got.status);
    assert_eq!(None, got.block_reason, "NULL, not Some(\"\")");
    let cancelled = tasks.add("global", "c", "").await.unwrap();
    let got = flock
        .task_cancel(&cancelled.id, None, Some(WORKER))
        .await
        .unwrap()
        .task;
    assert_eq!("cancelled", got.status);
    assert_eq!(None, got.block_reason);
    // A given reason is still kept verbatim.
    let kept = tasks.add("global", "k", "").await.unwrap();
    let got = flock
        .task_block(&kept.id, Some("why"), Some(WORKER))
        .await
        .unwrap();
    assert_eq!(Some("why".to_string()), got.block_reason);
}

#[tokio::test]
async fn an_explicit_empty_note_event_is_kept_and_only_an_omitted_one_defaults() {
    let (flock, tasks) = flock_with_store("note-event").await;
    let task = tasks.add("global", "n", "").await.unwrap();
    flock.task_note(&task.id, None, None, None).await.unwrap();
    flock
        .task_note(&task.id, Some(""), None, None)
        .await
        .unwrap();
    let events = tasks.events_named(&task.id).await;
    assert_eq!(
        vec!["created".to_string(), "note".to_string(), "".to_string()],
        events
    );
}

// -- task_add ------------------------------------------------------------------------

#[tokio::test]
async fn task_add_defaults_to_global_low_open_and_unowned() {
    let (flock, _tasks) = flock_with_store("add-default").await;
    let t = flock
        .task_add(
            "title",
            None,
            None,
            TaskAddArgs::default(),
            "owner",
            Some("w1:p1"),
        )
        .await
        .unwrap();
    assert_eq!(
        ("global", "low", "open"),
        (t.scope.as_str(), t.priority.as_str(), t.status.as_str())
    );
    assert_eq!(
        "[]", t.assignees,
        "unassigned work is claimable, not handed to the creator"
    );
}

#[tokio::test]
async fn task_add_resolves_self_and_refuses_lane_roles_assigning_others() {
    let (flock, _tasks) = flock_with_store("add-owners").await;
    let mine = TaskAddArgs {
        assignees: &["self"],
        ..Default::default()
    };
    let t = flock
        .task_add("a", None, None, mine, "worker", Some("w1:p1"))
        .await
        .unwrap();
    assert_eq!(r#"["w1:p1"]"#, t.assignees);
    let other = TaskAddArgs {
        assignees: &["w2:p2"],
        ..Default::default()
    };
    assert!(flock
        .task_add("b", None, None, other, "worker", Some("w1:p1"))
        .await
        .is_err());
    assert!(flock
        .task_add("b", None, None, other, "owner", Some("w1:p1"))
        .await
        .is_ok());
}

#[tokio::test]
async fn task_add_refuses_anonymous_creation_in_a_private_queue_and_blank_titles() {
    let (flock, _tasks) = flock_with_store("add-refuse").await;
    let args = TaskAddArgs::default();
    let err = flock
        .task_add("x", Some("w3:p3"), None, args, "owner", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("private queue"), "{err}");
    assert!(flock
        .task_add("  ", None, None, args, "owner", None)
        .await
        .is_err());
    let review = TaskAddArgs {
        kind: Some("review"),
        ..Default::default()
    };
    assert!(flock
        .task_add("r", None, None, review, "worker", Some("w1:p1"))
        .await
        .is_err());
    let blocked = TaskAddArgs {
        status: Some("blocked"),
        ..Default::default()
    };
    assert!(flock
        .task_add("s", None, None, blocked, "worker", Some("w1:p1"))
        .await
        .is_err());
}

// -- task_read / task_update ---------------------------------------------------------

#[tokio::test]
async fn task_read_refuses_another_panes_private_task() {
    let (flock, tasks) = flock_with_store("read").await;
    let private = tasks.add("w3:p3", "mine", "").await.unwrap();
    assert!(flock.task_read(&private.id, Some("w3:p3")).await.is_ok());
    assert!(flock.task_read(&private.id, None).await.is_ok());
    let err = flock
        .task_read(&private.id, Some("w4:p4"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("private queue"), "{err}");
}

#[tokio::test]
async fn task_update_edits_fields_resolves_self_and_moves_status() {
    let (flock, tasks) = flock_with_store("update").await;
    let t = tasks.add("global", "t", "").await.unwrap();
    let args = TaskUpdateArgs {
        title: Some("new"),
        priority: Some("high"),
        assignees: Some(&["me"]),
        status: Some("blocked"),
        block_reason: Some(Some("waiting")),
        ..Default::default()
    };
    let got = flock.task_update(&t.id, args, Some("w1:p1")).await.unwrap();
    assert_eq!(
        ("new", "high", "blocked"),
        (
            got.title.as_str(),
            got.priority.as_str(),
            got.status.as_str()
        )
    );
    assert_eq!(r#"["w1:p1"]"#, got.assignees);
    assert_eq!(Some("waiting"), got.block_reason.as_deref());
    let private = tasks.add("w3:p3", "p", "").await.unwrap();
    assert!(flock
        .task_update(&private.id, TaskUpdateArgs::default(), Some("w4:p4"))
        .await
        .is_err());
}

#[tokio::test]
async fn task_update_keeps_field_edits_atomic_when_status_is_invalid() {
    let (flock, tasks) = flock_with_store("update-atomic-refusal").await;
    let task = tasks.add("global", "before", "").await.unwrap();
    let error = flock
        .task_update(
            &task.id,
            TaskUpdateArgs {
                title: Some("after"),
                priority: Some("urgent"),
                status: Some("invalid-status"),
                ..Default::default()
            },
            Some(WORKER),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("status must be one of"), "{error}");

    let unchanged = tasks.get(&task.id).await.unwrap();
    assert_eq!("before", unchanged.title);
    assert_eq!("low", unchanged.priority);
    assert_eq!("open", unchanged.status);
}

async fn depends_on(tasks: &Tasks, task: &str, deps: &[&str]) {
    tasks
        .update(
            task,
            crate::tasks::FieldEdit {
                depends_on: Some(deps.iter().map(|d| d.to_string()).collect()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn cancel_names_the_dependents_it_strands() {
    // tests/test_dunk_tasks.py test_cancel_names_the_dependents_it_strands (t878).
    let (flock, tasks) = flock_with_store("strands").await;
    let dep = tasks.add("global", "the dependency", "").await.unwrap();
    let row = tasks.add("global", "waits on it", "").await.unwrap();
    depends_on(&tasks, &row.id, &[&dep.id]).await;
    let cancelled = flock
        .task_cancel(&dep.id, Some("duplicate"), Some(WORKER))
        .await
        .unwrap();
    assert_eq!(vec![row.id.clone()], cancelled.blocked_dependents);
    assert_eq!("cancelled", cancelled.task.status);
}

#[tokio::test]
async fn only_live_dependents_are_named_and_the_ids_are_sorted() {
    let (flock, tasks) = flock_with_store("strands-live").await;
    let dep = tasks.add("global", "dep", "").await.unwrap();
    let blocked = tasks.add("global", "blocked", "").await.unwrap();
    let open = tasks.add("global", "open", "").await.unwrap();
    let done = tasks.add("global", "done", "").await.unwrap();
    let unrelated = tasks.add("global", "unrelated", "").await.unwrap();
    for t in [&blocked, &open, &done] {
        depends_on(&tasks, &t.id, &[&dep.id]).await;
    }
    tasks.block(&blocked.id, "x").await.unwrap();
    tasks.complete(&done.id).await.unwrap();
    let _ = unrelated;
    let cancelled = flock
        .task_cancel(&dep.id, None, Some(WORKER))
        .await
        .unwrap();
    let mut want = vec![blocked.id.clone(), open.id.clone()];
    want.sort();
    assert_eq!(
        want, cancelled.blocked_dependents,
        "done dependents are not stranded"
    );
    let none = tasks.add("global", "alone", "").await.unwrap();
    assert!(flock
        .task_cancel(&none.id, None, None)
        .await
        .unwrap()
        .blocked_dependents
        .is_empty());
}

#[test]
fn a_no_pane_caller_with_a_private_scope_and_self_gets_the_t278_refusal_first() {
    // Review t1365 finding 1: Python runs t278 BEFORE `_task_assignees`, so this caller is told
    // about the private queue, not "self-assign requires a known caller pane".
    for assignees in [&["self"][..], &["me", "w2:p2"][..], &[][..]] {
        let error = refused(Flock::check_task_add(
            "w2:p2",
            assignees,
            "anonymous",
            None,
            None,
            None,
        ));
        assert!(error.contains("--scope global"), "{assignees:?}: {error}");
        assert!(!error.contains("self-assign"), "{assignees:?}: {error}");
    }
    // On the global queue the same caller reaches the assignee expansion and is told that.
    let error = refused(Flock::check_task_add(
        "global",
        &["self"],
        "anonymous",
        None,
        None,
        None,
    ));
    assert!(
        error.contains("self-assign requires a known caller pane"),
        "{error}"
    );
}

#[test]
fn check_task_add_returns_the_resolved_owners_for_the_store_write() {
    let resolved = Flock::check_task_add(
        "global",
        &["self", " ", "w9:p9"],
        "owner",
        None,
        None,
        Some(OWNER),
    )
    .unwrap();
    assert_eq!(owners(&[OWNER, OTHER]), resolved);
    assert!(
        Flock::check_task_add("global", &[], "worker", None, None, Some(WORKER))
            .unwrap()
            .is_empty()
    );
    // A lane role may self-assign through `self`, which resolves to its own pane.
    assert_eq!(
        owners(&[WORKER]),
        Flock::check_task_add("global", &["me"], "worker", None, None, Some(WORKER)).unwrap()
    );
}

#[tokio::test]
async fn task_add_stores_kind_author_repo_pr_and_head_for_a_review() {
    let (flock, tasks) = flock_with_store("add-review").await;
    let args = TaskAddArgs {
        kind: Some("review"),
        author: Some("w7:p7"),
        repo: Some("o/r"),
        pr: Some("5"),
        head: Some("abcdef1234567890"),
        ..Default::default()
    };
    let t = flock
        .task_add("review it", None, None, args, "system", None)
        .await
        .unwrap();
    assert_eq!("review", t.kind);
    let again = flock
        .task_add("again", None, None, args, "system", None)
        .await;
    let err = again.unwrap_err().to_string();
    assert!(
        err.contains("a live review task for o/r#5@abcdef123456 already exists"),
        "{err}"
    );
    assert!(err.contains(&format!("({})", t.id)), "{err}");
    let bare = TaskAddArgs {
        kind: Some("review"),
        ..Default::default()
    };
    let err = flock
        .task_add("x", None, None, bare, "system", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("needs repo, pr and head"), "{err}");
    let bad = TaskAddArgs {
        pr: Some("five"),
        ..Default::default()
    };
    let err = flock
        .task_add("x", None, None, bad, "owner", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("pr must be a number; got 'five'"), "{err}");
    let (_, listed) = flock
        .task_list(None, None, None, true, 0, None, Some("review"), false)
        .await
        .unwrap();
    assert_eq!(1, listed.len());
    drop(tasks);
}

#[tokio::test]
async fn task_add_deduplicates_assignees_and_task_list_filters_needs_owner() {
    let (flock, _tasks) = flock_with_store("add-dedupe").await;
    let args = TaskAddArgs {
        assignees: &["w1:p1", "w1:p1", " "],
        ..Default::default()
    };
    let t = flock
        .task_add("d", None, None, args, "owner", None)
        .await
        .unwrap();
    assert_eq!(r#"["w1:p1"]"#, t.assignees);
    let q = flock
        .task_add("q", None, None, TaskAddArgs::default(), "owner", None)
        .await
        .unwrap();
    flock
        .task_block(&q.id, Some("needs-owner: which port?"), None)
        .await
        .unwrap();
    let w = flock
        .task_add("w", None, None, TaskAddArgs::default(), "owner", None)
        .await
        .unwrap();
    flock
        .task_block(&w.id, Some("waiting on PR"), None)
        .await
        .unwrap();
    let (_, asked) = flock
        .task_list(None, None, None, true, 0, None, None, true)
        .await
        .unwrap();
    assert_eq!(
        vec![q.id],
        asked.into_iter().map(|d| d.task.id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn task_reap_requeues_a_dead_caller_and_returns_the_python_shape() {
    let (flock, tasks) = flock_with_store("task-reap").await;
    let task = tasks.add("global", "leased", "").await.unwrap();
    tasks
        .claim_next(
            "global",
            WORKER,
            ClaimOptions {
                caller: Some((999_999_999, Some("missing".into()), Some("test".into()))),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = flock.task_reap(None, None).await.unwrap();
    assert_eq!(
        serde_json::json!({"requeued": [{
            "task_id": task.id,
            "pane_id": WORKER,
            "reason": "caller_dead"
        }]}),
        result
    );
    assert_eq!("open", tasks.get(&task.id).await.unwrap().status);
}

#[tokio::test]
async fn task_reap_refuses_fixture_pane_lists_before_reaping() {
    let (flock, tasks) = flock_with_store("task-reap-fixture").await;
    let task = tasks.add("global", "leased", "").await.unwrap();
    tasks
        .claim_next("global", WORKER, ClaimOptions::default())
        .await
        .unwrap();

    let error = flock
        .task_reap(Some(vec![]), Some("fixture"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("alive_source='live'"), "{error}");
    assert_eq!("in_progress", tasks.get(&task.id).await.unwrap().status);
}

#[tokio::test]
async fn task_metrics_privacy_and_scope() {
    let (flock, tasks) = flock_with_store("task-metrics-scope").await;
    tasks.add("global", "global", "").await.unwrap();
    tasks.add(WORKER, "private", "").await.unwrap();

    let visible = flock
        .task_metrics(None, None, None, None, None, Some(WORKER))
        .await
        .unwrap();
    assert_eq!(serde_json::json!("visible"), visible["scope"]);
    assert_eq!(2, visible["current"]["total"]);

    let global = flock
        .task_metrics(None, None, None, Some("global"), None, Some(WORKER))
        .await
        .unwrap();
    assert_eq!(1, global["current"]["total"]);
    assert!(flock
        .task_metrics(None, None, None, Some(OTHER), None, Some(WORKER))
        .await
        .is_err());

    let all = flock
        .task_metrics(None, None, None, Some("all"), None, None)
        .await
        .unwrap();
    assert_eq!(serde_json::json!("all"), all["scope"]);
    assert_eq!(2, all["current"]["total"]);
}
