//! t3582 tests: batch membership, order and lifecycle. A claimed batch is recorded in the
//! claim's transaction; a second batch with an active member is refused; heartbeat, release and
//! expiry act on the whole batch; a member completes only after its earlier internal members.

use rusqlite::params;

use super::ClaimBatchOutcome;
use crate::error::DunkError;
use crate::tasks::{CallerAlive, ClaimOptions, Refusal, Tasks};

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-batch-life-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

fn ids(list: &[&String]) -> Vec<String> {
    list.iter().map(|id| id.to_string()).collect()
}

fn batch_of(outcome: &ClaimBatchOutcome) -> i64 {
    match outcome {
        ClaimBatchOutcome::Claimed { batch_id, .. } => *batch_id,
        other => panic!("expected a claimed batch, got {other:?}"),
    }
}

fn refused(outcome: ClaimBatchOutcome) -> Refusal {
    match outcome {
        ClaimBatchOutcome::Refused { refusal } => refusal,
        other => panic!("expected a refused batch, got {other:?}"),
    }
}

/// (task, active) for each recorded member of `batch_id`, in position order.
async fn members_of(tasks: &Tasks, batch_id: i64) -> Vec<(String, i64)> {
    tasks
        .writer
        .call(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, active FROM task_batch_members WHERE batch_id = ? ORDER BY position",
                )
                .unwrap();
            let rows = stmt
                .query_map(params![batch_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            Ok(rows)
        })
        .await
        .unwrap()
}

async fn all_inactive(tasks: &Tasks, batch_id: i64) -> bool {
    members_of(tasks, batch_id)
        .await
        .iter()
        .all(|(_, active)| *active == 0)
}

#[tokio::test]
async fn a_claimed_batch_records_its_members_in_order() {
    let tasks = store("record").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let outcome = tasks
        .claim_batch(&ids(&[&c.id, &a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    let batch_id = batch_of(&outcome);
    assert_eq!(
        members_of(&tasks, batch_id).await,
        vec![(c.id.clone(), 1), (a.id.clone(), 1), (b.id.clone(), 1)],
        "the members are recorded in the order given, all active"
    );
}

#[tokio::test]
async fn a_second_batch_containing_an_active_member_is_refused() {
    let tasks = store("conflict").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let x = tasks.add("global", "x", "").await.unwrap();
    tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&b.id, &x.id]), "%2", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "batch_conflict");
    assert_eq!(refusal.task_ids, vec![b.id.clone()]);
    assert_eq!(tasks.get(&x.id).await.unwrap().status, "open");
    assert!(tasks.leases_for(&x.id, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_heartbeat_on_one_member_extends_every_member() {
    let tasks = store("heartbeat").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .claim_batch(
            &ids(&[&a.id, &b.id]),
            "%1",
            ClaimOptions {
                lease_seconds: Some(60.0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let (expires, _) = tasks
        .heartbeat(&a.id, "%1", Some(3600.0), None)
        .await
        .unwrap();
    for id in [&a.id, &b.id] {
        let leases = tasks.leases_for(id, None).await.unwrap();
        assert_eq!(leases.len(), 1, "{id} keeps its lease");
        assert_eq!(
            leases[0].expires_at, expires,
            "{id} shares the batch deadline"
        );
    }
}

#[tokio::test]
async fn a_release_frees_every_member_and_ends_the_batch() {
    let tasks = store("release").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let first = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    let released = tasks.release(&a.id, "%1").await.unwrap();
    assert_eq!(released.status, "open");
    for id in [&a.id, &b.id] {
        assert_eq!(tasks.get(id).await.unwrap().status, "open", "{id} is freed");
        assert!(tasks.leases_for(id, None).await.unwrap().is_empty());
    }
    assert!(all_inactive(&tasks, first).await, "the batch has ended");
    // The freed members can be claimed again, by a new batch.
    let again = tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%2", ClaimOptions::default())
        .await
        .unwrap();
    assert_ne!(batch_of(&again), first);
}

#[tokio::test]
async fn an_expired_lease_reaps_every_member() {
    let tasks = store("expiry").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(
                &ids(&[&a.id, &b.id]),
                "%1",
                ClaimOptions {
                    lease_seconds: Some(0.5),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    let later = crate::clock::now_s() + 3600.0;
    let requeued = tasks.reap(None, None, Some(later), None).await.unwrap();
    let mut reaped: Vec<String> = requeued.iter().map(|r| r.task_id.clone()).collect();
    reaped.sort();
    let mut expected = vec![a.id.clone(), b.id.clone()];
    expected.sort();
    assert_eq!(
        reaped, expected,
        "one member's expiry requeues the whole batch"
    );
    assert!(requeued.iter().all(|r| r.reason == "expired"));
    for id in [&a.id, &b.id] {
        assert_eq!(tasks.get(id).await.unwrap().status, "open");
        assert!(tasks.leases_for(id, None).await.unwrap().is_empty());
    }
    assert!(all_inactive(&tasks, batch_id).await, "the batch has ended");
}

#[tokio::test]
async fn a_dead_caller_reaps_every_member() {
    let tasks = store("dead-caller").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .claim_batch(
            &ids(&[&a.id, &b.id]),
            "%1",
            ClaimOptions {
                caller: Some((4242, None, None)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let dead: CallerAlive = Box::new(|_: i64, _: Option<&str>| false);
    let requeued = tasks.reap(None, Some(dead), None, None).await.unwrap();
    assert_eq!(requeued.len(), 2, "both members are requeued");
    assert!(requeued.iter().all(|r| r.reason == "caller_dead"));
}

#[tokio::test]
async fn a_member_completes_only_after_its_earlier_members() {
    let tasks = store("order").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    match tasks.complete(&b.id).await {
        Err(DunkError::Refused(message)) => assert!(
            message.contains(a.id.as_str()),
            "the refusal names the earlier member: {message}"
        ),
        other => panic!("b must not complete before a: {other:?}"),
    }
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "in_progress");

    tasks.complete(&a.id).await.unwrap();
    tasks.complete(&b.id).await.unwrap();
    assert!(
        !all_inactive(&tasks, batch_id).await,
        "c is still open, so the batch is still active"
    );
    tasks.complete(&c.id).await.unwrap();
    assert!(
        all_inactive(&tasks, batch_id).await,
        "the last member's completion ends the batch"
    );
}

/// A cancelled prerequisite is satisfied (t530): it must not hold back the rest of the batch.
#[tokio::test]
async fn a_cancelled_earlier_member_does_not_hold_back_the_rest() {
    let tasks = store("cancelled").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks
        .set_status(&a.id, "cancelled", None, None)
        .await
        .unwrap();
    tasks.complete(&b.id).await.unwrap();
    tasks.complete(&c.id).await.unwrap();
    assert!(
        all_inactive(&tasks, batch_id).await,
        "the batch ends when its last active member leaves"
    );
}

/// Blocking one member ends only that member. The rest of the batch keeps its identity, its
/// leases and its order, and it can still complete.
#[tokio::test]
async fn a_blocked_member_ends_only_itself_and_the_batch_goes_on() {
    let tasks = store("blocked").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks.block(&b.id, "waiting on the owner").await.unwrap();
    assert_eq!(
        members_of(&tasks, batch_id).await,
        vec![(a.id.clone(), 1), (b.id.clone(), 0), (c.id.clone(), 1)],
        "only the blocked member leaves the batch"
    );
    assert!(tasks.leases_for(&b.id, None).await.unwrap().is_empty());
    assert_eq!(tasks.leases_for(&a.id, None).await.unwrap().len(), 1);
    assert_eq!(tasks.leases_for(&c.id, None).await.unwrap().len(), 1);
    // The remaining members are still one batch: a second batch cannot take c.
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&c.id]), "%2", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "batch_conflict");
    assert_eq!(refusal.task_ids, vec![c.id.clone()]);
    // b is no longer an earlier active member, so a and then c complete without it.
    tasks.complete(&a.id).await.unwrap();
    tasks.complete(&c.id).await.unwrap();
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "blocked");
    assert!(all_inactive(&tasks, batch_id).await);
}

/// The batch-identity caveat: members can leave one at a time, so a batch can lose a member and
/// keep the rest. The rest must still be freed together under release, never split.
#[tokio::test]
async fn a_batch_that_lost_a_member_is_still_freed_as_one_under_release() {
    let tasks = store("split-release").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks.block(&b.id, "waiting").await.unwrap();
    tasks.release(&a.id, "%1").await.unwrap();
    for id in [&a.id, &c.id] {
        assert_eq!(tasks.get(id).await.unwrap().status, "open", "{id} is freed");
        assert!(tasks.leases_for(id, None).await.unwrap().is_empty());
    }
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "blocked");
    assert!(all_inactive(&tasks, batch_id).await);
}

/// The same caveat under expiry: a member that left earlier is not requeued with the rest, and
/// the rest lapse together.
#[tokio::test]
async fn an_expiry_after_a_member_left_reaps_only_the_rest() {
    let tasks = store("split-expiry").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    tasks
        .claim_batch(
            &ids(&[&a.id, &b.id, &c.id]),
            "%1",
            ClaimOptions {
                lease_seconds: Some(0.5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tasks
        .set_status(&b.id, "cancelled", None, None)
        .await
        .unwrap();
    let later = crate::clock::now_s() + 3600.0;
    let requeued = tasks.reap(None, None, Some(later), None).await.unwrap();
    let mut reaped: Vec<String> = requeued.iter().map(|r| r.task_id.clone()).collect();
    reaped.sort();
    let mut expected = vec![a.id.clone(), c.id.clone()];
    expected.sort();
    assert_eq!(reaped, expected, "the cancelled member is not requeued");
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "cancelled");
}

/// Caveat 2: a dependency cycle among the members (a needs b, b needs a) is refused at claim, with
/// no write. In batch order a may depend only on earlier members, so the later edge is the cycle.
#[tokio::test]
async fn a_dependency_cycle_among_members_is_refused() {
    let tasks = store("cycle").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .update(
            &a.id,
            crate::tasks::FieldEdit {
                depends_on: Some(vec![b.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    tasks
        .update(
            &b.id,
            crate::tasks::FieldEdit {
                depends_on: Some(vec![a.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "order_conflict");
    assert_eq!(refusal.task_ids, vec![a.id.clone()], "a needs the later b");
    for id in [&a.id, &b.id] {
        assert_eq!(tasks.get(id).await.unwrap().status, "open");
        assert!(tasks.leases_for(id, None).await.unwrap().is_empty());
    }
}

/// A member that depends on a LATER member (not in a cycle, so no edge comes back) is refused:
/// the earlier member would wait for work that starts after it. The store already refuses a task
/// that depends on itself, so that case never reaches the batch.
#[tokio::test]
async fn a_dependency_on_a_later_member_is_refused() {
    let tasks = store("forward").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    tasks
        .update(
            &a.id,
            crate::tasks::FieldEdit {
                depends_on: Some(vec![c.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "order_conflict");
    assert_eq!(
        refusal.task_ids,
        vec![a.id.clone()],
        "a waits on the later c"
    );
    for id in [&a.id, &b.id, &c.id] {
        assert_eq!(tasks.get(id).await.unwrap().status, "open");
    }
}
