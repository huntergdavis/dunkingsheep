//! Ports of the guard cases of tests/test_dunk_complete_ownership*.py and the review gates of
//! tests/test_dunk_github.py that need no backend: ownership, the binding's refusals before
//! the process walk, and the review-completion gate order.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::api::{CommandEnv, HandlerContext};
use crate::config::Env;
use crate::flock::panes::Stores;
use crate::github::{GitHubRequest, GitHubResponse, GitHubTransport, PinBoxFuture, Tokens};
use crate::proc::Caller;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::tasks::next_job_runtime::NextJobs;
use crate::tasks::{AddOptions, Task, Tasks};
use crate::terminal::{FakeTerminal, Terminal};

#[tokio::test]
async fn after_review_verdict_preserves_the_review_service_result() {
    let scratch = crate::testsupport::dir("tc-review-verdict-ok");
    let flock = Flock::new(
        scratch.path().join("l.json"),
        scratch.path().join("n.json"),
        false,
    );
    let task = json!({"id": "t1"});
    let expected = json!({"review": "approved", "merged": true});
    let result = flock
        .contain_review_verdict(&task, async { Ok(expected.clone()) })
        .await;
    assert_eq!(expected, result);
}

#[tokio::test]
async fn after_review_verdict_contains_github_errors_without_failing_completion() {
    let scratch = crate::testsupport::dir("tc-review-verdict-error");
    let flock = Flock::new(
        scratch.path().join("l.json"),
        scratch.path().join("n.json"),
        false,
    );
    let task = json!({"id": "t9"});
    let result = flock
        .contain_review_verdict(&task, async { Err("GitHub unavailable".to_string()) })
        .await;
    assert_eq!(json!({"errors": ["GitHub unavailable"]}), result);
}

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

struct TaskCompletionEnv<'a> {
    terminal: Mutex<Box<dyn Terminal>>,
    jobs: &'a NextJobs,
    tokens: Tokens,
    transport: Box<dyn GitHubTransport>,
    pane: String,
}

impl PaneInfoEnv for TaskCompletionEnv<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        vec![json!({"pane_id": self.pane, "pane_pid": 1101})]
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        (pane_id == self.pane).then(|| json!({"pane_id": self.pane, "pane_pid": 1101}))
    }
}

impl CommandEnv for TaskCompletionEnv<'_> {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }

    fn next_jobs(&self) -> Result<&NextJobs, DunkError> {
        Ok(self.jobs)
    }

    fn github_tokens(&self) -> Result<&Tokens, DunkError> {
        Ok(&self.tokens)
    }

    fn review_transport(&self) -> Result<&dyn GitHubTransport, DunkError> {
        Ok(self.transport.as_ref())
    }
}

#[derive(Default)]
struct NoNetworkTransport;

impl GitHubTransport for NoNetworkTransport {
    fn send<'a>(&'a self, _: GitHubRequest) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async { Err("unexpected token mint in task completion test".into()) })
    }
}

struct CompletionReviewTransport {
    tasks: Arc<Tasks>,
    task_id: String,
    observed: Arc<Mutex<Vec<(String, Option<String>)>>>,
    fail: bool,
}

impl GitHubTransport for CompletionReviewTransport {
    fn send<'a>(&'a self, _: GitHubRequest) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async move {
            let task = self
                .tasks
                .get_full_wire(&self.task_id, None)
                .await
                .map_err(|error| error.to_string())?;
            self.observed.lock().unwrap().push((
                task.typed.status,
                task.wire["verdict"].as_str().map(str::to_owned),
            ));
            if self.fail {
                Err("scripted ReviewSync transport failure".into())
            } else {
                Ok(GitHubResponse {
                    status: 200,
                    reason: "OK".into(),
                    body: b"{}".to_vec(),
                })
            }
        })
    }
}

fn completion_services(
    role: &str,
    dispatch: bool,
) -> (
    tempfile::TempDir,
    tokio::runtime::Runtime,
    Arc<Flock>,
    Arc<Tasks>,
    NextJobs,
    Tokens,
) {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(dir.path().join("tasks.db")))
            .unwrap(),
    );
    runtime.block_on(tasks.allocation_init()).unwrap();
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    ));
    let role_path = dir.path().join("roles.json");
    std::fs::write(
        &role_path,
        serde_json::to_vec(&json!({
            "lanes": {"w1:p1": {"role": role, "provider": "openai"}},
            "reviewer_providers": ["openai"]
        }))
        .unwrap(),
    )
    .unwrap();
    let roles = Roles::new(
        &Env::from_pairs(&[]),
        Some(role_path),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .unwrap();
    flock.attach_roles(roles, None);
    let inbox = Arc::new(
        runtime
            .block_on(tokio::task::spawn_blocking({
                let db = dir.path().join("inbox.db");
                let blobs = dir.path().join("blobs");
                move || crate::inbox_store::Store::open(db, blobs)
            }))
            .unwrap()
            .unwrap(),
    );
    flock.attach_stores(Stores {
        tasks: Some(Arc::clone(&tasks)),
        inbox: Some(inbox),
        ..Default::default()
    });
    let pane = "w1:p1".to_owned();
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_DIR", dir.path().to_str().unwrap()),
        (
            "DUNKINGSHEEP_TURN_END_DISPATCH",
            if dispatch { "1" } else { "0" },
        ),
        ("DUNKINGSHEEP_DISPATCH_LANES", pane.as_str()),
    ]);
    let factory: crate::flock::loop_env::LoopTerminalFactory =
        Arc::new(|| Box::new(FakeTerminal::default()));
    let jobs = NextJobs::with_providers(
        Arc::clone(&flock),
        Arc::new(move || Ok(env.clone())),
        Some(factory),
    );
    let token_path = dir.path().join("github-tokens.json");
    std::fs::write(
        &token_path,
        serde_json::to_vec(&json!({
            "tokens": {"reviewer": {
                "token": "reviewer-test-token",
                "expires_at": 4_102_444_800.0
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let tokens = Tokens::new(
        Some(token_path),
        dir.path().to_path_buf(),
        Box::new(NoNetworkTransport),
        Some("https://github.invalid".into()),
    );
    (dir, runtime, flock, tasks, jobs, tokens)
}

fn completion_env<'a>(
    jobs: &'a NextJobs,
    tokens: Tokens,
    transport: Box<dyn GitHubTransport>,
) -> TaskCompletionEnv<'a> {
    TaskCompletionEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        jobs,
        tokens,
        transport,
        pane: "w1:p1".into(),
    }
}

fn invoke_task_complete(
    flock: &Flock,
    runtime: &tokio::runtime::Runtime,
    env: &dyn CommandEnv,
    request: &RequestCtx,
    task_id: &str,
    by: Option<&str>,
    verdict: Option<&str>,
    landed: bool,
) -> Result<Value, DunkError> {
    let handle = runtime.handle();
    let context = HandlerContext {
        flock,
        request,
        env,
        runtime: &handle,
    };
    flock.task_complete(&context, task_id, by, verdict, landed)
}

fn completion_request() -> RequestCtx {
    RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..Default::default()
    }
}

fn add_owned_task(runtime: &tokio::runtime::Runtime, tasks: &Tasks, title: &str) -> Task {
    runtime
        .block_on(tasks.add_with(
            "global",
            title,
            "",
            AddOptions {
                assignees: vec!["w1:p1".into()],
                ..Default::default()
            },
        ))
        .unwrap()
}

fn add_review_task(runtime: &tokio::runtime::Runtime, tasks: &Tasks) -> Task {
    runtime
        .block_on(tasks.add_with(
            "global",
            "review",
            "",
            AddOptions {
                kind: Some("review".into()),
                assignees: vec!["w1:p1".into()],
                author: Some("w9:p9".into()),
                repo: Some("o/r".into()),
                pr: Some("5".into()),
                head: Some("abc".into()),
                ..Default::default()
            },
        ))
        .unwrap()
}

#[test]
fn task_complete_wakes_next_job_push_after_commit() {
    let (_dir, runtime, flock, tasks, jobs, tokens) = completion_services("owner", true);
    let current = add_owned_task(&runtime, &tasks, "current");
    let next = add_owned_task(&runtime, &tasks, "next");
    runtime
        .block_on(tasks.update_with_status(
            &current.id,
            Default::default(),
            Some("in_progress"),
            Some("w1:p1"),
        ))
        .unwrap();
    let wake = flock.push_wake();
    let notified = wake.notified();
    let env = completion_env(&jobs, tokens, Box::<NoNetworkTransport>::default());
    let result = invoke_task_complete(
        &flock,
        &runtime,
        &env,
        &completion_request(),
        &current.id,
        None,
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        "done",
        runtime.block_on(tasks.get(&current.id)).unwrap().status
    );
    assert!(result["next_job"].is_object());
    assert!(result["next_job"].get("task").is_none());
    assert!(runtime
        .block_on(tokio::time::timeout(
            std::time::Duration::from_secs(1),
            notified
        ))
        .is_ok());
    let next_after = runtime.block_on(tasks.get(&next.id)).unwrap();
    assert_eq!("in_progress", next_after.status);
}

#[test]
fn task_complete_landed_false_closes_and_true_opens_dependency() {
    let (_dir, runtime, flock, tasks, jobs, tokens) = completion_services("owner", false);
    let current = add_owned_task(&runtime, &tasks, "stacked current");
    let dependent = runtime
        .block_on(tasks.add_with(
            "global",
            "dependent",
            "",
            AddOptions {
                assignees: vec!["w1:p1".into()],
                depends_on: vec![current.id.clone()],
                ..Default::default()
            },
        ))
        .unwrap();
    let env = completion_env(&jobs, tokens, Box::<NoNetworkTransport>::default());
    let first = invoke_task_complete(
        &flock,
        &runtime,
        &env,
        &completion_request(),
        &current.id,
        None,
        None,
        false,
    )
    .unwrap();
    assert_eq!(false, first["landed"]);
    let blocked = runtime
        .block_on(tasks.decorate(&dependent.id, None))
        .unwrap();
    assert_eq!(vec![current.id.clone()], blocked.deps.blocked_by);
    runtime
        .block_on(tasks.set_landed(&current.id, true, Some("w1:p1"), None, None))
        .unwrap();
    let opened = runtime
        .block_on(tasks.decorate(&dependent.id, None))
        .unwrap();
    assert!(opened.deps.blocked_by.is_empty());
}

#[test]
fn task_complete_review_sync_observes_committed_verdict_and_contains_failure() {
    let (_dir, runtime, flock, tasks, jobs, tokens) = completion_services("reviewer", false);
    let review = add_review_task(&runtime, &tasks);
    runtime
        .block_on(tasks.claim_next("global", "w1:p1", Default::default()))
        .unwrap();
    runtime
        .block_on(tasks.note(&review.id, "note", "w1:p1", Some("reviewed")))
        .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let transport = CompletionReviewTransport {
        tasks: Arc::clone(&tasks),
        task_id: review.id.clone(),
        observed: Arc::clone(&observed),
        fail: true,
    };
    let env = completion_env(&jobs, tokens, Box::new(transport));
    let result = invoke_task_complete(
        &flock,
        &runtime,
        &env,
        &completion_request(),
        &review.id,
        None,
        Some("accept"),
        true,
    )
    .unwrap();
    assert_eq!(
        "done",
        runtime.block_on(tasks.get(&review.id)).unwrap().status
    );
    assert_eq!(
        Some("accept"),
        runtime
            .block_on(tasks.get_full_wire(&review.id, None))
            .unwrap()
            .wire["verdict"]
            .as_str()
    );
    assert!(!result["github"]["errors"].as_array().unwrap().is_empty());
    assert_eq!(
        vec![("done".into(), Some("accept".into()))],
        *observed.lock().unwrap()
    );
}

#[test]
fn task_complete_advance_failure_does_not_wake_or_run_review_sync() {
    let (_dir, runtime, flock, tasks, jobs, tokens) = completion_services("reviewer", false);
    let review = add_review_task(&runtime, &tasks);
    runtime
        .block_on(tasks.claim_next("global", "w1:p1", Default::default()))
        .unwrap();
    runtime
        .block_on(tasks.note(&review.id, "note", "w1:p1", Some("reviewed")))
        .unwrap();
    let conn = rusqlite::Connection::open(tasks.path()).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER reject_completion BEFORE UPDATE OF status ON tasks \
         WHEN OLD.id = '{}' AND NEW.status = 'done' \
         BEGIN SELECT RAISE(ABORT, 'scripted advance failure'); END;",
        review.id
    ))
    .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let env = completion_env(
        &jobs,
        tokens,
        Box::new(CompletionReviewTransport {
            tasks: Arc::clone(&tasks),
            task_id: review.id.clone(),
            observed: Arc::clone(&observed),
            fail: false,
        }),
    );
    let wake = flock.push_wake();
    let result = invoke_task_complete(
        &flock,
        &runtime,
        &env,
        &completion_request(),
        &review.id,
        None,
        Some("accept"),
        true,
    );
    assert!(result.is_err());
    assert_eq!(
        "in_progress",
        runtime.block_on(tasks.get(&review.id)).unwrap().status
    );
    assert!(observed.lock().unwrap().is_empty());
    assert!(runtime
        .block_on(tokio::time::timeout(
            std::time::Duration::from_millis(25),
            wake.notified()
        ))
        .is_err());
}

#[test]
fn task_complete_landed_failure_is_after_wake_and_skips_review_sync() {
    let (_dir, runtime, flock, tasks, jobs, tokens) = completion_services("reviewer", false);
    let review = add_review_task(&runtime, &tasks);
    runtime
        .block_on(tasks.claim_next("global", "w1:p1", Default::default()))
        .unwrap();
    runtime
        .block_on(tasks.note(&review.id, "note", "w1:p1", Some("reviewed")))
        .unwrap();
    let conn = rusqlite::Connection::open(tasks.path()).unwrap();
    conn.execute_batch(&format!(
        "CREATE TRIGGER reject_unlanded BEFORE UPDATE OF landed ON tasks \
         WHEN OLD.id = '{}' AND NEW.landed = 0 \
         BEGIN SELECT RAISE(ABORT, 'scripted landed failure'); END;",
        review.id
    ))
    .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let env = completion_env(
        &jobs,
        tokens,
        Box::new(CompletionReviewTransport {
            tasks: Arc::clone(&tasks),
            task_id: review.id.clone(),
            observed: Arc::clone(&observed),
            fail: false,
        }),
    );
    let wake = flock.push_wake();
    let result = invoke_task_complete(
        &flock,
        &runtime,
        &env,
        &completion_request(),
        &review.id,
        None,
        Some("accept"),
        false,
    );
    assert!(result.is_err());
    assert_eq!(
        "done",
        runtime.block_on(tasks.get(&review.id)).unwrap().status
    );
    assert!(observed.lock().unwrap().is_empty());
    assert!(runtime
        .block_on(tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wake.notified()
        ))
        .is_ok());
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
