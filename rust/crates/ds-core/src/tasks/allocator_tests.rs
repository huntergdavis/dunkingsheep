//! Ported from `tests/test_dunk_allocator.py`: the registration and offer cases. The
//! allocation-pass, review and API cases arrive with their slices.

use serde_json::json;

use super::*;
use crate::tasks::FieldEdit;

async fn store(tag: &str) -> Tasks {
    let dir = std::env::temp_dir().join(format!("ds-alloc-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    let tasks = Tasks::open(&path).await.unwrap();
    tasks.allocation_init().await.unwrap();
    tasks
}

/// `self.task(...)`: a global task with a body, owned by `%leader`.
async fn task(tasks: &Tasks, title: &str) -> super::super::Task {
    let t = tasks
        .add("global", title, "concrete acceptance")
        .await
        .unwrap();
    let edit = FieldEdit {
        assignees: Some(vec!["%leader".into()]),
        ..Default::default()
    };
    tasks.update(&t.id, edit, None).await.unwrap()
}

fn msg(e: DunkError) -> String {
    e.to_string()
}

#[test]
fn tags_are_simple_names_sorted_and_unique() {
    assert_eq!(
        tags(&json!(["b", "a", "b", "x.y:z-1_2"]), false).unwrap(),
        ["a", "b", "x.y:z-1_2"]
    );
    for bad in [
        json!("code"),
        json!([1]),
        json!([""]),
        json!(["has space"]),
        json!(["a/b"]),
        json!(["x".repeat(81)]),
    ] {
        assert_eq!(
            msg(tags(&bad, false).unwrap_err()),
            "tags must be a list of nonempty simple names",
            "{bad}"
        );
    }
    assert_eq!(
        msg(tags(&json!([]), true).unwrap_err()),
        "at least one tag is required"
    );
    assert!(tags(&json!([]), false).unwrap().is_empty());
}

#[tokio::test]
async fn registration_validates_and_is_listed_in_status() {
    let t = store("register").await;
    let err = |r: Result<serde_json::Value, DunkError>| msg(r.unwrap_err());
    assert_eq!(
        err(t
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%leader",
                &json!("yes")
            )
            .await),
        "enabled must be boolean"
    );
    assert_eq!(
        err(t
            .allocation_register(
                "%1",
                &json!([]),
                &json!(["worker"]),
                &json!([]),
                "%leader",
                &json!(true)
            )
            .await),
        "at least one tag is required"
    );
    assert_eq!(
        err(t
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["boss"]),
                &json!([]),
                "%leader",
                &json!(true)
            )
            .await),
        "roles must be worker or reviewer"
    );
    assert_eq!(
        err(t
            .allocation_register(
                "",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%leader",
                &json!(true)
            )
            .await),
        "registration requires pane and operator"
    );
    assert_eq!(
        err(t
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "",
                &json!(true)
            )
            .await),
        "registration requires pane and operator"
    );
    let status = t
        .allocation_register(
            "%1",
            &json!(["code", "code"]),
            &json!(["worker"]),
            &json!(["c"]),
            "%leader",
            &json!(false),
        )
        .await
        .unwrap();
    let w = &status["workers"][0];
    assert_eq!(
        (
            w["pane_id"].as_str(),
            w["pools"].as_str(),
            w["roles"].as_str(),
            w["skills"].as_str()
        ),
        (
            Some("%1"),
            Some("[\"code\"]"),
            Some("[\"worker\"]"),
            Some("[\"c\"]")
        )
    );
    assert_eq!(
        (
            w["enabled"].as_i64(),
            w["registered_by"].as_str(),
            w["last_assigned"].as_f64()
        ),
        (Some(0), Some("%leader"), Some(0.0))
    );
    // re-registering updates in place
    let again = t
        .allocation_register(
            "%1",
            &json!(["code"]),
            &json!(["worker", "reviewer"]),
            &json!([]),
            "%op",
            &json!(true),
        )
        .await
        .unwrap();
    assert_eq!(again["workers"].as_array().unwrap().len(), 1);
    assert_eq!(
        (
            again["workers"][0]["enabled"].as_i64(),
            again["workers"][0]["registered_by"].as_str()
        ),
        (Some(1), Some("%op"))
    );
}

#[tokio::test]
async fn an_offer_needs_an_owned_exclusive_open_global_task_with_a_body() {
    let t = store("offer").await;
    let ok = task(&t, "real work").await;
    let offered = t
        .allocation_offer(&ok.id, "code", &json!(["c"]), "%leader")
        .await
        .unwrap();
    assert_eq!(offered.id, ok.id);
    let status = t.allocation_status().await.unwrap();
    let o = &status["offers"][0];
    assert_eq!(
        (
            o["task_id"].as_str(),
            o["pool"].as_str(),
            o["role"].as_str(),
            o["state"].as_str()
        ),
        (
            Some(ok.id.as_str()),
            Some("code"),
            Some("worker"),
            Some("ready")
        )
    );
    assert_eq!(
        (
            o["requires"].as_str(),
            o["owners"].as_str(),
            o["excluded"].as_str()
        ),
        (Some("[\"c\"]"), Some("[\"%leader\"]"), Some("[]"))
    );
    // a private task cannot be offered (test_disabled_registration_and_private_tasks_refused)
    let private = t.add("%1", "private", "body").await.unwrap();
    let edit = FieldEdit {
        assignees: Some(vec!["%1".into()]),
        ..Default::default()
    };
    t.update(&private.id, edit, None).await.unwrap();
    assert_eq!(
        msg(t
            .allocation_offer(&private.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "only open/blocked global backlog can be offered"
    );
    // no body / no owner / a lease / multi-assignee
    let bare = t.add("global", "no body", "  ").await.unwrap();
    assert_eq!(
        msg(t
            .allocation_offer(&bare.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "offer requires body, owner, exclusive task and no lease"
    );
    let unowned = t.add("global", "unowned", "body").await.unwrap();
    assert_eq!(
        msg(t
            .allocation_offer(&unowned.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "offer requires body, owner, exclusive task and no lease"
    );
    let leased = task(&t, "leased").await;
    t.claim_for_test(&leased.id, "%9").await;
    assert_eq!(
        msg(t
            .allocation_offer(&leased.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "offer requires body, owner, exclusive task and no lease"
    );
    // a bad pool or bad requires is refused before anything is written
    let fresh = task(&t, "fresh").await;
    assert_eq!(
        msg(t
            .allocation_offer(&fresh.id, "bad pool", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "tags must be a list of nonempty simple names"
    );
    assert_eq!(
        msg(t
            .allocation_offer(&fresh.id, "code", &json!("c"), "%leader")
            .await
            .unwrap_err()),
        "tags must be a list of nonempty simple names"
    );
    assert_eq!(
        t.allocation_status().await.unwrap()["offers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // an unknown task id is a refusal, not a panic
    assert!(t
        .allocation_offer("t999", "code", &json!([]), "%leader")
        .await
        .is_err());
}

#[tokio::test]
async fn a_task_with_a_live_delivery_intent_cannot_be_offered_and_a_canonical_review_cannot_be_reoffered(
) {
    let t = store("intent").await;
    let a = task(&t, "has intent").await;
    t.record_next_job_handoff(
        &a.id,
        "%9",
        None,
        Some("nj-1"),
        0,
        None,
        None,
        Some(serde_json::Map::from_iter([(
            "state".to_string(),
            json!("pending"),
        )])),
    )
    .await
    .unwrap();
    assert_eq!(
        msg(t
            .allocation_offer(&a.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "task already has a delivery intent"
    );
    // a CANCELLED intent no longer blocks
    let b = task(&t, "cancelled intent").await;
    t.record_next_job_handoff(
        &b.id,
        "%9",
        None,
        Some("nj-2"),
        0,
        None,
        None,
        Some(serde_json::Map::from_iter([(
            "state".to_string(),
            json!("cancelled"),
        )])),
    )
    .await
    .unwrap();
    t.allocation_offer(&b.id, "code", &json!([]), "%leader")
        .await
        .unwrap();
    // canonical review rows are written by `review` (a later slice): seed one directly
    let c = task(&t, "review task").await;
    t.writer
        .call({
            let id = c.id.clone();
            move |conn| {
                conn.execute(
                    "INSERT INTO allocation_reviews VALUES ('src', ?, ?, '%author')",
                    params![id, "a".repeat(40)],
                )
                .map_err(db::map_sqlite)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    assert_eq!(
        msg(t
            .allocation_offer(&c.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "canonical review cannot be reoffered as worker work"
    );
}

const REV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[tokio::test]
async fn a_review_is_one_canonical_task_per_source_and_resubmitting_the_same_revision_is_idempotent(
) {
    let t = store("review").await;
    let source = task(&t, "the work").await;
    let review = t
        .allocation_review(
            &source.id,
            REV_A,
            "artifact and acceptance",
            "code",
            &json!([]),
            "%leader",
        )
        .await
        .unwrap();
    assert_eq!(review.title, "Review: the work");
    assert_eq!(
        review.body,
        format!(
            "Frozen revision: {REV_A}\nSource task: {}\n\nartifact and acceptance",
            source.id
        )
    );
    assert_eq!(
        (review.scope.as_str(), review.status.as_str()),
        ("global", "open")
    );
    assert_eq!(review.assignees, "[\"%leader\"]");
    let again = t
        .allocation_review(
            &source.id,
            REV_A,
            "artifact and acceptance",
            "code",
            &json!([]),
            "%leader",
        )
        .await
        .unwrap();
    assert_eq!(review.id, again.id, "same revision: idempotent");
    // the offer carries the reviewer role and excludes the author
    let status = t.allocation_status().await.unwrap();
    let o = &status["offers"][0];
    assert_eq!(
        (
            o["task_id"].as_str(),
            o["role"].as_str(),
            o["offered_by"].as_str()
        ),
        (Some(review.id.as_str()), Some("reviewer"), Some("%leader"))
    );
    assert_eq!(o["excluded"], "[\"%leader\"]");
    // a new revision cannot replace one that has not finished (test_canonical_review_and_author_exclusion)
    assert_eq!(
        msg(t
            .allocation_review(&source.id, REV_B, "new", "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "prior review is not finished"
    );
    // the review task cannot be re-offered as worker work
    assert_eq!(
        msg(t
            .allocation_offer(&review.id, "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "canonical review cannot be reoffered as worker work"
    );
}

#[tokio::test]
async fn review_input_and_ownership_are_validated() {
    let t = store("review-validate").await;
    let source = task(&t, "the work").await;
    let refused = |r: Result<super::super::Task, DunkError>| msg(r.unwrap_err());
    for rev in ["abc", &"A".repeat(40), &"g".repeat(40), &"a".repeat(65)] {
        assert_eq!(
            refused(
                t.allocation_review(&source.id, rev, "body", "code", &json!([]), "%leader")
                    .await
            ),
            "review requires full revision hash and evidence/acceptance body",
            "{rev}"
        );
    }
    assert_eq!(
        refused(
            t.allocation_review(&source.id, REV_A, "  ", "code", &json!([]), "%leader")
                .await
        ),
        "review requires full revision hash and evidence/acceptance body"
    );
    assert_eq!(
        refused(
            t.allocation_review(&source.id, REV_A, "body", "code", &json!([]), "%stranger")
                .await
        ),
        "only source owner may submit global work for review"
    );
    let private = t.add("%1", "private", "body").await.unwrap();
    assert_eq!(
        refused(
            t.allocation_review(&private.id, REV_A, "body", "code", &json!([]), "%1")
                .await
        ),
        "only source owner may submit global work for review"
    );
    assert!(t
        .allocation_review("t999", REV_A, "body", "code", &json!([]), "%leader")
        .await
        .is_err());
    // a bad pool is refused and rolls the whole review back: no task, no review row
    let before = t.list("global").await.unwrap().len();
    assert!(t
        .allocation_review(&source.id, REV_A, "body", "bad pool", &json!([]), "%leader")
        .await
        .is_err());
    assert_eq!(
        t.list("global").await.unwrap().len(),
        before,
        "the failed review left no task behind"
    );
}

#[tokio::test]
async fn a_corrected_revision_reopens_the_same_task_and_the_former_coauthor_stays_excluded() {
    let t = store("review-revise").await;
    let source = task(&t, "the work").await;
    let coauthor = FieldEdit {
        assignees: Some(vec!["%leader".into(), "%coauthor".into()]),
        ..Default::default()
    };
    t.update(&source.id, coauthor, None).await.unwrap();
    let review = t
        .allocation_review(&source.id, REV_A, "first", "code", &json!([]), "%leader")
        .await
        .unwrap();
    let first_excluded = &t.allocation_status().await.unwrap()["offers"][0]["excluded"];
    assert_eq!(first_excluded, "[\"%coauthor\",\"%leader\"]");
    t.claim_for_test(&review.id, "%r").await;
    t.task_complete(&review.id, Some("%r"), true).await.unwrap();
    // the coauthor leaves the source; the exclusion must remain for the corrected review
    let solo = FieldEdit {
        assignees: Some(vec!["%leader".into()]),
        ..Default::default()
    };
    t.update(&source.id, solo, None).await.unwrap();
    let new = t
        .allocation_review(&source.id, REV_B, "second", "code", &json!([]), "%leader")
        .await
        .unwrap();
    assert_eq!(review.id, new.id, "the same review task is reused");
    assert_eq!(new.status, "open");
    assert!(new.body.contains(REV_B) && new.body.ends_with("second"));
    assert_eq!(
        t.leases(&new.id).await,
        0,
        "terminal leases do not delay a corrected revision"
    );
    let o = &t.allocation_status().await.unwrap()["offers"][0];
    // The exclusion is the SOURCE's assignees/creator/completer plus the prior exclusion: the
    // former coauthor stays, and %r - who completed the REVIEW, not the source - is not excluded.
    assert_eq!(
        o["excluded"], "[\"%coauthor\",\"%leader\"]",
        "the former coauthor stays excluded; the review's own completer does not"
    );
    // a different author cannot take over the canonical review
    assert_eq!(
        msg(t
            .allocation_review(
                &source.id,
                "c".repeat(40).as_str(),
                "x",
                "code",
                &json!([]),
                "%coauthor"
            )
            .await
            .unwrap_err()),
        "only source owner may submit global work for review"
    );
}
