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

#[tokio::test]
async fn a_cancelled_earlier_member_still_holds_back_its_dependant() {
    let tasks = store("cancelled").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    tasks
        .set_status(&a.id, "cancelled", None, None)
        .await
        .unwrap();
    // Removal never satisfies an internal prerequisite: only its completion does.
    match tasks.complete(&b.id).await {
        Err(DunkError::Refused(message)) => assert!(message.contains(a.id.as_str())),
        other => panic!("b must stay held back by a: {other:?}"),
    }
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "in_progress");
}
