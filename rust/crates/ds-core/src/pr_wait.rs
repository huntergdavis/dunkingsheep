//! One-shot read-only GitHub waits, driven by the existing review-sync timer.
//! Review reads are recorded, never cached back into review decisions. Only the wait pass
//! reuses them. No GitHub write path is reachable from this module.
use crate::github::{
    self, GitHubRequest, GitHubResponse, GitHubTransport, PinBoxFuture, RequestOptions,
};
use crate::inbox_store::Store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// SyncWriter owns a separate runtime. Its calls must run on a blocking thread,
/// with owned arguments, rather than nesting that runtime inside the network future.
async fn db<T: Send + 'static>(
    store: &Arc<Store>,
    work: impl FnOnce(&Store) -> Result<T, crate::error::DunkError> + Send + 'static,
) -> Result<T, String> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || work(&store))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

pub struct SweepTransport<'a> {
    inner: &'a dyn GitHubTransport,
    reads: Mutex<BTreeMap<String, Value>>,
    gets: std::sync::atomic::AtomicUsize,
    reviews: Vec<crate::tasks::ReviewTaskRow>,
}
impl<'a> SweepTransport<'a> {
    pub fn new(inner: &'a dyn GitHubTransport) -> Self {
        Self {
            inner,
            reads: Mutex::new(BTreeMap::new()),
            gets: std::sync::atomic::AtomicUsize::new(0),
            reviews: Vec::new(),
        }
    }
    pub(crate) fn set_reviews(&mut self, rows: Vec<crate::tasks::ReviewTaskRow>) {
        self.reviews = rows;
    }
    async fn get(
        &self,
        store: &Arc<Store>,
        root: &str,
        path: &str,
        token: &str,
    ) -> Result<Value, String> {
        let url = format!("{root}/repos/{path}");
        let cached = self.reads.lock().unwrap().get(&url).cloned();
        if let Some(v) = cached {
            return Ok(v);
        }
        let conditional = Conditional {
            inner: self.inner,
            store,
            gets: &self.gets,
        };
        let (_, body) = github::request(
            &conditional,
            RequestOptions {
                method: "GET",
                url: &url,
                token: Some(token),
                bearer: None,
                payload: None,
                timeout_s: 20,
                api_root: root,
            },
        )
        .await
        .map_err(|_| "GitHub read failed".to_owned())?;
        body.ok_or_else(|| "GitHub returned no data".into())
    }
}
impl GitHubTransport for SweepTransport<'_> {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async move {
            let url = request.url.clone();
            let read = request.method == "GET";
            let response = self.inner.send(request).await?;
            if read && response.status == 200 {
                if let Ok(body) = serde_json::from_slice(&response.body) {
                    self.reads.lock().unwrap().insert(url, body);
                }
            }
            Ok(response)
        })
    }
}

struct Conditional<'a> {
    inner: &'a dyn GitHubTransport,
    store: &'a Arc<Store>,
    gets: &'a std::sync::atomic::AtomicUsize,
}
impl GitHubTransport for Conditional<'_> {
    fn send<'a>(
        &'a self,
        mut request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        Box::pin(async move {
            let url = request.url.clone();
            let cache_url = url.clone();
            let cached = db(self.store, move |s| s.pr_wait_http_get(&cache_url)).await?;
            if let Some((etag, _)) = &cached {
                request.headers.insert("If-None-Match".into(), etag.clone());
            }
            self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (response, etag) = self.inner.send_with_etag(request).await?;
            if response.status == 304 {
                let (_, body) = cached.ok_or("304 without cached body")?;
                return Ok(GitHubResponse {
                    status: 200,
                    reason: "OK".into(),
                    body,
                });
            }
            if response.status == 200 {
                if let Some(etag) = etag {
                    let body = response.body.clone();
                    db(self.store, move |s| s.pr_wait_http_put(url, etag, body)).await?;
                }
            }
            Ok(response)
        })
    }
}

struct Observation {
    head: String,
    terminal: Option<String>,
    verdict: Option<String>,
    checks: Option<String>,
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}
fn checks_result(
    required: &Value,
    runs: &Value,
    combined: &Value,
) -> Result<Option<String>, String> {
    let contexts = required
        .get("contexts")
        .and_then(Value::as_array)
        .ok_or("invalid required contexts")?;
    let checks = required
        .get("checks")
        .and_then(Value::as_array)
        .ok_or("invalid required checks")?;
    let mut names = BTreeMap::new();
    for context in contexts {
        names.insert(context.as_str().ok_or("invalid context")?.to_owned(), None);
    }
    for check in checks {
        names.insert(
            string(check, "context")?.to_owned(),
            check
                .get("app_id")
                .and_then(Value::as_i64)
                .filter(|id| *id > 0),
        );
    }
    // The review status has its own terminal event; it must not hold up CI completion.
    names.remove("dunkingsheep/review");
    if names.is_empty() {
        return Ok(None);
    }
    let runs = runs
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or("invalid check runs")?;
    let statuses = combined
        .get("statuses")
        .and_then(Value::as_array)
        .ok_or("invalid commit statuses")?;
    let mut failed = Vec::new();
    for (name, app_id) in names {
        // API returns newest first. Ignore superseded attempts of the same check.
        if let Some(run) = runs.iter().find(|r| {
            r.get("name").and_then(Value::as_str) == Some(&name)
                && app_id.is_none_or(|id| r["app"]["id"].as_i64() == Some(id))
        }) {
            if string(run, "status")? != "completed" {
                return Ok(None);
            }
            if !matches!(
                string(run, "conclusion")?,
                "success" | "neutral" | "skipped"
            ) {
                failed.push(name);
            }
        } else if let Some(status) = statuses
            .iter()
            .filter(|_| app_id.is_none())
            .find(|s| s.get("context").and_then(Value::as_str) == Some(&name))
        {
            match string(status, "state")? {
                "pending" => return Ok(None),
                "success" => {}
                "failure" | "error" => failed.push(name),
                _ => return Err("invalid status state".into()),
            }
        } else {
            return Ok(None);
        }
    }
    Ok(Some(if failed.is_empty() {
        "required checks passed".into()
    } else {
        format!("required checks failed: {}", failed.join(", "))
    }))
}

impl SweepTransport<'_> {
    async fn observe(
        &self,
        store: &Arc<Store>,
        root: &str,
        repo: &str,
        pr: i64,
        token: &str,
        waits: &[crate::inbox_store::pr_wait::Wait],
    ) -> Result<Observation, String> {
        let pull = self
            .get(store, root, &format!("{repo}/pulls/{pr}"), token)
            .await?;
        let head = string(&pull["head"], "sha")?.to_owned();
        for wait in waits {
            let id = wait.id;
            let head = head.clone();
            db(store, move |s| s.pr_wait_head(id, &head)).await?;
        }
        let terminal = if pull.get("merged").and_then(Value::as_bool) == Some(true) {
            Some("PR merged".into())
        } else if string(&pull, "state")? == "closed" {
            Some("PR closed".into())
        } else {
            None
        };
        let mut out = Observation {
            head,
            terminal,
            verdict: None,
            checks: None,
        };
        if out.terminal.is_some()
            || waits
                .iter()
                .filter_map(|w| w.head.as_ref())
                .any(|h| h != &out.head)
        {
            return Ok(out);
        }
        // A first verdict in a multi-review repository is an event even while the
        // aggregate GitHub status remains pending. Apply the canonical exact-head guards.
        if let Some(row) = self
            .reviews
            .iter()
            .filter(|row| {
                row.status == "done"
                    && row.repo.as_deref() == Some(repo)
                    && row.pr == Some(pr)
                    && row.head.as_deref() == Some(out.head.as_str())
                    && row
                        .completed_by
                        .as_deref()
                        .is_some_and(|lane| !lane.is_empty())
                    && matches!(row.verdict.as_deref(), Some("accept" | "needs-fix"))
            })
            .min_by_key(|row| row.verdict.as_deref() != Some("needs-fix"))
        {
            out.verdict = Some(format!(
                "review verdict: {} (task {})",
                row.verdict.as_deref().unwrap(),
                row.id
            ));
            return Ok(out);
        }
        let combined = self
            .get(
                store,
                root,
                &format!("{repo}/commits/{}/status", out.head),
                token,
            )
            .await?;
        let statuses = combined
            .get("statuses")
            .and_then(Value::as_array)
            .ok_or("invalid commit statuses")?;
        for s in statuses {
            if s.get("context").and_then(Value::as_str) == Some("dunkingsheep/review") {
                let state = string(s, "state")?;
                if matches!(state, "success" | "failure" | "error") {
                    out.verdict = Some(format!("review verdict: {state}"));
                }
                break;
            }
        }
        if out.verdict.is_some() {
            return Ok(out);
        }
        let base = string(&pull["base"], "ref")?;
        let branch: String = base
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let required = self
            .get(
                store,
                root,
                &format!("{repo}/branches/{branch}/protection/required_status_checks"),
                token,
            )
            .await?;
        let runs = self
            .get(
                store,
                root,
                &format!("{repo}/commits/{}/check-runs?per_page=100", out.head),
                token,
            )
            .await?;
        // Refuse truncation instead of declaring completion with an incomplete check set.
        if runs
            .get("total_count")
            .and_then(Value::as_u64)
            .ok_or("invalid check count")?
            > 100
            || combined
                .get("total_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 100
        {
            return Err("too many checks to watch safely".into());
        }
        out.checks = checks_result(&required, &runs, &combined)?;
        Ok(out)
    }
    pub(crate) async fn poll(
        &self,
        store: &Arc<Store>,
        root: &str,
        token: Result<String, String>,
        now: f64,
    ) -> Result<Value, String> {
        // Expire before making any GitHub reads, even if the timer was stopped for a day.
        db(store, move |s| s.pr_wait_pending(now)).await?;
        let watches = db(store, move |s| s.pr_wait_due(now)).await?;
        let mut completed = 0;
        for watch in watches {
            let repo = watch.repo.clone();
            let pr = watch.pr;
            let waits = db(store, move |s| s.pr_wait_rows(&repo, pr)).await?;
            let observed = match &token {
                Ok(token) => {
                    self.observe(store, root, &watch.repo, watch.pr, token, &waits)
                        .await
                }
                Err(_) => Err("GitHub token unavailable".into()),
            };
            match observed {
                Ok(observation) => {
                    for wait in waits {
                        let event = if wait.head.as_ref().is_some_and(|h| h != &observation.head) {
                            Some(format!("head moved to {}", observation.head))
                        } else {
                            observation
                                .terminal
                                .clone()
                                .or(observation.verdict.clone())
                                .or(observation.checks.clone())
                        };
                        if let Some(event) = event {
                            let id = wait.id;
                            let body = format!(
                                "{}#{} {}: {event}",
                                watch.repo, watch.pr, observation.head
                            );
                            db(store, move |s| s.pr_wait_finish(id, &body)).await?;
                            completed += 1;
                        } else {
                            let id = wait.id;
                            let head = observation.head.clone();
                            db(store, move |s| s.pr_wait_head(id, &head)).await?;
                        }
                    }
                    db(store, move |s| s.pr_wait_retry(&watch, now, false)).await?;
                }
                Err(_) => {
                    // Four attempts, at >= 120/240/480/960 seconds, then one notice.
                    if watch.failures >= 3 {
                        for wait in waits {
                            let id = wait.id;
                            let body=format!("{}#{}: could not watch after 4 failed attempts; register again to retry",watch.repo,watch.pr);
                            db(store, move |s| s.pr_wait_finish(id, &body)).await?;
                            completed += 1;
                        }
                    } else {
                        db(store, move |s| s.pr_wait_retry(&watch, now, true)).await?;
                    }
                }
            }
        }
        let gets = self.gets.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("pr-wait sweep: {gets} shared GETs, {completed} ending events");
        Ok(json!({"completed":completed,"gets":gets}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::dir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fake {
        pull: Value,
        status: Value,
        runs: Value,
        calls: AtomicUsize,
        fail: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                pull: json!({"head":{"sha":"abc"},"base":{"ref":"main"},"state":"open","merged":false}),
                status: json!({"statuses":[],"total_count":0}),
                runs: json!({"total_count":1,"check_runs":[{"name":"CI","status":"completed","conclusion":"success"}]}),
                calls: AtomicUsize::new(0),
                fail: false,
            }
        }
    }
    impl GitHubTransport for Fake {
        fn send<'a>(
            &'a self,
            request: GitHubRequest,
        ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
            Box::pin(async move {
                assert_eq!(request.method, "GET");
                self.calls.fetch_add(1, Ordering::Relaxed);
                if self.fail {
                    return Err("fixture failure".into());
                }
                let body = if request.url.contains("/pulls/") {
                    self.pull.clone()
                } else if request.url.ends_with("/status") {
                    self.status.clone()
                } else if request.url.contains("/check-runs") {
                    self.runs.clone()
                } else if request.url.contains("/protection/") {
                    json!({"contexts":["CI"],"checks":[]})
                } else {
                    panic!("unexpected URL {}", request.url)
                };
                Ok(GitHubResponse {
                    status: 200,
                    reason: "OK".into(),
                    body: serde_json::to_vec(&body).unwrap(),
                })
            })
        }
    }
    fn store(label: &str) -> (tempfile::TempDir, Arc<Store>) {
        let d = dir(label);
        let s = Arc::new(Store::open(d.path().join("inbox.db"), d.path().join("blobs")).unwrap());
        (d, s)
    }
    fn register(s: &Store, mailbox: &str, head: Option<&str>) -> i64 {
        s.pr_wait_register("o/r", 1, mailbox, "%2", head).unwrap()["id"]
            .as_i64()
            .unwrap()
    }
    fn poll(s: &Arc<Store>, f: &Fake, now: f64) -> Value {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(SweepTransport::new(f).poll(s, "https://api.test", Ok("fixture".into()), now))
            .unwrap()
    }
    #[test]
    fn pr_wait_fixture_ending_events_and_shared_calls() {
        for event in ["pass", "fail", "head", "review", "merged", "closed"] {
            let (_d, s) = store(&format!("pr-wait-{event}"));
            register(&s, "one", Some("abc"));
            register(&s, "two", Some("abc"));
            let mut f = Fake::new();
            match event {
                "fail" => f.runs["check_runs"][0]["conclusion"] = json!("failure"),
                "head" => f.pull["head"]["sha"] = json!("def"),
                "review" => {
                    f.status["statuses"] =
                        json!([{"context":"dunkingsheep/review","state":"failure"}])
                }
                "merged" => {
                    f.pull["merged"] = json!(true);
                    f.pull["state"] = json!("closed");
                }
                "closed" => f.pull["state"] = json!("closed"),
                _ => {}
            }
            assert_eq!(poll(&s, &f, 100.)["completed"], 2);
            let pending = s.pr_wait_pending(100.).unwrap();
            assert_eq!(pending.len(), 2);
            let text = &pending[0].2;
            assert!(
                text.contains(match event {
                    "pass" => "checks passed",
                    "fail" => "checks failed: CI",
                    "head" => "head moved to def",
                    "review" => "review verdict: failure",
                    "merged" => "PR merged",
                    _ => "PR closed",
                }),
                "{text}"
            );
            assert_eq!(
                f.calls.load(Ordering::Relaxed),
                match event {
                    "pass" | "fail" => 4,
                    "review" => 2,
                    _ => 1,
                }
            );
            poll(&s, &f, 300.);
            assert_eq!(s.pr_wait_pending(300.).unwrap().len(), 2);
        }
    }
    #[test]
    fn pr_wait_restart_delivery_and_bounded_failure() {
        let (d, s) = store("pr-wait-restart");
        register(&s, "one", Some("abc"));
        let mut f = Fake::new();
        f.fail = true;
        poll(&s, &f, 100.);
        poll(&s, &f, 200.);
        assert_eq!(f.calls.load(Ordering::Relaxed), 1);
        poll(&s, &f, 340.);
        poll(&s, &f, 820.);
        assert!(s.pr_wait_pending(100.).unwrap().is_empty());
        drop(s);
        let s = Arc::new(Store::open(d.path().join("inbox.db"), d.path().join("blobs")).unwrap());
        poll(&s, &f, 1780.);
        let pending = s.pr_wait_pending(1780.).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].2.contains("could not watch"));
        s.pr_wait_delivered(pending[0].0).unwrap();
        drop(s);
        let s = Arc::new(Store::open(d.path().join("inbox.db"), d.path().join("blobs")).unwrap());
        poll(&s, &f, 5000.);
        assert!(s.pr_wait_pending(5000.).unwrap().is_empty());
        assert_eq!(f.calls.load(Ordering::Relaxed), 4);
    }
    #[test]
    fn pr_wait_cancellation_scope_expiry_and_global_bound() {
        let (_d, s) = store("pr-wait-cap");
        let id = register(&s, "one", None);
        assert_eq!(s.pr_wait_list("other", Some(id)).unwrap()["cancelled"], 0);
        assert_eq!(s.pr_wait_list("one", Some(id)).unwrap()["cancelled"], 1);
        let id = register(&s, "one", Some("abc"));
        assert_eq!(register(&s, "one", Some("abc")), id);
        assert!(s
            .pr_wait_register("o/r", 1, "one", "%2", Some("def"))
            .is_err());
        s.pr_wait_list("one", Some(id)).unwrap();
        for pr in 1..=10 {
            s.pr_wait_register("o/r", pr, "one", "%2", None).unwrap();
        }
        assert!(s.pr_wait_register("o/r", 11, "one", "%2", None).is_err());
        s.pr_wait_register("o/r", 1, "two", "%3", None).unwrap();
        assert_eq!(
            s.pr_wait_pending(crate::clock::now_s() + 86401.)
                .unwrap()
                .len(),
            11
        );
    }
    #[test]
    fn pr_wait_first_local_verdict_ends_wait_while_aggregate_is_pending_only_for_exact_head() {
        for head in ["abc", "old"] {
            let (_d, s) = store(&format!("pr-wait-local-verdict-{head}"));
            register(&s, "one", Some("abc"));
            let mut f = Fake::new();
            f.status["statuses"] = json!([{"context":"dunkingsheep/review","state":"pending"}]);
            f.runs["check_runs"][0]["status"] = json!("in_progress");
            let mut sweep = SweepTransport::new(&f);
            sweep.set_reviews(vec![crate::tasks::ReviewTaskRow {
                id: "t-review".into(),
                status: "done".into(),
                completed_by: Some("%reviewer".into()),
                repo: Some("o/r".into()),
                pr: Some(1),
                head: Some(head.into()),
                verdict: Some("accept".into()),
            }]);
            let result = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(sweep.poll(&s, "https://api.test", Ok("fixture".into()), 100.))
                .unwrap();
            if head == "abc" {
                assert_eq!(result["completed"], 1);
                assert_eq!(f.calls.load(Ordering::Relaxed), 1);
                assert!(s.pr_wait_pending(100.).unwrap()[0]
                    .2
                    .contains("review verdict: accept (task t-review)"));
            } else {
                assert_eq!(result["completed"], 0);
                assert_eq!(f.calls.load(Ordering::Relaxed), 4);
                assert!(s.pr_wait_pending(100.).unwrap().is_empty());
            }
        }
    }
    #[test]
    fn pr_wait_missing_pending_and_app_bound_checks_do_not_pass() {
        let req = json!({"contexts":["CI"],"checks":[{"context":"CI","app_id":42}]});
        let statuses = json!({"statuses":[{"context":"CI","state":"success"}]});
        let mut runs = json!({"check_runs":[{"name":"CI","status":"completed","conclusion":"success","app":{"id":7}}]});
        assert_eq!(checks_result(&req, &runs, &statuses).unwrap(), None);
        runs["check_runs"][0]["app"]["id"] = json!(42);
        runs["check_runs"][0]["status"] = json!("in_progress");
        assert_eq!(checks_result(&req, &runs, &statuses).unwrap(), None);
        runs["check_runs"][0]["status"] = json!("completed");
        assert!(checks_result(&req, &runs, &statuses)
            .unwrap()
            .unwrap()
            .contains("passed"));
    }
}

#[cfg(test)]
mod etag_tests {
    use super::*;
    struct Endpoint(Mutex<Vec<GitHubRequest>>);
    impl GitHubTransport for Endpoint {
        fn send<'a>(
            &'a self,
            _: GitHubRequest,
        ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
            panic!("conditional transport required")
        }
        fn send_with_etag<'a>(
            &'a self,
            request: GitHubRequest,
        ) -> PinBoxFuture<'a, Result<(GitHubResponse, Option<String>), String>> {
            Box::pin(async move {
                let conditional = request.headers.contains_key("If-None-Match");
                self.0.lock().unwrap().push(request);
                Ok((
                    GitHubResponse {
                        status: if conditional { 304 } else { 200 },
                        reason: "OK".into(),
                        body: if conditional {
                            Vec::new()
                        } else {
                            b"{\"fixture\":true}".to_vec()
                        },
                    },
                    Some("\"fixture-v1\"".into()),
                ))
            })
        }
    }
    #[test]
    fn pr_wait_etag_survives_store_restart() {
        let d = crate::testsupport::dir("pr-wait-etag");
        let endpoint = Endpoint(Mutex::new(Vec::new()));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for _ in 0..2 {
            let store =
                Arc::new(Store::open(d.path().join("inbox.db"), d.path().join("blobs")).unwrap());
            let sweep = SweepTransport::new(&endpoint);
            let result = runtime
                .block_on(sweep.get(&store, "https://fake.invalid", "o/r/pulls/1", "fixture"))
                .unwrap();
            assert_eq!(result, json!({"fixture":true}));
        }
        let requests = endpoint.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].headers.get("If-None-Match").map(String::as_str),
            Some("\"fixture-v1\"")
        );
    }
}
