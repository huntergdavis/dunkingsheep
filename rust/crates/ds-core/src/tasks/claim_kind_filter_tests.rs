//! t3617 (owner root cause): the reviewer role's claim filter allows kind=review only, so a verify task
//! explicitly assigned to a reviewer pane was refused (not_claimable). An explicitly assigned task is claimable
//! by its assignee whatever its kind. Unassigned verify work stays refused for the reviewer role, and the
//! author exclusion holds.

use super::*;
use crate::tasks::AddOptions;

const REVIEWER: &str = "%243";

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-kind-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

/// The reviewer role's claim filter: kind review only, and never a task the pane authored.
fn reviewer_options() -> ClaimOptions {
    ClaimOptions {
        kinds: Some(vec!["review".to_string()]),
        exclude_author: Some(REVIEWER.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn an_assigned_verify_task_is_claimable_by_its_reviewer_assignee() {
    let tasks = store("assigned").await;
    let task = tasks
        .add_with(
            "global",
            "verify t3616",
            "concrete acceptance",
            AddOptions {
                assignees: vec![REVIEWER.to_string()],
                kind: Some("verify".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    match tasks
        .claim_next("global", REVIEWER, reviewer_options())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task: claimed, .. } => assert_eq!(claimed.id, task.id),
        other => panic!("expected the reviewer to claim its assigned verify task, got {other:?}"),
    }
}

#[tokio::test]
async fn an_unassigned_verify_task_stays_refused_for_the_reviewer_role() {
    let tasks = store("unassigned").await;
    tasks
        .add_with(
            "global",
            "verify unassigned",
            "concrete acceptance",
            AddOptions {
                kind: Some("verify".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    match tasks
        .claim_next("global", REVIEWER, reviewer_options())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => assert_eq!(refusal.reason, "not_claimable"),
        other => panic!("expected a refusal for unassigned verify work, got {other:?}"),
    }
}

#[tokio::test]
async fn an_assigned_task_the_reviewer_authored_stays_excluded() {
    let tasks = store("authored").await;
    tasks
        .add_with(
            "global",
            "verify self-authored",
            "concrete acceptance",
            AddOptions {
                assignees: vec![REVIEWER.to_string()],
                kind: Some("verify".to_string()),
                author: Some(REVIEWER.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    match tasks
        .claim_next("global", REVIEWER, reviewer_options())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => assert_eq!(refusal.reason, "not_claimable"),
        other => panic!("expected the author exclusion to refuse the claim, got {other:?}"),
    }
}
