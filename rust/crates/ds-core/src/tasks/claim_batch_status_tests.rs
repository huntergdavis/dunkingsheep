//! t3582, the review of #807: every status path honours the batch. The order guard and the
//! membership retirement now apply to the generic update, `block_by`, `delete` and `advance`'s
//! completion, not only to `set_status`. Also a cancelled OUTSIDE prerequisite must not refuse a
//! batch claim (the first owner ruling, t530).

use std::sync::Arc;

use rusqlite::params;
use serde_json::json;

use super::ClaimBatchOutcome;
use crate::error::DunkError;
use crate::tasks::advance::{AdvanceOptions, Plan, Planner};
use crate::tasks::{AddOptions, ClaimOptions, FieldEdit, Tasks};

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-batch-status-{tag}"));
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

/// The refusal names the earlier member that has to finish first.
fn assert_held_back_by(error: DunkError, earlier: &str) {
    match error {
        DunkError::Refused(message) => assert!(
            message.contains(earlier),
            "the refusal names the earlier member {earlier}: {message}"
        ),
        other => panic!("expected a refusal naming {earlier}, got {other:?}"),
    }
}

/// A planner that never claims, as in the advance tests.
fn skip_planner() -> Arc<Planner> {
    Arc::new(|_input| {
        let mut plan = Plan::new();
        plan.insert("action".into(), json!("skip"));
        plan.insert("reason".into(), json!("test"));
        plan
    })
}

#[tokio::test]
async fn the_generic_update_cannot_complete_a_member_before_its_earlier_member() {
    let tasks = store("update-order").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    let error = tasks
        .update_with_status(&b.id, FieldEdit::default(), Some("done"), Some("%1"))
        .await
        .unwrap_err();
    assert_held_back_by(error, &a.id);
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "in_progress");
}

#[tokio::test]
async fn the_generic_update_ends_only_the_member_it_finishes() {
    let tasks = store("update-retire").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks
        .update_with_status(&a.id, FieldEdit::default(), Some("done"), Some("%1"))
        .await
        .unwrap();
    assert_eq!(
        members_of(&tasks, batch_id).await,
        vec![(a.id.clone(), 0), (b.id.clone(), 1)],
        "the finished member leaves the batch through the update path"
    );
    // The batch goes on: b completes, now that no earlier member is active.
    tasks.complete(&b.id).await.unwrap();
}

#[tokio::test]
async fn block_by_ends_only_the_member_it_blocks() {
    let tasks = store("block-by").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let blocker = tasks.add("global", "outside blocker", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks.block_by(&b.id, &blocker.id).await.unwrap();
    assert_eq!(
        members_of(&tasks, batch_id).await,
        vec![(a.id.clone(), 1), (b.id.clone(), 0), (c.id.clone(), 1)],
        "only the blocked member leaves the batch"
    );
    tasks.complete(&a.id).await.unwrap();
    tasks.complete(&c.id).await.unwrap();
}

#[tokio::test]
async fn deleting_a_member_ends_its_membership() {
    let tasks = store("delete").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    let c = tasks.add("global", "c", "").await.unwrap();
    let batch_id = batch_of(
        &tasks
            .claim_batch(&ids(&[&a.id, &b.id, &c.id]), "%1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    tasks.delete(&b.id).await.unwrap();
    assert_eq!(
        members_of(&tasks, batch_id).await,
        vec![(a.id.clone(), 1), (b.id.clone(), 0), (c.id.clone(), 1)],
        "a deleted member does not stay active and hold later members back"
    );
    // a is still an earlier active member, so c waits for it; once a is done, c completes.
    assert_held_back_by(tasks.complete(&c.id).await.unwrap_err(), &a.id);
    tasks.complete(&a.id).await.unwrap();
    tasks.complete(&c.id).await.unwrap();
}

#[tokio::test]
async fn an_advance_completion_cannot_skip_an_earlier_member() {
    let tasks = store("advance").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks.assign(&a.id, &["%1"], "set").await.unwrap();
    tasks.assign(&b.id, &["%1"], "set").await.unwrap();
    tasks
        .claim_batch(&ids(&[&a.id, &b.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    let completion = |task: &str| AdvanceOptions {
        from_task: Some(task.into()),
        ..Default::default()
    };
    let error = tasks
        .advance_next_job("%1", "completion", completion(&b.id), skip_planner())
        .await
        .unwrap_err();
    assert_held_back_by(error, &a.id);
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "in_progress");
    tasks
        .advance_next_job("%1", "completion", completion(&a.id), skip_planner())
        .await
        .unwrap();
    tasks
        .advance_next_job("%1", "completion", completion(&b.id), skip_planner())
        .await
        .unwrap();
    assert_eq!(tasks.get(&b.id).await.unwrap().status, "done");
}

#[tokio::test]
async fn a_cancelled_outside_prerequisite_does_not_block_a_batch_claim() {
    let tasks = store("outside").await;
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
    tasks
        .set_status(&prereq.id, "cancelled", None, None)
        .await
        .unwrap();
    // A cancelled prerequisite is satisfied (t530): the batch claim goes through.
    let outcome = tasks
        .claim_batch(&ids(&[&member.id]), "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert!(batch_of(&outcome) > 0, "the batch was recorded");
    assert_eq!(tasks.get(&member.id).await.unwrap().status, "in_progress");
}
