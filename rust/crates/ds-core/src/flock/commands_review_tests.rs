use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::*;
use crate::flock::panes::Stores;
use crate::github::{GitHubRequest, GitHubResponse, GitHubTransport, PinBoxFuture};
use crate::review::{ReviewFuture, ReviewPolicy, ReviewRepoConfig, ReviewTokenSource};
use crate::tasks::{AddOptions, Tasks};

struct TestTokens(Result<String, String>);

impl ReviewTokenSource for TestTokens {
    fn reviewer_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        let result = self.0.clone();
        Box::pin(async move { result })
    }
}

struct TestPolicy;

impl ReviewPolicy for TestPolicy {
    fn required_verdicts(&self, _: &str) -> Result<usize, String> {
        Ok(1)
    }
}

impl ReviewRepoConfig for TestPolicy {
    fn repos(&self) -> Result<Vec<String>, String> {
        Ok(vec!["o/r".into()])
    }

    fn author_from_mints(&self, _: f64) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[derive(Default)]
struct TestTransport {
    requests: Mutex<Vec<GitHubRequest>>,
    responses: Mutex<VecDeque<Result<GitHubResponse, String>>>,
    completed_task: Mutex<Option<(Arc<Tasks>, String)>>,
}

impl TestTransport {
    fn with_responses(values: impl IntoIterator<Item = (u16, Value)>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(
                values
                    .into_iter()
                    .map(|(status, value)| {
                        Ok(GitHubResponse {
                            status,
                            reason: "OK".into(),
                            body: serde_json::to_vec(&value).unwrap(),
                        })
                    })
                    .collect(),
            ),
            completed_task: Mutex::new(None),
        }
    }

    fn assert_task_done_before_requests(&self, tasks: Arc<Tasks>, id: &str) {
        *self.completed_task.lock().unwrap() = Some((tasks, id.to_owned()));
    }

    fn failing(message: &str) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(VecDeque::from([Err(message.to_owned())])),
            completed_task: Mutex::new(None),
        }
    }
}

impl GitHubTransport for TestTransport {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async move {
            let completed = self.completed_task.lock().unwrap().clone();
            if let Some((tasks, id)) = completed {
                let task = tasks.get(&id).await.map_err(|error| error.to_string())?;
                assert_eq!("done", task.status);
                assert_eq!(Some("accept"), task.verdict.as_deref());
            }
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(GitHubResponse {
                        status: 200,
                        reason: "OK".into(),
                        body: b"{}".to_vec(),
                    })
                })
        })
    }
}

async fn setup(tag: &str) -> (crate::testsupport::Scratch<Flock>, Arc<Tasks>) {
    let scratch = crate::testsupport::dir(&format!("review-adapter-{tag}"));
    let dir = scratch.path().to_path_buf();
    let tasks = Arc::new(Tasks::open(dir.join("tasks.db")).await.unwrap());
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false);
    let mut roles = crate::roles::Roles::new(
        &crate::config::Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    roles
        .register("%2", "reviewer", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Default::default()
    });
    (crate::testsupport::Scratch::new(flock, scratch), tasks)
}

fn response(status: u16, value: Value) -> (u16, Value) {
    (status, value)
}

#[tokio::test]
async fn test_accept_posts_success_and_approves_as_the_reviewer_app() {
    let (flock, tasks) = setup("verdict").await;
    let stored = tasks
        .add_with(
            "global",
            "review task",
            "",
            AddOptions {
                kind: Some("review".into()),
                author: Some("%3".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("head-7".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tasks
        .set_verdict(&stored.id, "accept", Some("%2"))
        .await
        .unwrap();
    tasks
        .task_complete(&stored.id, Some("%2"), true)
        .await
        .unwrap();

    let transport = TestTransport::with_responses([
        response(200, json!({"state":"success"})),
        response(200, json!({"id": 1})),
        response(200, json!({"merged": true})),
    ]);
    transport.assert_task_done_before_requests(tasks.clone(), &stored.id);
    let task = json!({
        "id": stored.id.clone(),
        "repo": "o/r",
        "pr": 7,
        "head": "head-7",
        "author": "%3"
    });

    let out = flock
        .after_review_verdict(
            &TestTokens(Ok("reviewer-app-token".into())),
            TestPolicy,
            "https://api.example.test",
            BorrowedGitHubTransport(&transport),
            &task,
            "accept",
            Some("%2"),
        )
        .await;

    assert_eq!(Value::Bool(true), out["merged"]);
    assert_eq!("done", tasks.get(&stored.id).await.unwrap().status);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(3, requests.len());
    assert_eq!("POST", requests[0].method);
    assert!(requests[0].url.ends_with("/repos/o/r/statuses/head-7"));
    assert_eq!("POST", requests[1].method);
    assert!(requests[1].url.ends_with("/repos/o/r/pulls/7/reviews"));
    assert_eq!(
        "token reviewer-app-token",
        requests[1].headers["Authorization"]
    );
    let approval: Value = serde_json::from_slice(requests[1].body.as_ref().unwrap()).unwrap();
    assert_eq!("APPROVE", approval["event"]);
    assert_eq!("PUT", requests[2].method);
    assert!(requests[2].url.ends_with("/repos/o/r/pulls/7/merge"));
}

#[tokio::test]
async fn after_review_verdict_contains_token_failure_without_rolling_back_verdict() {
    let (flock, tasks) = setup("token-error").await;
    let stored = tasks
        .add_with(
            "global",
            "review task",
            "",
            AddOptions {
                kind: Some("review".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("head-7".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tasks
        .set_verdict(&stored.id, "accept", Some("%2"))
        .await
        .unwrap();
    tasks
        .task_complete(&stored.id, Some("%2"), true)
        .await
        .unwrap();
    let task = json!({"id": stored.id.clone(), "repo":"o/r", "pr":7, "head":"head-7"});
    let transport = TestTransport::default();

    let out = flock
        .after_review_verdict(
            &TestTokens(Err("token cache unavailable".into())),
            TestPolicy,
            "https://api.example.test",
            BorrowedGitHubTransport(&transport),
            &task,
            "accept",
            Some("%2"),
        )
        .await;

    assert_eq!(json!(["token cache unavailable"]), out["errors"]);
    assert!(transport.requests.lock().unwrap().is_empty());
    let after = tasks.get(&stored.id).await.unwrap();
    assert_eq!("done", after.status);
    assert_eq!(Some("accept"), after.verdict.as_deref());
}

#[tokio::test]
async fn after_review_verdict_contains_transport_failure_without_rolling_back_verdict() {
    let (flock, tasks) = setup("transport-error").await;
    let stored = tasks
        .add_with(
            "global",
            "review task",
            "",
            AddOptions {
                kind: Some("review".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("head-7".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tasks
        .set_verdict(&stored.id, "accept", Some("%2"))
        .await
        .unwrap();
    tasks
        .task_complete(&stored.id, Some("%2"), true)
        .await
        .unwrap();
    let task = json!({"id": stored.id.clone(), "repo":"o/r", "pr":7, "head":"head-7"});
    let transport = TestTransport::failing("GitHub transport unavailable");

    let out = flock
        .after_review_verdict(
            &TestTokens(Ok("reviewer-app-token".into())),
            TestPolicy,
            "https://api.example.test",
            BorrowedGitHubTransport(&transport),
            &task,
            "accept",
            Some("%2"),
        )
        .await;

    assert!(!out["errors"].as_array().unwrap().is_empty());
    assert!(out["errors"].as_array().unwrap()[0]
        .as_str()
        .unwrap()
        .contains("GitHub transport unavailable"));
    let after = tasks.get(&stored.id).await.unwrap();
    assert_eq!("done", after.status);
    assert_eq!(Some("accept"), after.verdict.as_deref());
}

#[tokio::test]
async fn test_a_pr_becomes_one_review_task_with_a_pending_status() {
    let (flock, tasks) = setup("sync").await;
    let transport = TestTransport::with_responses([
        response(
            200,
            json!([{
                "number": 7,
                "title": "candidate",
                "html_url": "https://example.test/pull/7",
                "draft": false,
                "head": {"sha":"head-7", "ref":"work/topic"},
                "base": {"ref":"main"},
                "user": {"login":"author"}
            }]),
        ),
        response(200, json!({"mergeable":true, "mergeable_state":"clean"})),
        response(200, json!({"state":"open", "body":""})),
        response(201, json!({"id":1})),
    ]);

    let report = flock
        .review_sync(
            &TestTokens(Ok("reviewer-app-token".into())),
            TestPolicy,
            "https://api.example.test",
            BorrowedGitHubTransport(&transport),
            false,
        )
        .await
        .unwrap();

    assert_eq!(1, report["filed"].as_array().unwrap().len());
    let rows = tasks.list_review_rows_visible().await.unwrap();
    assert_eq!(1, rows.len());
    assert_eq!(Some("o/r"), rows[0].repo.as_deref());
    assert_eq!(Some(7), rows[0].pr);
    assert_eq!(Some("head-7"), rows[0].head.as_deref());
    assert_eq!("open", rows[0].status);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(4, requests.len());
    assert!(requests[3].url.ends_with("/repos/o/r/statuses/head-7"));
    assert_eq!("POST", requests[3].method);
    let status: Value = serde_json::from_slice(requests[3].body.as_ref().unwrap()).unwrap();
    assert_eq!("pending", status["state"]);
}

#[tokio::test]
async fn test_an_accepted_head_is_never_refiled_and_is_merged_when_protection_allows() {
    let (flock, tasks) = setup("accepted-head").await;
    let stored = tasks
        .add_with(
            "global",
            "accepted review",
            "",
            AddOptions {
                kind: Some("review".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some("head-7".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tasks
        .set_verdict(&stored.id, "accept", Some("%2"))
        .await
        .unwrap();
    tasks
        .task_complete(&stored.id, Some("%2"), true)
        .await
        .unwrap();
    let transport = TestTransport::with_responses([
        response(
            200,
            json!([{
                "number": 7,
                "title": "candidate",
                "html_url": "https://example.test/pull/7",
                "draft": false,
                "head": {"sha":"head-7", "ref":"work/topic"},
                "base": {"ref":"main"},
                "user": {"login":"author"}
            }]),
        ),
        response(200, json!({"mergeable":true, "mergeable_state":"clean"})),
        response(200, json!({"state":"open", "head":{"sha":"head-7"}})),
        response(
            200,
            json!({"statuses":[{"context":"dunkingsheep/review", "state":"success"}]}),
        ),
        response(200, json!({"merged":true})),
    ]);

    let report = flock
        .review_sync(
            &TestTokens(Ok("reviewer-app-token".into())),
            TestPolicy,
            "https://api.example.test",
            BorrowedGitHubTransport(&transport),
            false,
        )
        .await
        .unwrap();

    assert!(report["filed"].as_array().unwrap().is_empty());
    assert_eq!(1, report["merged"].as_array().unwrap().len());
    let rows = tasks.list_review_rows_visible().await.unwrap();
    assert_eq!(1, rows.len());
    assert_eq!(stored.id, rows[0].id);
}
