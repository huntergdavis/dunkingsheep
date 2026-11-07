//! t3581 tests: the all-or-nothing batch claim. Competing claimers, an unavailable member, an
//! unmet outside prerequisite, and the refusals that share `claim_next`'s rules. A refused
//! batch must leave every member untouched: still open, unleased, and with no `claimed` event.

use super::ClaimBatchOutcome;
use crate::tasks::{AddOptions, ClaimOptions, Refusal, Tasks};

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-batch-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

fn ids(list: &[&String]) -> Vec<String> {
    list.iter().map(|id| id.to_string()).collect()
}

fn claimed_ids(outcome: &ClaimBatchOutcome) -> Vec<String> {
    match outcome {
        ClaimBatchOutcome::Claimed { tasks, .. } => tasks.iter().map(|t| t.id.clone()).collect(),
        other => panic!("expected a whole-batch claim, got {other:?}"),
    }
}

fn refused(outcome: ClaimBatchOutcome) -> Refusal {
    match outcome {
        ClaimBatchOutcome::Refused { refusal } => refusal,
        other => panic!("expected a refused batch, got {other:?}"),
    }
}

async fn claimed_events(tasks: &Tasks, id: &str) -> usize {
    tasks
        .events_named(id)
        .await
        .iter()
        .filter(|event| event.as_str() == "claimed")
        .count()
}

/// The member was not touched by a refused batch.
async fn assert_untouched(tasks: &Tasks, id: &str) {
    let task = tasks.get(id).await.unwrap();
    assert_eq!(task.status, "open", "{id} must stay open");
    assert!(
        tasks.leases_for(id, None).await.unwrap().is_empty(),
        "{id} must hold no lease"
    );
    assert_eq!(
        claimed_events(tasks, id).await,
        0,
        "{id} must have no claimed event"
    );
}

#[tokio::test]
async fn a_free_batch_is_claimed_whole_and_in_the_given_order() {
    let tasks = store("free").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch = ids(&[&c.id, &a.id, &b.id]);
    let outcome = tasks
        .claim_batch(&batch, "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(claimed_ids(&outcome), batch, "the order given is kept");
    for id in &batch {
        assert_eq!(tasks.get(id).await.unwrap().status, "in_progress");
        assert_eq!(tasks.leases_for(id, None).await.unwrap().len(), 1);
        assert_eq!(claimed_events(&tasks, id).await, 1);
    }
}

#[tokio::test]
async fn competing_claimers_on_one_batch_leave_exactly_one_whole_winner() {
    let first = store("race").await;
    let second = Tasks::open(first.path()).await.unwrap();
    let a = first.add("global", "a", "").await.unwrap();
    let b = first.add("global", "b", "").await.unwrap();
    let batch = ids(&[&a.id, &b.id]);
    let (x, y) = tokio::join!(
        first.claim_batch(&batch, "%1", ClaimOptions::default()),
        second.claim_batch(&batch, "%2", ClaimOptions::default()),
    );
    let outcomes = [x.unwrap(), y.unwrap()];
    let winners = outcomes
        .iter()
        .filter(|o| matches!(o, ClaimBatchOutcome::Claimed { .. }))
        .count();
    assert_eq!(
        winners, 1,
        "BEGIN IMMEDIATE: exactly one claimer wins the whole batch: {outcomes:?}"
    );
    // The winner holds BOTH members: never a split of the batch between two panes.
    for id in &batch {
        assert_eq!(
            first.leases_for(id, None).await.unwrap().len(),
            1,
            "{id} is held once"
        );
        assert_eq!(claimed_events(&first, id).await, 1);
    }
    let loser = outcomes
        .into_iter()
        .find(|o| matches!(o, ClaimBatchOutcome::Refused { .. }))
        .unwrap();
    // The loser runs after the winner's commit, so it sees the winner as the owner.
    assert_eq!(refused(loser).reason, "owned_elsewhere");
}

#[tokio::test]
async fn an_unavailable_member_leaves_zero_partial_claims_and_zero_events() {
    let tasks = store("partial").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks
        .add_with(
            "global",
            "b",
            "",
            AddOptions {
                assignees: vec!["%9".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let assignees_before = tasks.get(&b.id).await.unwrap().assignees;
    let batch = ids(&[&a.id, &b.id, &c.id]);
    let refusal = refused(
        tasks
            .claim_batch(&batch, "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "owned_elsewhere");
    assert_eq!(refusal.task_ids, vec![b.id.clone()]);
    assert_untouched(&tasks, &a.id).await;
    assert_untouched(&tasks, &b.id).await;
    assert_untouched(&tasks, &c.id).await;
    assert_eq!(
        tasks.get(&b.id).await.unwrap().assignees,
        assignees_before,
        "the owner's assignment is not rewritten by a refused claim"
    );
    assert!(
        tasks.leases_for_pane("%1", None).await.unwrap().is_empty(),
        "the claimer holds nothing after a refusal"
    );
}

#[tokio::test]
async fn a_member_leased_by_another_pane_refuses_the_whole_batch() {
    let tasks = store("leased").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks.claim_for_test(&b.id, "%9").await;
    let batch = ids(&[&a.id, &b.id]);
    let refusal = refused(
        tasks
            .claim_batch(&batch, "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "leased");
    assert_eq!(refusal.task_ids, vec![b.id.clone()]);
    assert_untouched(&tasks, &a.id).await;
}

#[tokio::test]
async fn an_unmet_prerequisite_outside_the_batch_refuses_the_whole_batch() {
    let tasks = store("external").await;
    let prereq = tasks.add("global", "prereq", "").await.unwrap();
    let member = tasks
        .add_with(
            "global",
            "member",
            "",
            AddOptions {
                depends_on: vec![prereq.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let free = tasks.add("global", "free", "").await.unwrap();
    let batch = ids(&[&member.id, &free.id]);
    let refusal = refused(
        tasks
            .claim_batch(&batch, "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "blocked");
    assert_eq!(refusal.task_ids, vec![member.id.clone()]);
    assert_untouched(&tasks, &free.id).await;
    assert_untouched(&tasks, &member.id).await;
}

#[tokio::test]
async fn a_prerequisite_inside_the_batch_does_not_hold_its_dependant() {
    let tasks = store("internal").await;
    let prereq = tasks.add("global", "prereq", "").await.unwrap();
    let member = tasks
        .add_with(
            "global",
            "member",
            "",
            AddOptions {
                depends_on: vec![prereq.id.clone()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let batch = ids(&[&prereq.id, &member.id]);
    let outcome = tasks
        .claim_batch(&batch, "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(
        claimed_ids(&outcome),
        batch,
        "a dependency on a batch member is satisfied by the same claim"
    );
}

#[tokio::test]
async fn an_empty_repeated_or_unknown_batch_is_refused_without_writes() {
    let tasks = store("shape").await;
    let a = tasks.add("global", "a", "").await.unwrap();

    let empty = refused(
        tasks
            .claim_batch(&[], "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(empty.reason, "empty");

    let repeated = refused(
        tasks
            .claim_batch(&ids(&[&a.id, &a.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(repeated.reason, "duplicate");
    assert_eq!(repeated.task_ids, vec![a.id.clone()]);
    assert_untouched(&tasks, &a.id).await;

    let unknown = refused(
        tasks
            .claim_batch(
                &[a.id.clone(), "t9999".to_string()],
                "%1",
                ClaimOptions::default(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(unknown.reason, "missing");
    assert_eq!(unknown.task_ids, vec!["t9999".to_string()]);
    assert_untouched(&tasks, &a.id).await;
}

#[tokio::test]
async fn a_busy_pane_is_refused_unless_allowed_like_claim_next() {
    let tasks = store("busy").await;
    let first = tasks.add("global", "first", "").await.unwrap();
    let second = tasks.add("global", "second", "").await.unwrap();
    // The pane takes one task on its own, so it now holds in-progress work.
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let batch = ids(&[&second.id]);
    let refusal = refused(
        tasks
            .claim_batch(&batch, "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "busy");
    assert_eq!(refusal.task_ids, vec![first.id.clone()]);
    assert_untouched(&tasks, &second.id).await;

    let allowed = tasks
        .claim_batch(
            &batch,
            "%1",
            ClaimOptions {
                allow_busy: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed_ids(&allowed), batch);
}

#[tokio::test]
async fn a_task_scoped_to_another_pane_is_refused_and_its_own_pane_takes_it() {
    let tasks = store("scope").await;
    let private = tasks.add("%7", "private", "").await.unwrap();
    let batch = ids(&[&private.id]);
    let refusal = refused(
        tasks
            .claim_batch(&batch, "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "scope");
    assert_eq!(refusal.task_ids, vec![private.id.clone()]);
    assert_untouched(&tasks, &private.id).await;

    let own = tasks
        .claim_batch(&batch, "%7", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(claimed_ids(&own), batch);
}

#[tokio::test]
async fn exclude_author_refuses_a_member_the_claimer_authored() {
    let tasks = store("author").await;
    let own = tasks
        .add_with(
            "global",
            "own work",
            "",
            AddOptions {
                author: Some("%1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let other = tasks.add("global", "other", "").await.unwrap();
    let batch = ids(&[&other.id, &own.id]);
    let refusal = refused(
        tasks
            .claim_batch(
                &batch,
                "%1",
                ClaimOptions {
                    exclude_author: Some("%1".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "not_claimable");
    assert_eq!(refusal.task_ids, vec![own.id.clone()]);
    assert_untouched(&tasks, &other.id).await;
}
