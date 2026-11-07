//! t3582, the owner's ruling on #807: an expired batch releases its members at expiry, without
//! waiting for reap. A live batch still refuses a conflicting claim.

use std::time::Duration;

use rusqlite::params;

use super::ClaimBatchOutcome;
use crate::tasks::{ClaimOptions, Refusal, Tasks};

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-batch-expiry-{tag}"));
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

/// Whether any member of `batch_id` is still active.
async fn any_active(tasks: &Tasks, batch_id: i64) -> bool {
    tasks
        .writer
        .call(move |conn| {
            let active: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_batch_members WHERE batch_id = ? AND active = 1",
                    params![batch_id],
                    |row| row.get(0),
                )
                .unwrap();
            Ok(active > 0)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn an_expired_batch_releases_its_members_without_a_reap() {
    let tasks = store("released").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let old = batch_of(
        &tasks
            .claim_batch(
                &ids(&[&a.id, &b.id]),
                "%1",
                ClaimOptions {
                    lease_seconds: Some(0.001),
                    ..Default::default()
                },
            )
            .await
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(30));
    // No reap has run. The lapsed batch no longer conflicts: the refusal is about ownership now
    // (the claimer still owns the members, as reap leaves them), not about the batch.
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&a.id]), "%2", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(refusal.reason, "owned_elsewhere");
    assert!(!any_active(&tasks, old).await, "the lapsed batch has ended");
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "open");
    assert!(tasks.leases_for(&b.id, None).await.unwrap().is_empty());
    // The original claimer can take its members again, in a new batch.
    let again = tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_ne!(batch_of(&again), old);
}

#[tokio::test]
async fn a_live_batch_still_refuses_a_conflicting_claim() {
    let tasks = store("live").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    let refusal = refused(
        tasks
            .claim_batch(&ids(&[&a.id]), "%2", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(
        refusal.reason, "batch_conflict",
        "a live batch is not released"
    );
    assert_eq!(tasks.get(&a.id).await.unwrap().status, "in_progress");
}
