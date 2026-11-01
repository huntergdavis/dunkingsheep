//! Ports of the guard cases of tests/test_dunk_complete_ownership*.py and the review gates of
//! tests/test_dunk_github.py that need no backend: ownership, the binding's refusals before
//! the process walk, and the review-completion gate order.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use super::*;
use crate::flock::panes::Stores;
use crate::tasks::{AddOptions, Tasks};

struct Panes(Option<Value>);

impl PaneInfoEnv for Panes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        self.0.clone()
    }
}

async fn setup(tag: &str) -> (crate::testsupport::Scratch<Flock>, std::sync::Arc<Tasks>) {
    let scratch = crate::testsupport::dir(&format!("tc-{tag}"));
    let dir = scratch.path().to_path_buf();
    let tasks = std::sync::Arc::new(Tasks::open(dir.join("tasks.db")).await.unwrap());
    let flock = Flock::new(dir.join("l.json"), dir.join("n.json"), false);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (crate::testsupport::Scratch::new(flock, scratch), tasks)
}

fn text(result: Result<(), DunkError>) -> String {
    result.expect_err("expected a refusal").to_string()
}

#[tokio::test]
async fn ownership_decides_completion() {
    let (flock, tasks) = setup("own").await;
    let t = tasks
        .add_with(
            "global",
            "t",
            "",
            AddOptions {
                assignees: vec!["w1:p1".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let d = tasks.decorate(&t.id, None).await.unwrap();
    let env = Panes(None);
    assert!(flock
        .authorize_task_complete(&d, Some("w1:p1"), false, &env, None)
        .is_ok());
    assert!(flock
        .authorize_task_complete(&d, Some("w9:p9"), true, &env, None)
        .is_ok());
    let err = text(flock.authorize_task_complete(&d, Some("w9:p9"), false, &env, None));
    assert!(
        err.contains("is owned by w1:p1; w9:p9 may not complete it"),
        "{err}"
    );
    let err = text(flock.authorize_task_complete(&d, None, true, &env, None));
    assert!(
        err.contains("cannot be completed without a pane identity"),
        "{err}"
    );
}

#[tokio::test]
async fn an_unowned_task_names_no_one_recorded() {
    let (flock, tasks) = setup("unowned").await;
    let t = tasks.add("global", "t", "").await.unwrap();
    let d = tasks.decorate(&t.id, None).await.unwrap();
    let err = text(flock.authorize_task_complete(&d, Some("w1:p1"), false, &Panes(None), None));
    assert!(err.contains("is owned by no one recorded"), "{err}");
}

#[tokio::test]
async fn a_socket_request_is_never_trusted_on_its_labels() {
    let (flock, _tasks) = setup("bind").await;
    let caller = Caller::new(
        Some(std::process::id() as i64),
        Some("1".into()),
        None,
        None,
        "peer",
    );
    // credentials failed on the transport
    flock.set_socket_peer(Some(false));
    let err = text(flock.require_completion_binding("w1:p1", &Panes(None), Some(&caller)));
    assert!(err.contains("socket peer credentials unavailable"), "{err}");
    // credentials ok, but the pane is unknown / has no pid / the caller is not inside it
    flock.set_socket_peer(Some(true));
    let err = text(flock.require_completion_binding("w1:p1", &Panes(None), Some(&caller)));
    assert!(err.contains("is not visible on this backend"), "{err}");
    let nopid = Panes(Some(json!({"pane_id": "w1:p1"})));
    let err = text(flock.require_completion_binding("w1:p1", &nopid, Some(&caller)));
    assert!(err.contains("reports no process id"), "{err}");
    let far = Panes(Some(json!({"pane_pid": 4_000_000_000_i64})));
    let err = text(flock.require_completion_binding("w1:p1", &far, Some(&caller)));
    assert!(err.contains("caller binding failed"), "{err}");
    // no peer at all (in-process code, tests): the same shapes keep label policy
    flock.set_socket_peer(None);
    assert!(flock
        .require_completion_binding("w1:p1", &Panes(None), Some(&caller))
        .is_ok());
    assert!(flock
        .require_completion_binding("w1:p1", &nopid, Some(&caller))
        .is_ok());
    assert!(flock
        .require_completion_binding("w1:p1", &far, None)
        .is_ok());
    let err = text(flock.require_completion_binding("w1:p1", &far, Some(&caller)));
    assert!(err.contains("caller binding failed"), "{err}");
}

async fn review(tasks: &Tasks, status_done: bool) -> (Decorated, ReviewFacts) {
    let t = tasks
        .add_with(
            "global",
            "review",
            "",
            AddOptions {
                kind: Some("review".into()),
                author: Some("w7:p7".into()),
                repo: Some("o/r".into()),
                pr: Some("5".into()),
                head: Some("abc".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    if status_done {
        tasks.complete(&t.id).await.unwrap();
    }
    (
        tasks.decorate(&t.id, None).await.unwrap(),
        tasks.review_facts(&t.id).await.unwrap(),
    )
}

fn caller<'a>(
    pane: &'a str,
    role: &'a str,
    may: bool,
    worked: &'a BTreeSet<String>,
) -> ReviewCaller<'a> {
    ReviewCaller {
        pane: Some(pane),
        role,
        may_review: may,
        worked: Ok(worked),
    }
}

/// A reviewer whose authorship check could not be answered (`authors_of` raised).
fn caller_store_down<'a>(pane: &'a str, role: &'a str, error: &'a str) -> ReviewCaller<'a> {
    ReviewCaller {
        pane: Some(pane),
        role,
        may_review: true,
        worked: Err(error),
    }
}

#[tokio::test]
async fn the_review_gates_run_in_the_pythons_order() {
    let (_flock, tasks) = setup("gates").await;
    let (d, facts) = review(&tasks, false).await;
    let none = BTreeSet::new();
    let gate = |who: &ReviewCaller<'_>, verdict: Option<&str>| {
        Flock::check_task_completion_gates(&d, &facts, verdict, who).map_err(|e| e.to_string())
    };
    let err = gate(&caller("w2:p2", "reviewer", false, &none), Some("accept")).unwrap_err();
    assert!(
        err.contains("may not review (provider not on the reviewer list)"),
        "{err}"
    );
    let err = gate(&caller("w2:p2", "worker", true, &none), Some("accept")).unwrap_err();
    assert!(err.contains("not role worker"), "{err}");
    let err = gate(&caller("w7:p7", "reviewer", true, &none), Some("accept")).unwrap_err();
    assert!(
        err.contains("a lane never accepts its own work (no-self-merge)"),
        "{err}"
    );
    let worked: BTreeSet<String> = ["w3:p3".to_string()].into();
    let err = gate(&caller("w3:p3", "reviewer", true, &worked), Some("accept")).unwrap_err();
    assert!(err.contains("w3:p3 worked on o/r#5"), "{err}");
    let err = gate(&caller("w2:p2", "reviewer", true, &none), None).unwrap_err();
    assert!(err.contains("needs --verdict accept|needs-fix"), "{err}");
    assert!(gate(&caller("w2:p2", "reviewer", true, &none), Some("maybe")).is_err());
    assert_eq!(
        Some("needs-fix".to_string()),
        gate(&caller("w2:p2", "reviewer", true, &none), Some("needs-fix")).unwrap()
    );
    // the owner is exempt from the lane checks
    assert!(gate(&caller("w1:p1", "owner", false, &worked), Some("accept")).is_ok());
}

#[tokio::test]
async fn a_store_that_cannot_answer_fails_closed_for_a_reviewer_t723() {
    // review t1500: Python raises "cannot verify that <pane> is not the author of <repo>#<pr>: ..."
    // when `authors_of` raises, BEFORE any verdict is recorded; the gate must not default to "no
    // authors".
    let (_flock, tasks) = setup("failclosed").await;
    let (d, facts) = review(&tasks, false).await;
    let gate = |who: &ReviewCaller<'_>, verdict: Option<&str>| {
        Flock::check_task_completion_gates(&d, &facts, verdict, who).map_err(|e| e.to_string())
    };
    let down = caller_store_down("w2:p2", "reviewer", "database is locked");
    let expected = "cannot verify that w2:p2 is not the author of o/r#5: database is locked. \
                    no-self-review is checked before a verdict is recorded, so this review is \
                    refused until the task store answers.";
    assert_eq!(expected, gate(&down, Some("accept")).unwrap_err());
    // it refuses before the verdict is even looked at
    assert_eq!(expected, gate(&down, None).unwrap_err());
    assert_eq!(expected, gate(&down, Some("not-a-verdict")).unwrap_err());
    // the owner is exempt from the lane checks, so a store error does not stop an owner
    assert!(gate(
        &caller_store_down("w1:p1", "owner", "locked"),
        Some("accept")
    )
    .is_ok());
    // an earlier gate still wins: a reviewer that may not review is refused for THAT reason
    let mut not_allowed = caller_store_down("w2:p2", "reviewer", "locked");
    not_allowed.may_review = false;
    assert!(gate(&not_allowed, Some("accept"))
        .unwrap_err()
        .contains("may not review"));
    // and the task's own author check still precedes the store question
    let err = gate(
        &caller_store_down("w7:p7", "reviewer", "locked"),
        Some("accept"),
    )
    .unwrap_err();
    assert!(
        err.contains("a lane never accepts its own work (no-self-merge)"),
        "{err}"
    );
    assert!(!err.contains("cannot verify"), "{err}");
}

#[tokio::test]
async fn the_halves_compose_to_the_whole_and_evidence_slots_between_them() {
    // %142 / Python order: authorship and verdict, THEN `_require_review_evidence`, THEN cancelled
    // and done. The two halves let the real caller put the evidence check in between.
    let (_flock, tasks) = setup("halves").await;
    let (d, facts) = review(&tasks, false).await;
    let none = BTreeSet::new();
    let who = caller("w2:p2", "reviewer", true, &none);
    let first = Flock::check_review_caller_and_verdict(&d, &facts, Some("accept"), &who).unwrap();
    assert_eq!(Some("accept".to_string()), first);
    assert!(Flock::check_review_head_unjudged(&d, &facts).is_ok());
    assert_eq!(
        first,
        Flock::check_task_completion_gates(&d, &facts, Some("accept"), &who).unwrap()
    );
    // the first half never looks at the head's status: a refusal about the caller wins regardless
    let down = caller_store_down("w2:p2", "reviewer", "locked");
    assert!(Flock::check_review_caller_and_verdict(&d, &facts, Some("accept"), &down).is_err());
}

#[tokio::test]
async fn a_done_or_cancelled_review_takes_no_second_verdict_and_work_takes_none() {
    let (_flock, tasks) = setup("done").await;
    let (d, facts) = review(&tasks, true).await;
    let none = BTreeSet::new();
    let who = caller("w2:p2", "reviewer", true, &none);
    let err = Flock::check_task_completion_gates(&d, &facts, Some("accept"), &who)
        .unwrap_err()
        .to_string();
    assert!(err.contains("is already done with verdict"), "{err}");
    let work = tasks.add("global", "w", "").await.unwrap();
    let wd = tasks.decorate(&work.id, None).await.unwrap();
    let wf = tasks.review_facts(&work.id).await.unwrap();
    let err = Flock::check_task_completion_gates(&wd, &wf, Some("accept"), &who)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("--verdict applies to review tasks only"),
        "{err}"
    );
    assert_eq!(
        None,
        Flock::check_task_completion_gates(&wd, &wf, None, &who).unwrap()
    );
}

#[tokio::test]
async fn a_verdict_needs_a_note_by_the_same_lane_written_after_the_claim() {
    let (_flock, tasks) = setup("evidence").await;
    let t = tasks.add("global", "review", "").await.unwrap();
    // a note by another lane, and an empty one, do not count
    tasks
        .note(&t.id, "note", "w9:p9", Some("someone else's finding"))
        .await
        .unwrap();
    let err = Flock::require_review_evidence(&tasks, &t, Some("w2:p2"), "reviewer")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("a verdict needs evidence"), "{err}");
    assert!(err.contains(&format!("task note {}", t.id)), "{err}");
    tasks.note(&t.id, "note", "w2:p2", None).await.unwrap();
    assert!(
        Flock::require_review_evidence(&tasks, &t, Some("w2:p2"), "reviewer")
            .await
            .is_err()
    );
    tasks
        .note(
            &t.id,
            "note",
            "w2:p2",
            Some("ran cargo test; did NOT run clippy"),
        )
        .await
        .unwrap();
    assert!(
        Flock::require_review_evidence(&tasks, &t, Some("w2:p2"), "reviewer")
            .await
            .is_ok()
    );
    // owner/system with no lease is exempt; an anonymous caller is not
    let bare = tasks.add("global", "bare", "").await.unwrap();
    assert!(
        Flock::require_review_evidence(&tasks, &bare, Some("w1:p1"), "owner")
            .await
            .is_ok()
    );
    assert!(
        Flock::require_review_evidence(&tasks, &bare, None, "anonymous")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_note_written_before_the_claim_is_a_plan_not_evidence() {
    let (_flock, tasks) = setup("evidence-claim").await;
    let t = tasks.add("global", "review", "").await.unwrap();
    tasks
        .note(&t.id, "note", "w2:p2", Some("my plan"))
        .await
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    tasks
        .claim_next("global", "w2:p2", Default::default())
        .await
        .unwrap();
    let err = Flock::require_review_evidence(&tasks, &t, Some("w2:p2"), "reviewer")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("one written before you claimed, does not count"),
        "{err}"
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    tasks
        .note(&t.id, "note", "w2:p2", Some("what I ran"))
        .await
        .unwrap();
    assert!(
        Flock::require_review_evidence(&tasks, &t, Some("w2:p2"), "reviewer")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn authors_of_names_the_lanes_that_worked_a_pr_but_not_its_reviews() {
    let (_flock, tasks) = setup("authors").await;
    let work = |assignee: &str, author: Option<&str>| AddOptions {
        assignees: vec![assignee.into()],
        author: author.map(Into::into),
        repo: Some("o/r".into()),
        pr: Some("5".into()),
        ..Default::default()
    };
    tasks
        .add_with("global", "fix", "", work("w1:p1", Some("w4:p4")))
        .await
        .unwrap();
    tasks
        .add_with("global", "fix2", "", work("w3:p3", None))
        .await
        .unwrap();
    tasks
        .add_with(
            "global",
            "review",
            "",
            AddOptions {
                kind: Some("review".into()),
                assignees: vec!["w2:p2".into()],
                repo: Some("o/r".into()),
                pr: Some("5".into()),
                head: Some("abc".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        vec!["w1:p1", "w3:p3", "w4:p4"],
        tasks.authors_of("o/r", 5).await.unwrap()
    );
    assert!(tasks.authors_of("o/r", 0).await.unwrap().is_empty());
    assert!(tasks.authors_of("", 5).await.unwrap().is_empty());
}
