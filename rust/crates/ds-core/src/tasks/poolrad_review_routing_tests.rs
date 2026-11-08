//! t3641: a review of huntergdavis/poolrad-decomp with no designated reviewer goes only to the default
//! Poolrad reviewer lanes (%243, %245). A designated reviewer is an assignee, and only that assignee may
//! take the task. Never %234 or %235. Coordinator decision i49337; owner answer i49380.
//!
//! Each case goes through `claim_next`, the path the push loop and the task queue both use.

use super::*;

const REVIEW_HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("poolrad-review-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
}

async fn add_review(store: &Tasks, repo: &str, assignees: Vec<String>) -> String {
    store
        .add_with(
            "global",
            "review a head",
            "review body",
            AddOptions {
                kind: Some("review".into()),
                repo: Some(repo.into()),
                pr: Some("1".into()),
                head: Some(REVIEW_HEAD.into()),
                assignees,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .id
}

async fn claim_as(store: &Tasks, pane: &str) -> Option<String> {
    match store
        .claim_next("global", pane, ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed { task, .. } => Some(task.id),
        ClaimOutcome::Refused { .. } => None,
    }
}

#[tokio::test]
async fn a_poolrad_review_with_no_designated_reviewer_goes_only_to_the_default_lanes() {
    let scratch = store("undesignated").await;
    let review = add_review(&scratch, "huntergdavis/poolrad-decomp", Vec::new()).await;
    for lane in ["%235", "%234", "%248"] {
        assert_eq!(
            None,
            claim_as(&scratch, lane).await,
            "{lane} must not take an undesignated Poolrad review"
        );
    }
    assert_eq!(
        Some(review),
        claim_as(&scratch, "%243").await,
        "the default Poolrad reviewer lane takes it"
    );
}

#[tokio::test]
async fn a_designated_poolrad_reviewer_is_the_only_pane_that_may_take_the_review() {
    let scratch = store("designated").await;
    let review = add_review(
        &scratch,
        "huntergdavis/poolrad-decomp",
        vec!["%248".to_string()],
    )
    .await;
    assert_eq!(
        None,
        claim_as(&scratch, "%243").await,
        "a default lane may not take a review designated to another pane"
    );
    assert_eq!(
        Some(review),
        claim_as(&scratch, "%248").await,
        "the designated reviewer takes it"
    );
}

#[tokio::test]
async fn a_review_of_another_repo_is_not_restricted_by_the_poolrad_rule() {
    let scratch = store("other-repo").await;
    let review = add_review(&scratch, "huntergdavis/dunkingsheep", Vec::new()).await;
    assert_eq!(
        Some(review),
        claim_as(&scratch, "%235").await,
        "the Poolrad rule covers poolrad-decomp only"
    );
}
