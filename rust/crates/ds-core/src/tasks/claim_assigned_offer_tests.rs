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

/// The Python-oracle parity tests (t3617) accept exactly one divergence. A task whose ready offer
/// waits on its assignee is claimable by that assignee, so the Rust view says `claimable` and
/// `eligible` are true where the Python oracle says false. Where the Rust row differs from the oracle
/// in only that way, take the oracle's `claimable` and `eligible` and keep comparing every other field.
/// Any other difference still fails the caller's assertion.
pub(crate) fn accept_t3617_claims(actual: &mut Value, expected: &Value, viewer: Option<&str>) {
    let Some(viewer) = viewer else { return };
    match (actual, expected) {
        (Value::Array(rows), Value::Array(expected_rows)) if rows.len() == expected_rows.len() => {
            for (row, want) in rows.iter_mut().zip(expected_rows) {
                accept_t3617_claims(row, want, Some(viewer));
            }
        }
        (Value::Object(row), Value::Object(want)) => {
            let assigned = row
                .get("assignees")
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(viewer)));
            let waiting = row.get("allocation_waiting") == Some(&Value::Bool(true));
            let divergent = waiting
                && assigned
                && row.get("claimable") == Some(&Value::Bool(true))
                && want.get("claimable") == Some(&Value::Bool(false));
            if divergent {
                for key in ["claimable", "eligible"] {
                    if let Some(value) = want.get(key) {
                        row.insert(key.to_owned(), value.clone());
                    }
                }
            }
            for (key, value) in row.iter_mut() {
                if let Some(expected_value) = want.get(key) {
                    accept_t3617_claims(value, expected_value, Some(viewer));
                }
            }
        }
        _ => {}
    }
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

    // A review routed by its author is offered by the author, who must not take it back.
    let mut routed_by_assignee = waiting.clone();
    routed_by_assignee.offered_by = Some(REVIEWER.into());
    assert!(!claimable(&routed_by_assignee, Some(REVIEWER)));
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
