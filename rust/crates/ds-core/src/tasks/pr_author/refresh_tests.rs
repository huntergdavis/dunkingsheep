use super::tests::work;
use super::*;

async fn review(tasks: &Tasks, head: &str, author: Option<&str>) -> Task {
    tasks
        .add_with(
            "global",
            "late declaration review",
            "author lane: unknown (not attributable; say so in the verdict)\nPR body:\nAuthor provenance: quoted PR text\nMore PR text\nAuthor provenance: guarded author-task declaration; task none for this head; accumulated contributors: %older\n",
            AddOptions {
                kind: Some("review".into()),
                created_by: Some("review-sync".into()),
                author: author.map(str::to_owned),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some(head.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn late_declaration_refreshes_review_and_lifts_unknown_author_hold_once() {
    let dir = crate::testsupport::dir("late-review-declaration");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    let (task, caller) = work(&tasks, "%1").await;
    let head = "a".repeat(40);
    let row = review(&tasks, &head, None).await;
    let options = ClaimOptions {
        kinds: Some(vec!["review".into()]),
        exclude_author: Some("%2".into()),
        ..Default::default()
    };
    assert!(matches!(
        tasks
            .claim_next("global", "%2", options.clone())
            .await
            .unwrap(),
        ClaimOutcome::Refused { .. }
    ));
    for _ in 0..2 {
        tasks
            .declare_pr_author(&task, "o/r", 7, &head, "%1", &caller, crate::clock::now_s())
            .await
            .unwrap();
    }
    let refreshed = tasks.get(&row.id).await.unwrap();
    assert_eq!(
        tasks.review_facts(&row.id).await.unwrap().author.as_deref(),
        Some("%1")
    );
    assert!(refreshed.body.contains("author lane: %1"));
    assert!(refreshed.body.contains(&format!(
        "guarded author-task declaration; task {task}; accumulated contributors: %1"
    )));
    assert!(!refreshed.body.contains("task none for this head"));
    assert!(refreshed
        .body
        .contains("Author provenance: quoted PR text\nMore PR text"));
    assert_eq!(tasks.notes(&row.id).await.unwrap().len(), 1);
    assert!(matches!(
        tasks.claim_next("global", "%2", options).await.unwrap(),
        ClaimOutcome::Claimed { task, .. } if task.id == row.id
    ));
    assert!(tasks
        .set_verdict_with_author_guard(&row.id, "accept", Some("%1"), true)
        .await
        .is_err());
}

#[tokio::test]
async fn superseded_head_declaration_does_not_attach_to_current_or_cancelled_review() {
    let dir = crate::testsupport::dir("late-review-old-head");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    let (task, caller) = work(&tasks, "%1").await;
    let old = "a".repeat(40);
    let current = "b".repeat(40);
    let stale = review(&tasks, &old, None).await;
    tasks.cancel(&stale.id, "head superseded").await.unwrap();
    let row = review(&tasks, &current, None).await;
    let stale_before = tasks.get(&stale.id).await.unwrap();
    tasks
        .declare_pr_author(&task, "o/r", 7, &old, "%1", &caller, crate::clock::now_s())
        .await
        .unwrap();
    let after = tasks.get(&row.id).await.unwrap();
    assert_eq!(tasks.review_facts(&row.id).await.unwrap().author, None);
    assert_eq!(after.body, row.body);
    let stale_after = tasks.get(&stale.id).await.unwrap();
    assert_eq!(tasks.review_facts(&stale.id).await.unwrap().author, None);
    assert_eq!(stale_after.body, stale_before.body);
}

#[tokio::test]
async fn late_declaration_does_not_rewrite_known_or_judged_reviews() {
    for mode in ["known", "judged"] {
        let dir = crate::testsupport::dir(&format!("late-review-protected-{mode}"));
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let (task, caller) = work(&tasks, "%1").await;
        let head = "a".repeat(40);
        let author = (mode == "known").then_some("%other");
        let row = review(&tasks, &head, author).await;
        if mode == "judged" {
            tasks
                .set_verdict_with_author_guard(&row.id, "accept", Some("%2"), true)
                .await
                .unwrap();
        }
        tasks
            .declare_pr_author(&task, "o/r", 7, &head, "%1", &caller, crate::clock::now_s())
            .await
            .unwrap();
        assert_eq!(
            tasks.review_facts(&row.id).await.unwrap().author.as_deref(),
            author
        );
        assert_eq!(tasks.get(&row.id).await.unwrap().body, row.body);
        if mode == "judged" {
            assert_eq!(
                tasks.verdict_of(&row.id).await.unwrap().as_deref(),
                Some("accept")
            );
        }
    }
}
