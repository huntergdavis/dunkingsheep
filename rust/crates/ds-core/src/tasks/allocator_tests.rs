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

// ---- allocate (slice 3): ports of the allocation-pass cases of test_dunk_allocator.py ----

use std::collections::BTreeMap;

async fn worker(t: &Tasks, pane: &str, role: &str, skills: &[&str]) {
    t.allocation_register(
        pane,
        &json!(["code"]),
        &json!([role]),
        &json!(skills),
        "%leader",
        &json!(true),
    )
    .await
    .unwrap();
}

async fn offer(t: &Tasks, task: &super::super::Task, requires: &[&str]) {
    t.allocation_offer(&task.id, "code", &json!(requires), "%leader")
        .await
        .unwrap();
}

fn idle(panes: &[&str]) -> BTreeMap<String, serde_json::Value> {
    panes.iter().map(|p| (p.to_string(), json!({}))).collect()
}

fn task_ids(handoffs: &[serde_json::Value]) -> Vec<String> {
    handoffs
        .iter()
        .map(|h| h["task_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn priority_age_and_id_order() {
    let t = store("order").await;
    let old = task(&t, "old").await;
    let high = task(&t, "high").await;
    t.update(
        &high.id,
        FieldEdit {
            priority: Some("high".into()),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    let later = task(&t, "later").await;
    for x in [&old, &high, &later] {
        offer(&t, x, &[]).await;
    }
    for p in ["%1", "%2", "%3"] {
        worker(&t, p, "worker", &[]).await;
    }
    let result = t
        .allocation_allocate(&idle(&["%1", "%2", "%3"]))
        .await
        .unwrap();
    assert_eq!(
        task_ids(&result),
        [high.id.clone(), old.id.clone(), later.id.clone()]
    );
    for h in &result {
        let got = t.get(h["task_id"].as_str().unwrap()).await.unwrap();
        assert_eq!(
            got.assignees,
            format!("[\"{}\"]", h["pane_id"].as_str().unwrap())
        );
        assert_eq!(got.status, "in_progress");
        let intents = t
            .next_job_handoffs(Some(h["pane_id"].as_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(intents[0]["message_id"], h["message_id"]);
        assert_eq!(
            (h["state"].as_str(), h["trigger"].as_str()),
            (Some("pending"), Some("allocation"))
        );
        assert!(
            h["message_id"].as_str().unwrap().starts_with("nj-")
                && h["message_id"].as_str().unwrap().len() == 35
        );
    }
}

#[tokio::test]
async fn no_capability_no_idle_no_assignment() {
    let t = store("capability").await;
    let x = task(&t, "real work").await;
    offer(&t, &x, &["c"]).await;
    worker(&t, "%wrong", "worker", &[]).await;
    worker(&t, "%right", "worker", &["c"]).await;
    assert!(t
        .allocation_allocate(&idle(&["%wrong"]))
        .await
        .unwrap()
        .is_empty());
    assert!(t.allocation_allocate(&idle(&[])).await.unwrap().is_empty());
    assert_eq!(t.get(&x.id).await.unwrap().assignees, "[\"%leader\"]");
    assert_eq!(
        t.allocation_allocate(&idle(&["%right"])).await.unwrap()[0]["pane_id"],
        "%right"
    );
}

#[tokio::test]
async fn manual_work_and_blocked_dependency_preserved() {
    let t = store("manual").await;
    let dependency = task(&t, "dep").await;
    let blocked = task(&t, "held").await;
    t.update(
        &blocked.id,
        FieldEdit {
            depends_on: Some(vec![dependency.id.clone()]),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    offer(&t, &blocked, &[]).await;
    worker(&t, "%worker", "worker", &[]).await;
    assert!(t
        .allocation_allocate(&idle(&["%worker"]))
        .await
        .unwrap()
        .is_empty());
    t.task_complete(&dependency.id, Some("%leader"), true)
        .await
        .unwrap();
    let manual = t.add("global", "manual", "reserved").await.unwrap();
    t.update(
        &manual.id,
        FieldEdit {
            assignees: Some(vec!["%worker".into()]),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    assert!(
        t.allocation_allocate(&idle(&["%worker"]))
            .await
            .unwrap()
            .is_empty(),
        "manual assigned work takes the capacity"
    );
    t.task_complete(&manual.id, Some("%worker"), true)
        .await
        .unwrap();
    assert_eq!(
        t.allocation_allocate(&idle(&["%worker"])).await.unwrap()[0]["task_id"],
        blocked.id.as_str()
    );
}

#[tokio::test]
async fn private_ready_queue_blocks_capacity_but_dependency_wait_does_not() {
    let t = store("private").await;
    let x = task(&t, "real work").await;
    offer(&t, &x, &[]).await;
    worker(&t, "%1", "worker", &[]).await;
    let private = t.add("%1", "private owned", "private").await.unwrap();
    t.update(
        &private.id,
        FieldEdit {
            assignees: Some(vec!["%1".into()]),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    assert!(t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .is_empty());
    let dependency = task(&t, "dependency").await;
    t.update(
        &private.id,
        FieldEdit {
            depends_on: Some(vec![dependency.id.clone()]),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        t.allocation_allocate(&idle(&["%1"])).await.unwrap()[0]["task_id"],
        x.id.as_str()
    );
}

#[tokio::test]
async fn reassignment_invalidates_the_offer() {
    let t = store("reassign").await;
    let x = task(&t, "real work").await;
    offer(&t, &x, &[]).await;
    worker(&t, "%1", "worker", &[]).await;
    t.assign(&x.id, &["%other"], "set").await.unwrap();
    assert!(t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_invalidated_offer_is_manual_work_and_blocks_a_second_offer() {
    let t = store("invalidated").await;
    let first = task(&t, "first").await;
    offer(&t, &first, &[]).await;
    worker(&t, "%1", "worker", &[]).await;
    t.assign(&first.id, &["%1"], "set").await.unwrap();
    let second = task(&t, "second").await;
    t.update(
        &second.id,
        FieldEdit {
            priority: Some("high".into()),
            ..Default::default()
        },
        None,
    )
    .await
    .unwrap();
    offer(&t, &second, &[]).await;
    assert!(
        !t.decorate(&first.id, None)
            .await
            .unwrap()
            .allocation_waiting
    );
    assert!(t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(t.get(&second.id).await.unwrap().status, "open");
}

#[tokio::test]
async fn a_disabled_worker_gets_nothing_and_a_private_task_cannot_be_offered() {
    let t = store("disabled").await;
    t.allocation_register(
        "%1",
        &json!(["code"]),
        &json!(["worker"]),
        &json!([]),
        "%leader",
        &json!(false),
    )
    .await
    .unwrap();
    let x = task(&t, "real work").await;
    offer(&t, &x, &[]).await;
    assert!(t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn atomic_rollback_retains_the_owner_when_the_intent_write_fails() {
    let t = store("rollback").await;
    let x = task(&t, "real work").await;
    offer(&t, &x, &[]).await;
    worker(&t, "%1", "worker", &[]).await;
    // The Python patches record_next_job_handoff to raise; here a trigger makes the intent
    // INSERT fail, which is the same point in the transaction.
    t.writer
        .call(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_intent BEFORE INSERT ON task_events WHEN NEW.event = 'next_job_handoff' BEGIN SELECT RAISE(ABORT, 'disk error'); END;",
            )
            .map_err(db::map_sqlite)
        })
        .await
        .unwrap();
    assert!(t.allocation_allocate(&idle(&["%1"])).await.is_err());
    assert_eq!(t.get(&x.id).await.unwrap().assignees, "[\"%leader\"]");
    assert_eq!(t.get(&x.id).await.unwrap().status, "open");
    assert_eq!(t.leases(&x.id).await, 0);
    assert_eq!(
        t.allocation_status().await.unwrap()["offers"][0]["state"],
        "ready"
    );
}

#[tokio::test]
async fn restart_and_duplicate_passes_keep_one_claim() {
    let dir = std::env::temp_dir().join(format!("ds-alloc-restart-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    let t = Tasks::open(&path).await.unwrap();
    t.allocation_init().await.unwrap();
    let x = task(&t, "real work").await;
    offer(&t, &x, &[]).await;
    worker(&t, "%1", "worker", &[]).await;
    let first = t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .remove(0);
    drop(t);
    let t = Tasks::open(&path).await.unwrap();
    t.allocation_init().await.unwrap();
    assert!(t
        .allocation_allocate(&idle(&["%1"]))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        t.next_job_handoffs(None).await.unwrap()[0]["message_id"],
        first["message_id"]
    );
    let claimed: i64 = t
        .writer
        .call(|c| {
            c.query_row(
                "SELECT count(*) FROM task_events WHERE event='claimed'",
                [],
                |r| r.get(0),
            )
            .map_err(db::map_sqlite)
        })
        .await
        .unwrap();
    assert_eq!(claimed, 1);
}

#[tokio::test]
async fn two_connections_cannot_double_allocate() {
    let dir = std::env::temp_dir().join(format!("ds-alloc-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    let seed = Tasks::open(&path).await.unwrap();
    seed.allocation_init().await.unwrap();
    let x = task(&seed, "real work").await;
    offer(&seed, &x, &[]).await;
    worker(&seed, "%1", "worker", &[]).await;
    worker(&seed, "%2", "worker", &[]).await;
    let (a, b) = (
        Tasks::open(&path).await.unwrap(),
        Tasks::open(&path).await.unwrap(),
    );
    let (idle_a, idle_b) = (idle(&["%1"]), idle(&["%2"]));
    let (ra, rb) = tokio::join!(
        a.allocation_allocate(&idle_a),
        b.allocation_allocate(&idle_b)
    );
    let total = ra.unwrap().len() + rb.unwrap().len();
    assert_eq!(total, 1, "exactly one allocator wins the offered task");
    assert_eq!(seed.leases(&x.id).await, 1);
}

#[tokio::test]
async fn canonical_review_and_author_exclusion_allocate_to_the_other_reviewer() {
    let t = store("review-alloc").await;
    let source = task(&t, "the work").await;
    worker(&t, "%leader", "reviewer", &[]).await;
    worker(&t, "%reviewer", "reviewer", &[]).await;
    t.allocation_review(
        &source.id,
        REV_A,
        "artifact and acceptance",
        "code",
        &json!([]),
        "%leader",
    )
    .await
    .unwrap();
    let handoff = t
        .allocation_allocate(&idle(&["%leader", "%reviewer"]))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        handoff["pane_id"], "%reviewer",
        "the author is excluded from reviewing their own work"
    );
    assert_eq!(
        msg(t
            .allocation_review(&source.id, REV_B, "new", "code", &json!([]), "%leader")
            .await
            .unwrap_err()),
        "prior review is not finished"
    );
}

#[tokio::test]
async fn the_former_coauthor_stays_excluded_from_a_corrected_review_when_allocating() {
    let t = store("coauthor-alloc").await;
    let source = task(&t, "the work").await;
    t.assign(&source.id, &["%coauthor"], "add").await.unwrap();
    worker(&t, "%r", "reviewer", &[]).await;
    worker(&t, "%coauthor", "reviewer", &[]).await;
    let review = t
        .allocation_review(&source.id, REV_A, "first", "code", &json!([]), "%leader")
        .await
        .unwrap();
    assert_eq!(
        t.allocation_allocate(&idle(&["%r"])).await.unwrap().len(),
        1
    );
    t.task_complete(&review.id, Some("%r"), true).await.unwrap();
    t.assign(&source.id, &["%leader"], "set").await.unwrap();
    t.allocation_review(&source.id, REV_B, "second", "code", &json!([]), "%leader")
        .await
        .unwrap();
    assert!(t
        .allocation_allocate(&idle(&["%coauthor"]))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        t.allocation_allocate(&idle(&["%r"])).await.unwrap()[0]["pane_id"],
        "%r"
    );
}

#[tokio::test]
async fn a_new_revision_reuses_the_review_task_and_it_can_be_allocated_again() {
    let t = store("revision-alloc").await;
    let source = task(&t, "the work").await;
    worker(&t, "%r", "reviewer", &[]).await;
    let review = t
        .allocation_review(&source.id, REV_A, "first", "code", &json!([]), "%leader")
        .await
        .unwrap();
    t.allocation_allocate(&idle(&["%r"])).await.unwrap();
    t.task_complete(&review.id, Some("%r"), true).await.unwrap();
    let new = t
        .allocation_review(&source.id, REV_B, "second", "code", &json!([]), "%leader")
        .await
        .unwrap();
    assert_eq!(review.id, new.id);
    assert_eq!(
        t.allocation_allocate(&idle(&["%r"])).await.unwrap().len(),
        1
    );
}
