//! t1270: store-level claim, lease and dependency tests ported from tests/test_dunk_tasks.py by name.
//!
//! PYTHON SHADOWING (reviewer finding, t3549): tests/test_dunk_tasks.py defines
//! `class CancelledDependencyTests` three times (lines 94, 154 and 1444). A later class statement
//! shadows the earlier ones, so only the class at line 1444 ever runs. The class at 94 (the t879
//! version) and the class at 154 (the pre-t879 version, where a cancelled edge blocks) are dead.
//!
//! Names whose ONLY Python original is dead (ported from dead code, so no live Python run backs them):
//!   - test_a_cancelled_dependency_is_named_and_the_row_stays_claimable (line 117)
//!   - test_cancel_names_the_dependents_it_strands (lines 127 and 185)
//!   - test_a_done_dependency_is_not_dead (lines 146 and 203)
//!   - test_repointing_the_edge_frees_the_row_without_cancelling_it (line 190)
//! Live Python originals: test_a_cancelled_dependency_is_satisfied, test_a_done_dependency_is_satisfied,
//! test_an_open_dependency_still_blocks (the class at line 1444), and the four lease tests
//! test_completing_releases_the_lease, test_cancelling_releases_the_lease, test_blocking_releases_the_lease
//! and test_another_panes_lease_is_untouched (TerminalStatusReleasesTheLease).
//!
//! ELIGIBILITY: Python asserted `eligible` on the task view. The Rust store has no such field on
//! DependencyState, so claimability is asserted through `claim_next`. That restores the check in
//! test_a_cancelled_dependency_is_satisfied, test_repointing_the_edge_frees_the_row_without_cancelling_it
//! and test_an_open_dependency_still_blocks. test_a_done_dependency_is_not_dead asserts only
//! dead_dependencies (its Python original is dead, and it asserted `eligible`). The repoint test does
//! not assert the Python pre-repoint `not eligible`: that belongs to the dead pre-t879 copy, and the
//! owner ruling makes a cancelled edge satisfied.
//!
//! DROPPED: test_a_cancelled_dependency_is_named_and_blocks_the_row. Its Python original is dead and
//! its name contradicted its assertions; the port duplicated the stays-claimable test. Dropped, not
//! relabelled.
//!
//! Flock-level tests (task_next, task_release, task_followers, reap advisories) are not here: they
//! need the Flock and roles layer (T31+). The full not-ported list is in the PR body.

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
/// Guard (reviewer R1, t3549): exactly one live lease must exist before the caller's terminal
/// action, so a lease test cannot pass vacuously when claim_next stops creating leases.
async fn claimed(store: &Tasks, pane: &str, title: &str) -> String {
    store.add("global", title, "").await.unwrap();
    let id = match store
        .claim_next("global", pane, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => task.id.clone(),
        other => panic!("expected a claim for {pane}, got {other:?}"),
    };
    assert_eq!(
        1,
        store.leases_for_pane(pane, None).await.unwrap().len(),
        "the fixture must hold exactly one live lease before the terminal action"
    );
    id
}

#[tokio::test]
async fn test_a_cancelled_dependency_is_named_and_the_row_stays_claimable() {
    // DEAD ORIGINAL (see the module doc). t879 made cancelled SATISFIED; t878 keeps the fact visible.
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
    // DEAD ORIGINAL (see the module doc).
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
    // DEAD ORIGINAL (see the module doc). Asserts dead_dependencies only; the Python also asserted eligible.
    let store = store("done-not-dead").await;
    let (dep, row) = pair(&store).await;
    store.complete(&dep.id).await.unwrap();
    let state = store.dependency_state(&row.id).await.unwrap();
    assert!(state.dead_dependencies.is_empty());
}

#[tokio::test]
async fn test_a_cancelled_dependency_is_satisfied() {
    // LIVE original: test_a_cancelled_dependency_is_satisfied (class at line 1444).
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
    // The Python asserted `eligible`: the row must be claimable.
    match store
        .claim_next("global", PANE, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(after.id, task.id),
        other => panic!("the dependent must be claimable, got {other:?}"),
    }
}

#[tokio::test]
async fn test_a_done_dependency_is_satisfied() {
    // LIVE original: test_a_done_dependency_is_satisfied (class at line 1444).
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
async fn test_an_open_dependency_still_blocks() {
    // LIVE original: test_an_open_dependency_still_blocks (class at line 1444): the Python asserts
    // blocked_by == [pre] and eligible False on the dependent. The prerequisite itself is
    // claimable, so the claim must return it and never the blocked dependent.
    let store = store("open-still-blocks").await;
    let pre = store.add("global", "prerequisite", "").await.unwrap();
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
    assert_eq!(vec![pre.id.clone()], state.blocked_by);
    match store
        .claim_next("global", PANE, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(pre.id, task.id),
        other => panic!("only the prerequisite may be claimed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_repointing_the_edge_frees_the_row_without_cancelling_it() {
    // DEAD ORIGINAL (see the module doc). The repair path: depends_on REPLACES the list.
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
    // The Python asserted `eligible` after the repoint: the row must be claimable again.
    match store
        .claim_next("global", PANE, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(row.id, task.id),
        other => panic!("a repointed row is claimable again, got {other:?}"),
    }
}

#[tokio::test]
async fn test_completing_releases_the_lease() {
    // LIVE original: TerminalStatusReleasesTheLease.
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
    // LIVE original: TerminalStatusReleasesTheLease.
    let store = store("cancel-releases").await;
    let task = claimed(&store, PANE, "claimed work").await;
    store.cancel(&task, "withdrawn").await.unwrap();
    assert!(store.leases_for_pane(PANE, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_blocking_releases_the_lease() {
    // LIVE original: TerminalStatusReleasesTheLease.
    let store = store("block-releases").await;
    let task = claimed(&store, PANE, "claimed work").await;
    store.block(&task, "needs-owner: a question").await.unwrap();
    assert!(store.leases_for_pane(PANE, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_another_panes_lease_is_untouched() {
    // LIVE original: TerminalStatusReleasesTheLease.
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
