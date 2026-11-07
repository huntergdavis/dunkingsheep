//! t1270: the store-level claim, lease and dependency tests of tests/test_dunk_tasks.py, ported
//! 1:1 by name. Each test keeps the Python name and asserts the same facts through the Rust
//! store. Flock-level tests (task_next, task_release, task_followers, reap advisories) are not
//! here: they need the Flock and roles layer (T31+). The list of what is not ported, and why, is
//! in the PR body.

use super::*;

const PANE: &str = "%1";

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-port-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

/// Python `_pair`: a dependency, and a row that waits on it.
async fn pair(store: &Tasks) -> (Task, Task) {
    let dep = store.add("global", "dependency", "").await.unwrap();
    let row = store
        .add_with(
            "global",
            "dependent",
            "",
            AddOptions {
                depends_on: vec![dep.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (dep, row)
}

/// A task claimed by `pane` through the store's own claim path; returns the claimed task's id.
async fn claimed(store: &Tasks, pane: &str, title: &str) -> String {
    store.add("global", title, "").await.unwrap();
    match store
        .claim_next("global", pane, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => task.id.clone(),
        other => panic!("expected a claim for {pane}, got {other:?}"),
    }
}

#[tokio::test]
async fn test_a_cancelled_dependency_is_named_and_the_row_stays_claimable() {
    // t879 made cancelled SATISFIED; t878 keeps the fact visible, not blocking.
    let store = store("cancelled-named").await;
    let (dep, row) = pair(&store).await;
    store.cancel(&dep.id, "duplicate").await.unwrap();
    let state = store.dependency_state(&row.id).await.unwrap();
    assert_eq!(vec![dep.id.clone()], state.dead_dependencies);
    assert!(
        !state.blocked_by.contains(&dep.id),
        "a cancelled dependency is satisfied (t164), not blocking"
    );
    match store
        .claim_next("global", PANE, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(row.id, task.id),
        other => panic!("the row must be claimable after the cancel, got {other:?}"),
    }
}

#[tokio::test]
async fn test_cancel_names_the_dependents_it_strands() {
    let store = store("names-dependents").await;
    let (dep, row) = pair(&store).await;
    store.cancel(&dep.id, "duplicate").await.unwrap();
    assert_eq!(
        vec![row.id.clone()],
        store.blocked_dependents(&dep.id).await.unwrap()
    );
}

#[tokio::test]
async fn test_a_done_dependency_is_not_dead() {
    let store = store("done-not-dead").await;
    let (dep, row) = pair(&store).await;
    store.complete(&dep.id).await.unwrap();
    let state = store.dependency_state(&row.id).await.unwrap();
    assert!(state.dead_dependencies.is_empty());
}

#[tokio::test]
async fn test_a_cancelled_dependency_is_satisfied() {
    let store = store("cancelled-satisfied").await;
    let pre = store.add("global", "prerequisite", "").await.unwrap();
    store.cancel(&pre.id, "superseded").await.unwrap();
    let after = store
        .add_with(
            "global",
            "dependent",
            "",
            AddOptions {
                depends_on: vec![pre.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let state = store.dependency_state(&after.id).await.unwrap();
    assert!(
        state.blocked_by.is_empty(),
        "a cancelled prerequisite is not outstanding work"
    );
}

#[tokio::test]
async fn test_a_done_dependency_is_satisfied() {
    let store = store("done-satisfied").await;
    let pre = store.add("global", "prerequisite", "").await.unwrap();
    store.complete(&pre.id).await.unwrap();
    let after = store
        .add_with(
            "global",
            "dependent",
            "",
            AddOptions {
                depends_on: vec![pre.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let state = store.dependency_state(&after.id).await.unwrap();
    assert!(state.blocked_by.is_empty());
}

#[tokio::test]
async fn test_repointing_the_edge_frees_the_row_without_cancelling_it() {
    // The repair path: depends_on REPLACES the list.
    let store = store("repoint").await;
    let (dep, row) = pair(&store).await;
    store.cancel(&dep.id, "duplicate").await.unwrap();
    assert_eq!(
        vec![dep.id.clone()],
        store
            .dependency_state(&row.id)
            .await
            .unwrap()
            .dead_dependencies
    );
    let good = store.add("global", "the canonical task", "").await.unwrap();
    store.complete(&good.id).await.unwrap();
    store
        .update(
            &row.id,
            FieldEdit {
                depends_on: Some(vec![good.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let repaired = store.get(&row.id).await.unwrap();
    assert!(
        repaired.depends_on.contains(good.id.as_str()),
        "the repointed row depends on the canonical task"
    );
    assert!(
        !repaired.depends_on.contains(dep.id.as_str()),
        "the list is replaced, not appended to"
    );
    assert!(store
        .dependency_state(&row.id)
        .await
        .unwrap()
        .dead_dependencies
        .is_empty());
}

#[tokio::test]
async fn test_completing_releases_the_lease() {
    let store = store("complete-releases").await;
    let task = claimed(&store, PANE, "claimed work").await;
    store.complete(&task).await.unwrap();
    assert!(
        store.leases_for_pane(PANE, None).await.unwrap().is_empty(),
        "a completed task still holds the lane's lease"
    );
    assert_eq!("done", store.get(&task).await.unwrap().status);
}

#[tokio::test]
async fn test_cancelling_releases_the_lease() {
    let store = store("cancel-releases").await;
    let task = claimed(&store, PANE, "claimed work").await;
    store.cancel(&task, "withdrawn").await.unwrap();
    assert!(store.leases_for_pane(PANE, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocking_releases_the_lease() {
    let store = store("block-releases").await;
    let task = claimed(&store, PANE, "claimed work").await;
    store.block(&task, "needs-owner: a question").await.unwrap();
    assert!(store.leases_for_pane(PANE, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_another_panes_lease_is_untouched() {
    let store = store("other-lease").await;
    let mine = claimed(&store, PANE, "mine").await;
    store.add("global", "theirs", "").await.unwrap();
    store
        .claim_next("global", "w2:p3", ClaimOptions::default())
        .await
        .unwrap();
    store.complete(&mine).await.unwrap();
    assert!(store.leases_for_pane(PANE, None).await.unwrap().is_empty());
    assert_eq!(
        1,
        store.leases_for_pane("w2:p3", None).await.unwrap().len(),
        "another lane's live claim was removed by my completion"
    );
}

/// The Python test of this name asserted that a cancelled dependency BLOCKS the row. Owner
/// ruling on t1270: a cancelled dependency counts as satisfied (the live rule, t530). The name
/// is kept for the 1:1 mapping; the assertions follow the ruling.
#[tokio::test]
async fn test_a_cancelled_dependency_is_named_and_blocks_the_row() {
    let store = store("cancelled-named-satisfied").await;
    let (dep, row) = pair(&store).await;
    store.cancel(&dep.id, "duplicate").await.unwrap();
    let state = store.dependency_state(&row.id).await.unwrap();
    assert_eq!(vec![dep.id.clone()], state.dead_dependencies);
    assert!(
        !state.blocked_by.contains(&dep.id),
        "owner ruling (t1270): a cancelled dependency is satisfied (t530), not blocking"
    );
    match store
        .claim_next("global", PANE, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(row.id, task.id),
        other => panic!("the row is claimable under the satisfied rule, got {other:?}"),
    }
}
