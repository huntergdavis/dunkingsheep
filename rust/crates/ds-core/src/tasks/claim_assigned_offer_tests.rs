//! t3617: a task offered while assigned to a pane is claimable by that pane. Before this, a
//! reviewer pane was refused its own assigned task (`not_claimable`), because the offer whose owners
//! equal the assignees is "waiting" and the allocator skips it. Nothing could move it.

use serde_json::{json, Value};

use super::*;
use crate::tasks::FieldEdit;

const REVIEWER: &str = "%243";
const OTHER: &str = "%9";

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-offer-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    tasks.allocation_init().await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

/// A global task with a body, assigned to the reviewer, with a ready offer. The offer records the
/// assignees as its owners, so the task is in the waiting state.
async fn assigned_offer(store: &Tasks, title: &str) -> String {
    let task = store
        .add("global", title, "concrete acceptance")
        .await
        .unwrap();
    let edit = FieldEdit {
        assignees: Some(vec![REVIEWER.into()]),
        ..Default::default()
    };
    let task = store.update(&task.id, edit, None).await.unwrap();
    store
        .allocation_offer(&task.id, "code", &json!([]), "%leader")
        .await
        .unwrap();
    task.id
}

fn offer_state(status: &Value, task_id: &str) -> String {
    status["offers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|offer| offer["task_id"] == json!(task_id))
        .map(|offer| offer["state"].as_str().unwrap().to_owned())
        .expect("the offer exists")
}

#[test]
fn a_waiting_offer_admits_its_assignee_and_no_one_else() {
    let waiting = Candidate {
        id: "t1".into(),
        status: "open".into(),
        assignees: vec![REVIEWER.into()],
        allocation_waiting: true,
        ..Default::default()
    };
    assert!(claimable(&waiting, Some(REVIEWER)));
    assert!(!claimable(&waiting, Some(OTHER)));
    assert!(!claimable(&waiting, None));
}

#[tokio::test]
async fn the_assignee_claims_the_task_its_waiting_offer_names_and_the_offer_closes() {
    let store = store("assignee").await;
    let id = assigned_offer(&store, "verify the t3616 assessment").await;

    match store
        .claim_next("global", REVIEWER, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => assert_eq!(task.id, id),
        other => panic!("expected the assignee to claim, got {other:?}"),
    }
    let status = store.allocation_status().await.unwrap();
    assert_eq!("assigned", offer_state(&status, &id));
}

#[tokio::test]
async fn another_pane_cannot_take_a_task_whose_offer_is_waiting_on_its_assignee() {
    let store = store("other").await;
    let id = assigned_offer(&store, "verify the t3616 assessment").await;

    match store
        .claim_next("global", OTHER, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => assert_eq!(refusal.task_ids, vec![id.clone()]),
        other => panic!("expected a refusal for another pane, got {other:?}"),
    }
    let status = store.allocation_status().await.unwrap();
    assert_eq!(
        "ready",
        offer_state(&status, &id),
        "a refusal leaves the offer waiting"
    );
}
