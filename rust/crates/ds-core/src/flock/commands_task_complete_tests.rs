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

async fn setup(tag: &str) -> (Flock, std::sync::Arc<Tasks>) {
    let dir = std::env::temp_dir().join(format!("ds-tc-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    let tasks = std::sync::Arc::new(Tasks::open(&path).await.unwrap());
    let flock = Flock::new(dir.join("l.json"), dir.join("n.json"), false);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (flock, tasks)
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
        worked,
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
