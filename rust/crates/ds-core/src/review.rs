//! `review` (T52): pure identity, attribution, row-selection, and accounting helpers
//! recovered from `dunk_review.py`.
//!
//! These functions operate on supplied task, PR, and ledger data. Service calls and
//! production writes remain outside this module. The accounting gates below take the
//! table text read by Python and preserve its refusal text byte-for-byte.

use serde_json::Value;
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use crate::flock::Flock;
use crate::github::{GitHubTransport, StatusUpdate};
use crate::tasks::{AddOptions, ReviewTaskRow, TaskNote, Tasks};

#[derive(Clone, Copy)]
struct AccountingLedgerConfig {
    ledger: &'static str,
    base: &'static str,
    docs_dir: &'static str,
    partial_table: &'static str,
    kind_table: &'static str,
}

fn accounting_ledger(repo: &str) -> Option<AccountingLedgerConfig> {
    (repo == "huntergdavis/poolrad-decomp").then_some(AccountingLedgerConfig {
        ledger: "docs/re/strict_full_c_addresses.csv",
        base: "main",
        docs_dir: "docs/re/",
        partial_table: "docs/re/partial_callee_models.csv",
        kind_table: "docs/re/credit_evidence_kind.csv",
    })
}

pub type ReviewPrKey = (Option<Value>, Option<Value>);
pub type JudgedHeadKey = (Option<Value>, Option<Value>, Option<Value>);
pub type FinalVerdict = (bool, Vec<String>, Vec<String>, usize);
pub type ReviewFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The exact task query used by the review timer, injectable for deterministic tests.
pub trait ReviewTaskStore {
    type Error;

    fn list(
        &self,
        scope: Option<&str>,
        include_done: bool,
        limit: usize,
        kind: &str,
    ) -> Result<Vec<Value>, Self::Error>;
}

/// Async actor operations needed by the source-faithful ReviewSync methods.
/// Production uses `Tasks`; tests inject a recording actor without opening a live DB.
pub trait ReviewTaskActor {
    fn set_accounting_pr<'a>(
        &'a self,
        id: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<(), String>>;

    fn note_with_meta<'a>(
        &'a self,
        id: &'a str,
        event: &'a str,
        actor: &'a str,
        meta: &'a Value,
    ) -> ReviewFuture<'a, Result<(), String>>;

    fn list_review_rows(&self) -> ReviewFuture<'_, Result<Vec<ReviewTaskRow>, String>>;

    fn task_status<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<String, String>>;

    fn cancel_review<'a>(
        &'a self,
        id: &'a str,
        reason: &'a str,
    ) -> ReviewFuture<'a, Result<(), crate::error::DunkError>>;

    fn authors_of<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<Vec<String>, String>>;

    fn notes<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<Vec<TaskNote>, String>>;

    fn add_followup_task<'a>(
        &'a self,
        title: &'a str,
        body: &'a str,
        author: Option<&'a str>,
        repo: &'a str,
        number: i64,
        head: &'a str,
    ) -> ReviewFuture<'a, Result<String, String>>;
}

impl ReviewTaskActor for Tasks {
    fn set_accounting_pr<'a>(
        &'a self,
        id: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<(), String>> {
        Box::pin(async move {
            Tasks::set_accounting_pr(self, id, number)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn note_with_meta<'a>(
        &'a self,
        id: &'a str,
        event: &'a str,
        actor: &'a str,
        meta: &'a Value,
    ) -> ReviewFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let reason = meta.get("reason").and_then(Value::as_str);
            Tasks::note_with_meta(self, id, event, actor, reason, meta)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn list_review_rows(&self) -> ReviewFuture<'_, Result<Vec<ReviewTaskRow>, String>> {
        Box::pin(async move {
            Tasks::list_review_rows_visible(self)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn task_status<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<String, String>> {
        Box::pin(async move {
            Tasks::get(self, id)
                .await
                .map(|task| task.status)
                .map_err(|error| error.to_string())
        })
    }

    fn cancel_review<'a>(
        &'a self,
        id: &'a str,
        reason: &'a str,
    ) -> ReviewFuture<'a, Result<(), crate::error::DunkError>> {
        Box::pin(async move { Tasks::cancel(self, id, reason).await.map(|_| ()) })
    }

    fn authors_of<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            Tasks::authors_of(self, repo, number)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn notes<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<Vec<TaskNote>, String>> {
        Box::pin(async move {
            Tasks::notes(self, id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn add_followup_task<'a>(
        &'a self,
        title: &'a str,
        body: &'a str,
        author: Option<&'a str>,
        repo: &'a str,
        number: i64,
        head: &'a str,
    ) -> ReviewFuture<'a, Result<String, String>> {
        Box::pin(async move {
            Tasks::add_with(
                self,
                "global",
                title,
                body,
                AddOptions {
                    priority: Some("high".into()),
                    assignees: author
                        .filter(|author| !author.is_empty())
                        .map(str::to_owned)
                        .into_iter()
                        .collect(),
                    created_by: Some("review-sync".into()),
                    kind: Some("work".into()),
                    author: author
                        .filter(|author| !author.is_empty())
                        .map(str::to_owned),
                    repo: Some(repo.into()),
                    pr: Some(number.to_string()),
                    head: Some(head.into()),
                    ..Default::default()
                },
            )
            .await
            .map(|task| task.id)
            .map_err(|error| error.to_string())
        })
    }
}

fn attached_review_tasks(flock: &Flock) -> Result<std::sync::Arc<Tasks>, crate::error::DunkError> {
    flock
        .stores()
        .tasks
        .ok_or_else(|| crate::error::DunkError::Refused("task store is not attached".into()))
}

impl ReviewTaskActor for Flock {
    fn set_accounting_pr<'a>(
        &'a self,
        id: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<(), String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::set_accounting_pr(&tasks, id, number)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn note_with_meta<'a>(
        &'a self,
        id: &'a str,
        event: &'a str,
        actor: &'a str,
        meta: &'a Value,
    ) -> ReviewFuture<'a, Result<(), String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            let reason = meta.get("reason").and_then(Value::as_str);
            Tasks::note_with_meta(&tasks, id, event, actor, reason, meta)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn list_review_rows(&self) -> ReviewFuture<'_, Result<Vec<ReviewTaskRow>, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::list_review_rows_visible(&tasks)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn task_status<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<String, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::get(&tasks, id)
                .await
                .map(|task| task.status)
                .map_err(|error| error.to_string())
        })
    }

    fn cancel_review<'a>(
        &'a self,
        id: &'a str,
        reason: &'a str,
    ) -> ReviewFuture<'a, Result<(), crate::error::DunkError>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks?;
            Tasks::cancel(tasks.as_ref(), id, reason).await.map(|_| ())
        })
    }

    fn authors_of<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<Vec<String>, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::authors_of(&tasks, repo, number)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn notes<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<Vec<TaskNote>, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::notes(&tasks, id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn add_followup_task<'a>(
        &'a self,
        title: &'a str,
        body: &'a str,
        author: Option<&'a str>,
        repo: &'a str,
        number: i64,
        head: &'a str,
    ) -> ReviewFuture<'a, Result<String, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            Tasks::add_with(
                tasks.as_ref(),
                "global",
                title,
                body,
                AddOptions {
                    priority: Some("high".into()),
                    assignees: author
                        .filter(|author| !author.is_empty())
                        .map(str::to_owned)
                        .into_iter()
                        .collect(),
                    created_by: Some("review-sync".into()),
                    kind: Some("work".into()),
                    author: author
                        .filter(|author| !author.is_empty())
                        .map(str::to_owned),
                    repo: Some(repo.into()),
                    pr: Some(number.to_string()),
                    head: Some(head.into()),
                    ..Default::default()
                },
            )
            .await
            .map(|task| task.id)
            .map_err(|error| error.to_string())
        })
    }
}

impl ReviewRunContext for Flock {
    fn lane_labels(&self) -> Result<Vec<(String, String)>, String> {
        let mut stores = self
            .role_stores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let roles = stores
            .roles
            .as_mut()
            .ok_or_else(|| "no role registry is attached to this flock".to_string())?;
        let described = roles.describe().map_err(|error| error.to_string())?;
        let lanes = described
            .get("lanes")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|lanes| lanes.iter())
            .map(|(pane, entry)| {
                let label = entry
                    .get("label")
                    .filter(|value| python_truthy(value))
                    .map(python_string)
                    .unwrap_or_default();
                (pane.clone(), label)
            })
            .collect();
        Ok(lanes)
    }

    fn author_token_mints(&self) -> Result<Vec<(String, f64)>, String> {
        let mints = self
            .gh_token_mints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(mints.iter().map(|(pane, at)| (pane.clone(), *at)).collect())
    }
}

/// The concrete actor seam for filing the next review task through `Tasks::add_with`.
/// The production implementation below always uses the real writer actor.
pub trait ReviewTaskCreator {
    fn add_review_task<'a>(
        &'a self,
        request: ReviewTaskRequest<'a>,
    ) -> ReviewFuture<'a, Result<Option<String>, String>>;
}

pub struct ReviewTaskRequest<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub author: Option<&'a str>,
    pub repo: &'a str,
    pub number: i64,
    pub head: &'a str,
    pub expected_verdict_ids: &'a [String],
}

/// Runtime inputs read from the existing owner/Flock sources for one timer pass.
pub trait ReviewRunContext {
    fn lane_labels(&self) -> Result<Vec<(String, String)>, String>;
    fn author_token_mints(&self) -> Result<Vec<(String, f64)>, String>;
}

/// Repo config and ledger-backed mint attribution used by the production timer.
pub trait ReviewRepoConfig {
    fn repos(&self) -> Result<Vec<String>, String>;
    fn author_from_mints(&self, since: f64) -> Result<Option<String>, String>;
}

/// Real config-backed adapter used by a production ReviewSync instance.
#[derive(Debug, Clone)]
pub struct ConfiguredReviewPolicy {
    pub config_dir: std::path::PathBuf,
}

impl ReviewPolicy for ConfiguredReviewPolicy {
    fn required_verdicts(&self, repo: &str) -> Result<usize, String> {
        crate::github::required_verdicts(repo, None).map_err(|error| error.to_string())
    }
}

impl ReviewRepoConfig for ConfiguredReviewPolicy {
    fn repos(&self) -> Result<Vec<String>, String> {
        Ok(crate::github::repos_config(&self.config_dir))
    }

    fn author_from_mints(&self, since: f64) -> Result<Option<String>, String> {
        crate::github::author_from_mints(Some(since), None, None, &self.config_dir)
            .map_err(|error| error.to_string())
    }
}

/// Token interface for the real GitHub token cache and deterministic tests.
pub trait ReviewTokenSource {
    fn reviewer_token(&self) -> ReviewFuture<'_, Result<String, String>>;

    fn author_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        Box::pin(async { Err("author token source is unavailable".into()) })
    }
}

/// The merge/accounting operation supplied by the adjacent accepted ReviewSync merge slice.
/// The timer owns selection and terminal guards; this seam owns only the existing merge path.
pub trait ReviewRunMerge {
    fn merge<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
        head: &'a str,
        token: &'a str,
        task_id: &'a str,
        report: &'a mut Value,
    ) -> ReviewFuture<'a, Result<(), String>>;
}

impl ReviewTokenSource for crate::github::Tokens {
    fn reviewer_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let token = self
                .get("reviewer", None)
                .await
                .map_err(|error| error.to_string())?;
            token
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "reviewer token response has no token".to_string())
        })
    }

    fn author_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let token = self
                .get("author", None)
                .await
                .map_err(|error| error.to_string())?;
            token
                .get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "author token response has no token".to_string())
        })
    }
}

impl ReviewTaskCreator for Tasks {
    fn add_review_task<'a>(
        &'a self,
        request: ReviewTaskRequest<'a>,
    ) -> ReviewFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            self.add_review_with_verdict_guard(
                "global",
                request.title,
                request.body,
                AddOptions {
                    priority: Some("high".into()),
                    kind: Some("review".into()),
                    author: request.author.map(str::to_owned),
                    repo: Some(request.repo.into()),
                    pr: Some(request.number.to_string()),
                    head: Some(request.head.into()),
                    created_by: Some("review-sync".into()),
                    ..Default::default()
                },
                request.expected_verdict_ids,
            )
            .await
            .map(|task| task.map(|task| task.id))
            .map_err(|error| error.to_string())
        })
    }
}

impl ReviewTaskCreator for Flock {
    fn add_review_task<'a>(
        &'a self,
        request: ReviewTaskRequest<'a>,
    ) -> ReviewFuture<'a, Result<Option<String>, String>> {
        let tasks = attached_review_tasks(self);
        Box::pin(async move {
            let tasks = tasks.map_err(|error| error.to_string())?;
            tasks
                .add_review_with_verdict_guard(
                    "global",
                    request.title,
                    request.body,
                    AddOptions {
                        priority: Some("high".into()),
                        kind: Some("review".into()),
                        author: request.author.map(str::to_owned),
                        repo: Some(request.repo.into()),
                        pr: Some(request.number.to_string()),
                        head: Some(request.head.into()),
                        created_by: Some("review-sync".into()),
                        ..Default::default()
                    },
                    request.expected_verdict_ids,
                )
                .await
                .map(|task| task.map(|task| task.id))
                .map_err(|error| error.to_string())
        })
    }
}

/// The one GitHub policy query this pure review slice needs. Production callers
/// can adapt the API/config source; tests supply a fixture implementation.
pub trait ReviewPolicy {
    fn required_verdicts(&self, repo: &str) -> Result<usize, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPull {
    pub repo: String,
    pub number: i64,
    pub head: String,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub base: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDetail {
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub body: Option<String>,
}

/// Rust counterpart of `Sync`: dependencies are supplied at construction so
/// these helpers never create credentials, make requests, or touch a live store.
pub struct ReviewSync<S, T, G, X = ()> {
    pub flock: S,
    pub tokens: T,
    pub github: G,
    pub transport: X,
    pub api_root: String,
}

impl<S, G: Default> ReviewSync<S, (), G, ()> {
    /// Missing tokens and GitHub adapters use their injected defaults, matching
    /// Python's `tokens or Tokens()` and `github=dunk_github` constructor values.
    pub fn new(flock: S, github: Option<G>) -> Self {
        Self {
            flock,
            tokens: (),
            github: github.unwrap_or_default(),
            transport: (),
            api_root: crate::github::DEFAULT_API.to_string(),
        }
    }
}

impl<S, T, G> ReviewSync<S, T, G, ()> {
    /// Construct with an explicitly supplied token source.
    pub fn with_tokens(flock: S, tokens: T, github: G) -> Self {
        Self {
            flock,
            tokens,
            github,
            transport: (),
            api_root: crate::github::DEFAULT_API.to_string(),
        }
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X> {
    /// Supply the production HTTP transport and API root explicitly.
    pub fn with_transport(
        flock: S,
        tokens: T,
        github: G,
        api_root: impl Into<String>,
        transport: X,
    ) -> Self {
        Self {
            flock,
            tokens,
            github,
            transport,
            api_root: api_root.into(),
        }
    }
}

impl<S: ReviewTaskStore, T, G: ReviewPolicy> ReviewSync<S, T, G> {
    /// Build the live review map, retaining Python's task query and last-value
    /// replacement behavior while keeping first-insertion order.
    pub fn _live_reviews(&self) -> Result<Vec<(ReviewPrKey, Value)>, S::Error> {
        let tasks = self.flock.list(None, true, usize::MAX, "review")?;
        let mut by_pr: Vec<(ReviewPrKey, Value)> = Vec::new();
        for task in tasks {
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(status, "open" | "in_progress" | "blocked") {
                continue;
            }
            let key = (optional_value(&task, "repo"), optional_value(&task, "pr"));
            insert_replacing(&mut by_pr, key, task);
        }
        Ok(by_pr)
    }

    /// Build the already-judged map. A done review with a falsey verdict is not
    /// a judgment; duplicate keys keep the last task as Python's dict does.
    pub fn _judged_heads(&self) -> Result<Vec<(JudgedHeadKey, Value)>, S::Error> {
        let tasks = self.flock.list(None, true, usize::MAX, "review")?;
        let mut out: Vec<(JudgedHeadKey, Value)> = Vec::new();
        for task in tasks {
            if task.get("status").and_then(Value::as_str) != Some("done")
                || !task.get("verdict").is_some_and(python_truthy)
            {
                continue;
            }
            let key = (
                optional_value(&task, "repo"),
                optional_value(&task, "pr"),
                optional_value(&task, "head"),
            );
            insert_replacing(&mut out, key, task);
        }
        Ok(out)
    }

    /// Distinct completed reviewer lanes at one repo/head, with an optional PR
    /// filter. Returned lists use Python's sorted-set order.
    pub fn head_verdicts(
        &self,
        repo: &str,
        head: &str,
        number: Option<i64>,
    ) -> Result<(Vec<String>, Vec<String>), S::Error> {
        let tasks = self.flock.list(None, true, usize::MAX, "review")?;
        let mut accepts = BTreeSet::new();
        let mut needs_fix = BTreeSet::new();
        for task in tasks {
            if task.get("repo").and_then(Value::as_str) != Some(repo)
                || task.get("head").and_then(Value::as_str) != Some(head)
                || number.is_some_and(|n| task.get("pr").and_then(Value::as_i64) != Some(n))
                || task.get("status").and_then(Value::as_str) != Some("done")
            {
                continue;
            }
            let Some(lane) = task.get("completed_by").filter(|lane| python_truthy(lane)) else {
                continue;
            };
            let lane = python_string(lane);
            match task.get("verdict").and_then(Value::as_str) {
                Some("accept") => {
                    accepts.insert(lane);
                }
                Some("needs-fix") => {
                    needs_fix.insert(lane);
                }
                _ => {}
            }
        }
        Ok((
            accepts.into_iter().collect(),
            needs_fix.into_iter().collect(),
        ))
    }

    /// Whether a head is final and the distinct-lane evidence used to decide.
    pub fn _is_final(
        &self,
        repo: &str,
        head: &str,
        number: Option<i64>,
        lane: Option<&str>,
    ) -> Result<FinalVerdict, String>
    where
        S::Error: std::fmt::Display,
    {
        let (accepts, needs_fix) = self
            .head_verdicts(repo, head, number)
            .map_err(|error| error.to_string())?;
        let required = self.github.required_verdicts(repo)?.max(1);
        Ok(final_verdict(accepts, needs_fix, lane, required))
    }
}

fn final_verdict(
    mut accepts: Vec<String>,
    needs_fix: Vec<String>,
    lane: Option<&str>,
    required: usize,
) -> FinalVerdict {
    if let Some(lane) = lane.filter(|lane| !lane.is_empty()) {
        if accepts
            .binary_search_by(|candidate| candidate.as_str().cmp(lane))
            .is_err()
        {
            accepts.push(lane.to_string());
            accepts.sort();
        }
    }
    let is_final = needs_fix.is_empty() && accepts.len() >= required.max(1);
    (is_final, accepts, needs_fix, required.max(1))
}

fn verdict_lanes_from_rows(
    rows: &[ReviewTaskRow],
    repo: &str,
    head: &str,
    number: Option<i64>,
) -> (Vec<String>, Vec<String>) {
    let mut accepts = BTreeSet::new();
    let mut needs_fix = BTreeSet::new();
    for row in rows {
        if row.repo.as_deref() != Some(repo)
            || row.head.as_deref() != Some(head)
            || number.is_some_and(|number| row.pr != Some(number))
            || row.status != "done"
        {
            continue;
        }
        let Some(lane) = row.completed_by.as_deref().filter(|lane| !lane.is_empty()) else {
            continue;
        };
        match row.verdict.as_deref() {
            Some("accept") => {
                accepts.insert(lane.to_string());
            }
            Some("needs-fix") => {
                needs_fix.insert(lane.to_string());
            }
            _ => {}
        }
    }
    (
        accepts.into_iter().collect(),
        needs_fix.into_iter().collect(),
    )
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    X: GitHubTransport,
{
    /// Return whether GitHub reports a conflict; unknown mergeability remains `None`.
    pub async fn conflicting(
        &self,
        repo: &str,
        number: i64,
        token: &str,
        report: Option<&mut Value>,
    ) -> Option<bool> {
        let detail =
            match crate::github::pull_detail(&self.transport, &self.api_root, repo, number, token)
                .await
            {
                Ok(detail) => detail,
                Err(error) => {
                    if let Some(report) = report {
                        append_report(
                            report,
                            "errors",
                            Value::String(format!("mergeable {repo}#{number}: {error}")),
                        );
                    }
                    return None;
                }
            };
        if !detail.is_object() {
            return None;
        }
        let mergeable = detail.get("mergeable");
        let state = detail
            .get("mergeable_state")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if mergeable == Some(&Value::Bool(false)) || state == "dirty" {
            return Some(true);
        }
        if mergeable == Some(&Value::Bool(true))
            || matches!(state.as_str(), "clean" | "blocked" | "unstable" | "behind")
        {
            return Some(false);
        }
        None
    }

    /// Post the review status and record the source-compatible success/error report.
    pub async fn _status(&self, token: &str, update: StatusUpdate<'_>, report: &mut Value) {
        let short_sha: String = update.sha.chars().take(12).collect();
        let (repo, state) = (update.repo, update.state);
        match crate::github::post_status(&self.transport, &self.api_root, token, update).await {
            Ok(_) => append_report(
                report,
                "statuses",
                serde_json::json!({"repo": repo, "head": short_sha, "state": state}),
            ),
            Err(error) => append_report(
                report,
                "errors",
                Value::String(format!("status {repo}@{short_sha}: {error}")),
            ),
        }
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor,
{
    /// Render the accounting PR body exactly as `Sync._accounting_body` does.
    pub fn _accounting_body(
        &self,
        task: &Value,
        ident: &str,
        doc: &str,
        ledger: &str,
        number: i64,
        by: Option<&str>,
    ) -> String {
        let by = by.filter(|value| !value.is_empty()).unwrap_or("reviewer");
        let head = task
            .get("head")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let id = task
            .get("id")
            .map(python_string)
            .unwrap_or_else(|| "None".into());
        format!(
            "Accounting row for the accepted body {ident}.\n\n- body PR (the reviewed content): #{number}\n- review task: {id}, verdict `accept` by {by}\n- head reviewed: {head}\n- ledger: `{ledger}`\n- row added: `{ident},{doc}`\n\nDerived deterministically from the verdict notes and the files the body PR\nadds: one row id, one evidence document that carries its unit and offset. No\nmodel decided anything here. The reviewed content is the body PR; this PR only\nrecords the credit.\n\nOpened by the review timer as `dunkingsheep-reviewer[bot]` and approved as\n`dunkingsheep-author[bot]` so the two identities GitHub counts are distinct.\nAuto-merge is enabled; branch protection decides when it lands."
        )
    }

    /// Record accounting metadata after awaiting the column write and then the note.
    /// Failures are returned to the caller for logging; they never mutate a verdict.
    pub async fn _record_accounting(&self, task: &Value, out: &Value) -> Option<String> {
        self.try_record_accounting(task, out).await.err()
    }

    async fn try_record_accounting(&self, task: &Value, out: &Value) -> Result<(), String> {
        let task_id = task
            .get("id")
            .map(python_string)
            .ok_or_else(|| "review task has no id".to_string())?;
        let accounting_pr = out
            .get("accounting_pr")
            .filter(|value| python_truthy(value));
        if let Some(number) = accounting_pr {
            let number = number
                .as_i64()
                .ok_or_else(|| "accounting PR number is not an integer".to_string())?;
            self.flock.set_accounting_pr(&task_id, number).await?;
        }
        let body_pr = out
            .get("body_pr")
            .map(python_string)
            .unwrap_or_else(|| "None".into());
        let summary = if let Some(number) = accounting_pr {
            format!(
                "accounting PR #{} for body PR #{body_pr}",
                python_string(number)
            )
        } else {
            let reason = out
                .get("reason")
                .filter(|value| python_truthy(value))
                .map(python_string)
                .unwrap_or_else(|| {
                    out.get("errors")
                        .filter(|value| python_truthy(value))
                        .and_then(Value::as_array)
                        .map(|errors| {
                            errors
                                .iter()
                                .map(python_string)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default()
                });
            format!("no accounting PR for body PR #{body_pr}: {reason}")
        };
        let meta = serde_json::json!({
            "reason": summary,
            "accounting_pr": out.get("accounting_pr").cloned().unwrap_or(Value::Null),
            "body_pr": out.get("body_pr").cloned().unwrap_or(Value::Null),
            "row": out.get("row").cloned().unwrap_or(Value::Null),
            "url": out.get("url").cloned().unwrap_or(Value::Null),
            "state": out.get("state").cloned().unwrap_or(Value::Null),
            "errors": out.get("errors").filter(|value| python_truthy(value)).cloned().unwrap_or_else(|| serde_json::json!([])),
        });
        self.flock
            .note_with_meta(&task_id, "accounting", "review-sync", &meta)
            .await
    }

    /// First matching, different, completed ACCEPT for the exact review identity.
    pub async fn _standing_accept(&self, task: &Value) -> Option<String> {
        let id = task.get("id").filter(|value| python_truthy(value))?;
        let repo = task.get("repo").filter(|value| python_truthy(value))?;
        let pr = task.get("pr").filter(|value| python_truthy(value))?;
        let head = task.get("head").filter(|value| python_truthy(value))?;
        let rows = self.flock.list_review_rows().await.ok()?;
        rows.into_iter().find_map(|other| {
            (other.id != python_string(id)
                && other.status == "done"
                && other.verdict.as_deref() == Some("accept")
                && other.repo.as_deref() == repo.as_str()
                && other.pr == pr.as_i64()
                && other.head.as_deref() == head.as_str())
            .then_some(other.id)
        })
    }

    /// Join the reviewer's notes or return the source-compatible fallback sentence.
    pub async fn _evidence(&self, task: &Value) -> String {
        let id = task.get("id").map(python_string).unwrap_or_default();
        let notes = match self.flock.notes(&id).await {
            Ok(notes) => notes,
            Err(_) => return "(no note store)".into(),
        };
        let text = notes
            .into_iter()
            .map(|note| note.text.trim().to_string())
            .filter(|note| !note.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            "(no evidence was recorded on the review task)".into()
        } else {
            text
        }
    }

    /// Render the needs-fix body with its supplied reviewer evidence.
    pub fn _review_body(
        &self,
        task: &Value,
        by: Option<&str>,
        head: &str,
        evidence: &str,
    ) -> String {
        format!(
            "Needs fix per {} (reviewer lane {}) at head {head}.\n\nThe reviewer's evidence, verbatim:\n\n{evidence}\n\nA verdict is bound to this head: a new push files a new review.",
            task.get("id").map(python_string).unwrap_or_else(|| "None".into()),
            by.filter(|value| !value.is_empty()).unwrap_or("?")
        )
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor,
{
    /// Join all note text on the review task in event order; a missing note store is empty.
    pub async fn _verdict_text(&self, task: &Value) -> String {
        let id = task.get("id").map(python_string).unwrap_or_default();
        self.flock
            .notes(&id)
            .await
            .map(|notes| {
                notes
                    .into_iter()
                    .filter(|note| !note.text.is_empty())
                    .map(|note| note.text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor,
    T: ReviewTokenSource,
    X: GitHubTransport,
{
    /// Open and queue the accounting PR for an accepted body, refusing any row whose
    /// evidence is ambiguous, incomplete, already credited, or not yet on the base branch.
    pub async fn account(
        &self,
        task: &Value,
        by: Option<&str>,
        dry_run: bool,
    ) -> Result<Value, String> {
        let repo = task
            .get("repo")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let body_pr = task.get("pr").cloned().unwrap_or(Value::Null);
        let mut out = serde_json::json!({
            "repo": task.get("repo").cloned().unwrap_or(Value::Null),
            "body_pr": body_pr,
            "ledger": null,
            "row": null,
            "accounting_pr": null,
            "url": null,
            "review": null,
            "auto_merge": null,
            "state": null,
            "reason": null,
            "errors": []
        });
        let Some(config) = accounting_ledger(&repo) else {
            out["reason"] = Value::String(format!(
                "no accounting ledger is configured for {}",
                if repo.is_empty() { "this repo" } else { &repo }
            ));
            return Ok(out);
        };
        out["ledger"] = Value::String(config.ledger.into());

        // Preserve the source order: both Apps are obtained before any GitHub request.
        let reviewer = match self.tokens.reviewer_token().await {
            Ok(token) => token,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("tokens: {error}")),
                );
                return Ok(out);
            }
        };
        let author = match self.tokens.author_token().await {
            Ok(token) => token,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("tokens: {error}")),
                );
                return Ok(out);
            }
        };

        let base_sha = match crate::github::get_ref(
            &self.transport,
            &self.api_root,
            &repo,
            config.base,
            &reviewer,
        )
        .await
        {
            Ok(sha) => sha,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("read {}@{}: {error}", config.ledger, config.base)),
                );
                return Ok(out);
            }
        };
        let (ledger_text, _) = match crate::github::file_text(
            &self.transport,
            &self.api_root,
            &repo,
            config.ledger,
            &base_sha,
            &reviewer,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("read {}@{}: {error}", config.ledger, config.base)),
                );
                return Ok(out);
            }
        };
        let Some(ledger_text) = ledger_text else {
            append_report(
                &mut out,
                "errors",
                Value::String(format!(
                    "{repo}/{} does not exist on {}",
                    config.ledger, config.base
                )),
            );
            return Ok(out);
        };

        let Some(number) = body_pr.as_i64() else {
            return Err("pr must be a number".into());
        };
        let files = match crate::github::pr_files(
            &self.transport,
            &self.api_root,
            &repo,
            number,
            &reviewer,
        )
        .await
        {
            Ok(files) => files,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("list PR #{number} files: {error}")),
                );
                return Ok(out);
            }
        };
        let added_docs: Vec<String> = files
            .iter()
            .filter_map(|item| {
                let filename = item.get("filename").and_then(Value::as_str)?;
                (item.get("status").and_then(Value::as_str) == Some("added")
                    && filename.starts_with(config.docs_dir)
                    && filename.ends_with(".md"))
                .then(|| filename.to_string())
            })
            .collect();
        let detail =
            crate::github::pull_detail(&self.transport, &self.api_root, &repo, number, &reviewer)
                .await
                .unwrap_or(Value::Null);
        let notes = self._verdict_text(task).await;
        let title = task
            .get("title")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let detail_body = detail
            .get("body")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let added_refs = added_docs.iter().map(String::as_str).collect::<Vec<_>>();
        let (row, reason) = choose_row(
            &[
                ("verdict notes", Some(&notes)),
                ("review task title", Some(&title)),
                ("PR body", Some(&detail_body)),
            ],
            &added_refs,
            Some(&ledger_text),
        );
        let Some((ident, doc)) = row else {
            out["reason"] = Value::String(reason);
            let _ = self._record_accounting(task, &out).await;
            return Ok(out);
        };
        out["row"] = Value::String(format!("{ident},{doc}"));

        let (partial_text, _) = match crate::github::file_text(
            &self.transport,
            &self.api_root,
            &repo,
            config.partial_table,
            &base_sha,
            &reviewer,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!(
                        "read {}@{}: {error}",
                        config.partial_table, config.base
                    )),
                );
                let _ = self._record_accounting(task, &out).await;
                return Ok(out);
            }
        };
        if let Some(reason) = partial_text
            .as_deref()
            .and_then(|text| partial_gate(&ident, config.partial_table, text))
        {
            out["reason"] = Value::String(reason);
            let _ = self._record_accounting(task, &out).await;
            return Ok(out);
        }

        let (kind_text, _) = match crate::github::file_text(
            &self.transport,
            &self.api_root,
            &repo,
            config.kind_table,
            &base_sha,
            &reviewer,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!(
                        "read {}@{}: {error}",
                        config.kind_table, config.base
                    )),
                );
                (None, None)
            }
        };
        if let Some(kind_text) = kind_text.as_deref() {
            if !labelled_ids(kind_text).contains(&ident) {
                out["reason"] = Value::String(format!(
                    "{ident} has no row in {}, which every strict credit needs; that table is generated by tools/overlay/credit_evidence_kind.py --write. The automation does not guess the label - regenerate the table in the accounting branch and re-run.",
                    config.kind_table
                ));
                let _ = self._record_accounting(task, &out).await;
                return Ok(out);
            }
        }

        let (doc_text, _) = match crate::github::file_text(
            &self.transport,
            &self.api_root,
            &repo,
            &doc,
            &base_sha,
            &reviewer,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("read {doc}@{}: {error}", config.base)),
                );
                let _ = self._record_accounting(task, &out).await;
                return Ok(out);
            }
        };
        if doc_text.is_none() {
            out["reason"] = Value::String(format!(
                "the evidence document {doc} does not exist on {}: the body PR #{number} is not merged, so this row would credit a document that is not there (t833)",
                config.base
            ));
            let _ = self._record_accounting(task, &out).await;
            return Ok(out);
        }

        let branch = format!("accounting/pr{number}-{ident}");
        let branch_exists = match crate::github::ref_exists(
            &self.transport,
            &self.api_root,
            &repo,
            &branch,
            &reviewer,
        )
        .await
        {
            Ok(exists) => exists,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("look up {branch}: {error}")),
                );
                return Ok(out);
            }
        };
        if branch_exists {
            let open_pulls = match crate::github::pulls_for_head(
                &self.transport,
                &self.api_root,
                &repo,
                &branch,
                &reviewer,
            )
            .await
            {
                Ok(pulls) => pulls,
                Err(error) => {
                    append_report(
                        &mut out,
                        "errors",
                        Value::String(format!("look up {branch}: {error}")),
                    );
                    return Ok(out);
                }
            };
            if let Some(open) = open_pulls.first() {
                out["accounting_pr"] = open.get("number").cloned().unwrap_or(Value::Null);
                out["url"] = open.get("url").cloned().unwrap_or(Value::Null);
                out["reason"] =
                    Value::String(format!("an accounting PR for {ident} is already open"));
                let _ = self._record_accounting(task, &out).await;
                return Ok(out);
            }
            out["reason"] = Value::String(format!(
                "branch {branch} exists without an open PR; refusing to overwrite a commit nobody reviewed"
            ));
            let _ = self._record_accounting(task, &out).await;
            return Ok(out);
        }

        let task_id = task
            .get("id")
            .map(python_string)
            .ok_or_else(|| "task has no id".to_string())?;
        let task_head = task
            .get("head")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let author_lane = by.filter(|value| !value.is_empty()).unwrap_or("reviewer");
        let message = format!(
            "Accept {ident}: accounting row for #{number}\n\nDerived from review {task_id} (verdict accept by {author_lane}) and PR #{number} at head {}.\nThe reviewed content is PR #{number}; this commit only adds {ident},{doc} to {}.\n",
            task_head.chars().take(12).collect::<String>(),
            config.ledger
        );
        let new_text = with_row(Some(&ledger_text), &ident, &doc).map_err(str::to_owned)?;
        if dry_run {
            out["reason"] = Value::String("dry run: no commit, no PR".into());
            return Ok(out);
        }

        let creation = async {
            let base_tree = crate::github::commit_tree(
                &self.transport,
                &self.api_root,
                &repo,
                &base_sha,
                &reviewer,
            )
            .await?;
            let tree = crate::github::create_tree(
                &self.transport,
                &self.api_root,
                &repo,
                &base_tree,
                &[serde_json::json!({
                    "path": config.ledger,
                    "mode": "100644",
                    "type": "blob",
                    "content": new_text,
                })],
                &reviewer,
            )
            .await?;
            let commit = crate::github::create_commit(
                &self.transport,
                &self.api_root,
                &repo,
                &message,
                &tree,
                std::slice::from_ref(&base_sha),
                &reviewer,
            )
            .await?;
            crate::github::create_ref(
                &self.transport,
                &self.api_root,
                &repo,
                &branch,
                &commit,
                &reviewer,
            )
            .await?;
            Ok::<(), crate::github::GitHubError>(())
        }
        .await;
        if let Err(error) = creation {
            append_report(
                &mut out,
                "errors",
                Value::String(format!("commit the ledger row: {error}")),
            );
            let _ = self._record_accounting(task, &out).await;
            return Ok(out);
        }

        let accounting_title = format!("Accept {ident}: accounting row for #{number}");
        let accounting_body = self._accounting_body(task, &ident, &doc, config.ledger, number, by);
        let opened = match crate::github::open_pull(
            &self.transport,
            &self.api_root,
            crate::github::OpenPullRequest {
                repo: &repo,
                title: &accounting_title,
                head: &branch,
                base: config.base,
                body: &accounting_body,
            },
            &reviewer,
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => {
                append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("open the accounting PR: {error}")),
                );
                let _ = self._record_accounting(task, &out).await;
                return Ok(out);
            }
        };
        let accounting_number = opened
            .get("number")
            .and_then(Value::as_i64)
            .ok_or_else(|| "'number'".to_string())?;
        out["accounting_pr"] = Value::from(accounting_number);
        out["url"] = opened.get("url").cloned().unwrap_or(Value::Null);
        let accounting_head = task_head.chars().take(12).collect::<String>();
        match crate::github::review_pull(
            &self.transport,
            &self.api_root,
            &repo,
            accounting_number,
            "APPROVE",
            &format!(
                "Accounting row derived from review {task_id} and the accepted content in #{number} at {accounting_head}."
            ),
            &author,
        )
        .await
        {
            Ok(_) => out["review"] = Value::String("approved_by_author_app".into()),
            Err(error) => append_report(
                &mut out,
                "errors",
                Value::String(format!("approve as the author App: {error}")),
            ),
        }
        match crate::github::enable_auto_merge(
            &self.transport,
            &self.api_root,
            &repo,
            accounting_number,
            &author,
            None,
        )
        .await
        {
            Ok(_) => out["auto_merge"] = Value::String("enabled".into()),
            Err(error) => append_report(
                &mut out,
                "errors",
                Value::String(format!("enable auto-merge: {error}")),
            ),
        }
        match crate::github::pull_state(
            &self.transport,
            &self.api_root,
            &repo,
            accounting_number,
            &author,
        )
        .await
        {
            Ok(state) => out["state"] = state,
            Err(error) => append_report(
                &mut out,
                "errors",
                Value::String(format!("read the accounting PR state: {error}")),
            ),
        }
        let _ = self._record_accounting(task, &out).await;
        Ok(out)
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor + ReviewTaskCreator,
    G: ReviewPolicy,
{
    /// File the distinct second review task for an accepted exact head.
    pub async fn _second_review(
        &self,
        repo: &str,
        number: i64,
        head: &str,
        first_lane: &str,
        report: Option<&mut Value>,
    ) -> Option<String> {
        let verdict_rows = match self.flock.list_review_rows().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| {
                    row.status == "done"
                        && row
                            .verdict
                            .as_deref()
                            .is_some_and(|verdict| !verdict.is_empty())
                        && row.repo.as_deref() == Some(repo)
                        && row.pr == Some(number)
                        && row.head.as_deref() == Some(head)
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                if let Some(report) = report {
                    append_report(
                        report,
                        "errors",
                        Value::String(format!("second review {repo}#{number}: {error}")),
                    );
                }
                return None;
            }
        };
        let mut accepts = BTreeSet::new();
        let mut needs_fix = BTreeSet::new();
        for row in &verdict_rows {
            let Some(lane) = row.completed_by.as_deref().filter(|lane| !lane.is_empty()) else {
                continue;
            };
            match row.verdict.as_deref() {
                Some("accept") => {
                    accepts.insert(lane.to_string());
                }
                Some("needs-fix") => {
                    needs_fix.insert(lane.to_string());
                }
                _ => {}
            }
        }
        let repository = repo.rsplit('/').next().unwrap_or(repo);
        let short_head: String = head.chars().take(8).collect();
        let title: String = format!(
            "Review {repository} PR #{number} {short_head} (second reviewer: {first_lane} accepted)"
        )
        .chars()
        .take(120)
        .collect();
        let required = match self.github.required_verdicts(repo) {
            Ok(required) => required.max(1),
            Err(error) => {
                if let Some(report) = report {
                    append_report(
                        report,
                        "errors",
                        Value::String(format!("second review {repo}#{number}: {error}")),
                    );
                }
                return None;
            }
        };
        if !needs_fix.is_empty() || accepts.len() >= required.max(1) {
            return None;
        }
        let body = format!(
            "SECOND REVIEW {repo}#{number} @ {head}\nLane {first_lane} already accepted this exact head. {required} distinct reviewer lanes are required, so this task exists for a DIFFERENT lane; the claim filter keeps {first_lane} off it.\n\nVerdict: `dunkingsheep task complete <id> --verdict accept|needs-fix` with the evidence in `task note` first."
        );
        let expected_verdict_ids = verdict_rows
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        match self
            .flock
            .add_review_task(ReviewTaskRequest {
                title: &title,
                body: &body,
                author: Some(first_lane),
                repo,
                number,
                head,
                expected_verdict_ids: &expected_verdict_ids,
            })
            .await
        {
            Ok(id) => id,
            Err(error) => {
                if let Some(report) = report {
                    append_report(
                        report,
                        "errors",
                        Value::String(format!("second review {repo}#{number}: {error}")),
                    );
                }
                None
            }
        }
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor + ReviewTaskCreator,
    T: ReviewTokenSource,
    G: ReviewPolicy,
    X: GitHubTransport,
{
    /// Apply a completed review verdict: status, approval/changes request, and follow-up.
    pub async fn on_verdict(
        &self,
        task: &Value,
        verdict: &str,
        by: Option<&str>,
    ) -> Result<Value, String> {
        let mut out = serde_json::json!({
            "task": task.get("id").cloned().unwrap_or(Value::Null),
            "verdict": verdict,
            "status": null,
            "review": null,
            "followup": null,
            "accounting": null,
            "errors": []
        });
        let repo_value = task.get("repo").filter(|value| python_truthy(value));
        let number_value = task.get("pr").filter(|value| python_truthy(value));
        let head_value = task.get("head").filter(|value| python_truthy(value));
        let (Some(repo_value), Some(number_value), Some(head_value)) =
            (repo_value, number_value, head_value)
        else {
            return Ok(out);
        };
        let repo = python_string(repo_value);
        let Some(number) = number_value.as_i64() else {
            return Ok(out);
        };
        let head = python_string(head_value);
        let token = match self.tokens.reviewer_token().await {
            Ok(token) => token,
            Err(error) => {
                append_report(&mut out, "errors", Value::String(error));
                return Ok(out);
            }
        };

        if verdict == "accept" {
            let rows = self.flock.list_review_rows().await?;
            let (accepts, needs_fix) = verdict_lanes_from_rows(&rows, &repo, &head, Some(number));
            let required = self.github.required_verdicts(&repo)?.max(1);
            let (final_accept, accepts, needs_fix, required) =
                final_verdict(accepts, needs_fix, by, required);
            out["accepts"] = serde_json::json!(accepts);
            out["required"] = Value::from(required as u64);

            if !needs_fix.is_empty() && !final_accept {
                let description = format!("needs-fix stands from {}", needs_fix.join(", "));
                match crate::github::post_status(
                    &self.transport,
                    &self.api_root,
                    &token,
                    StatusUpdate {
                        repo: &repo,
                        sha: &head,
                        state: "failure",
                        description: &description,
                        target_url: None,
                    },
                )
                .await
                {
                    Ok(_) => out["status"] = Value::String("failure".into()),
                    Err(error) => append_report(
                        &mut out,
                        "errors",
                        Value::String(format!("status: {error}")),
                    ),
                }
                return Ok(out);
            }

            if !final_accept {
                let description = format!(
                    "accepted by {} of {required} distinct reviewer lanes ({}) - second review filed",
                    accepts.len(),
                    accepts.join(", ")
                );
                match crate::github::post_status(
                    &self.transport,
                    &self.api_root,
                    &token,
                    StatusUpdate {
                        repo: &repo,
                        sha: &head,
                        state: "pending",
                        description: &description,
                        target_url: None,
                    },
                )
                .await
                {
                    Ok(_) => out["status"] = Value::String("pending".into()),
                    Err(error) => append_report(
                        &mut out,
                        "errors",
                        Value::String(format!("status: {error}")),
                    ),
                }
                let second = self
                    ._second_review(
                        &repo,
                        number,
                        &head,
                        by.filter(|lane| !lane.is_empty()).unwrap_or(""),
                        None,
                    )
                    .await;
                out["second_review"] = second.clone().map(Value::String).unwrap_or(Value::Null);
                out["merged"] = Value::Bool(false);
                out["held"] = serde_json::json!({
                    "reason": "not final: a second DISTINCT reviewer lane must accept",
                    "accepts": accepts,
                    "required": required,
                    "second_review": second,
                    "postponed": ["approval", "merge", "accounting"],
                });
                let reason = format!(
                    "accepted by {} of {required} distinct reviewer lanes ({}); a second distinct lane must accept before the approval, the merge and the accounting row are written",
                    accepts.len(),
                    accepts.join(", ")
                );
                let held_meta = serde_json::json!({
                    "reason": reason,
                    "second_review": second,
                    "head": head,
                });
                let held_task_id = task
                    .get("id")
                    .map(python_string)
                    .unwrap_or_else(|| "None".into());
                let _ = self
                    .flock
                    .note_with_meta(&held_task_id, "verdict_held", "review-sync", &held_meta)
                    .await;
                return Ok(out);
            }

            let description = format!(
                "accepted by {} distinct reviewer lanes ({})",
                accepts.len(),
                accepts.join(", ")
            );
            match crate::github::post_status(
                &self.transport,
                &self.api_root,
                &token,
                StatusUpdate {
                    repo: &repo,
                    sha: &head,
                    state: "success",
                    description: &description,
                    target_url: None,
                },
            )
            .await
            {
                Ok(_) => out["status"] = Value::String("success".into()),
                Err(error) => append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("status: {error}")),
                ),
            }
            let task_id = python_string(task.get("id").unwrap_or(&Value::Null));
            let review_body = format!(
                "Accepted in {task_id} by reviewer lane {} at head {head}.",
                by.filter(|lane| !lane.is_empty()).unwrap_or("?")
            );
            match crate::github::review_pull(
                &self.transport,
                &self.api_root,
                &repo,
                number,
                "APPROVE",
                &review_body,
                &token,
            )
            .await
            {
                Ok(_) => out["review"] = Value::String("approved".into()),
                Err(error) => append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("approve: {error}")),
                ),
            }
            match crate::github::merge_pull(
                &self.transport,
                &self.api_root,
                &repo,
                number,
                &token,
                Some(&head),
            )
            .await
            {
                Ok(_) => out["merged"] = Value::Bool(true),
                Err(error) => {
                    out["merged"] = Value::Bool(false);
                    if !error.to_string().contains("405") {
                        append_report(&mut out, "errors", Value::String(format!("merge: {error}")));
                    }
                }
            }
            if out["merged"] == Value::Bool(true) {
                match self.account(task, by, false).await {
                    Ok(accounting) => out["accounting"] = accounting,
                    Err(error) => out["accounting"] = serde_json::json!({"errors": [error]}),
                }
            } else {
                out["accounting"] = serde_json::json!({
                    "skipped": "the body PR is not merged yet; the accounting row is written by the sync that observes the merge (t835)"
                });
            }
            return Ok(out);
        }

        let evidence = self._evidence(task).await;
        let standing = self._standing_accept(task).await;
        if let Some(standing) = standing {
            out["status"] = Value::String("skipped_standing_accept".into());
            out["standing_accept"] = Value::String(standing.clone());
            append_report(
                &mut out,
                "errors",
                Value::String(format!(
                    "failure status not posted: {repo}#{number} @ {} already carries an accept verdict from {standing}",
                    head.chars().take(12).collect::<String>()
                )),
            );
        } else {
            let description = format!(
                "needs-fix per {}",
                task.get("id")
                    .map(python_string)
                    .unwrap_or_else(|| "None".into())
            );
            match crate::github::post_status(
                &self.transport,
                &self.api_root,
                &token,
                StatusUpdate {
                    repo: &repo,
                    sha: &head,
                    state: "failure",
                    description: &description,
                    target_url: None,
                },
            )
            .await
            {
                Ok(_) => out["status"] = Value::String("failure".into()),
                Err(error) => append_report(
                    &mut out,
                    "errors",
                    Value::String(format!("status: {error}")),
                ),
            }
        }
        let request_changes_body = self._review_body(task, by, &head, &evidence);
        match crate::github::review_pull(
            &self.transport,
            &self.api_root,
            &repo,
            number,
            "REQUEST_CHANGES",
            &request_changes_body,
            &token,
        )
        .await
        {
            Ok(_) => out["review"] = Value::String("changes_requested".into()),
            Err(error) => append_report(
                &mut out,
                "errors",
                Value::String(format!("request-changes: {error}")),
            ),
        }
        let task_id = task
            .get("id")
            .map(python_string)
            .unwrap_or_else(|| "None".into());
        let title = format!(
            "Fix {} PR #{number} per {task_id}",
            repo.rsplit('/').next().unwrap_or(&repo)
        );
        let body = format!(
            "Reviewer verdict needs-fix on {repo}#{number} @ {head}.\n\nThe reviewer's evidence, verbatim:\n\n{evidence}\n\nPush a fix to the same branch; the review timer files a new review for the new head."
        );
        let author = task
            .get("author")
            .filter(|value| python_truthy(value))
            .map(python_string);
        match self
            .flock
            .add_followup_task(&title, &body, author.as_deref(), &repo, number, &head)
            .await
        {
            Ok(id) => out["followup"] = Value::String(id),
            Err(error) => append_report(
                &mut out,
                "errors",
                Value::String(format!("followup: {error}")),
            ),
        }
        Ok(out)
    }
}

impl<S, T, G, X> ReviewSync<S, T, G, X>
where
    S: ReviewTaskActor + ReviewTaskCreator + ReviewRunContext,
    T: ReviewTokenSource,
    G: ReviewPolicy + ReviewRepoConfig,
    X: GitHubTransport,
{
    /// Run the deterministic ReviewSync sweep against the injected real task, token,
    /// repository, identity, and transport sources. Final merges are delegated to the
    /// adjacent merge/accounting slice through `ReviewRunMerge`.
    pub async fn run<M: ReviewRunMerge>(&self, merger: &M, dry_run: bool) -> Result<Value, String> {
        let mut report = serde_json::json!({
            "repos": [], "filed": [], "cancelled": [], "statuses": [],
            "followups": [], "conflicted": [], "errors": [], "merged": []
        });
        let token = match self.tokens.reviewer_token().await {
            Ok(token) => token,
            Err(error) => {
                append_report(
                    &mut report,
                    "errors",
                    Value::String(format!("reviewer token: {error}")),
                );
                return Ok(report);
            }
        };
        let lanes = self
            .flock
            .lane_labels()
            .map_err(|error| format!("review lane snapshot: {error}"))?;
        let mints = self
            .flock
            .author_token_mints()
            .map_err(|error| format!("author token mint snapshot: {error}"))?;
        let initial_rows = self.flock.list_review_rows().await?;
        let mut live = Vec::new();
        for row in initial_rows {
            if matches!(row.status.as_str(), "open" | "in_progress" | "blocked") {
                let Some(repo) = row.repo.as_ref() else {
                    continue;
                };
                let Some(number) = row.pr else {
                    continue;
                };
                insert_replacing(&mut live, (repo.clone(), number), row);
            }
        }

        let repos = self.github.repos()?;
        let mut seen = BTreeSet::new();
        let mut listed = BTreeSet::new();
        for repo in repos {
            let pulls =
                match crate::github::open_pulls(&self.transport, &self.api_root, &repo, &token)
                    .await
                {
                    Ok(pulls) => pulls,
                    Err(error) => {
                        append_report(
                            &mut report,
                            "errors",
                            Value::String(format!("{repo}: {error}")),
                        );
                        continue;
                    }
                };
            listed.insert(repo.clone());
            append_report(
                &mut report,
                "repos",
                serde_json::json!({"repo": repo, "open": pulls.len()}),
            );
            for raw_pr in pulls {
                if raw_pr.get("draft").is_some_and(python_truthy) {
                    continue;
                }
                let Some(pr) = parse_review_pull(&repo, &raw_pr) else {
                    continue;
                };
                if pr.head.is_empty() {
                    continue;
                }

                let conflict = self
                    .conflicting(&repo, pr.number, &token, Some(&mut report))
                    .await;
                if conflict == Some(true) {
                    append_report(
                        &mut report,
                        "conflicted",
                        serde_json::json!({
                            "repo": repo,
                            "pr": pr.number,
                            "head": pr.head.chars().take(12).collect::<String>()
                        }),
                    );
                    continue;
                }

                let key = (repo.clone(), pr.number);
                seen.insert(key.clone());
                let mut current = live
                    .iter()
                    .find(|(candidate, _)| candidate == &key)
                    .map(|(_, row)| row.clone());
                // This read is deliberately after the GitHub conflict request. A verdict
                // may have committed while GitHub was answering.
                let latest_rows = self.flock.list_review_rows().await?;
                let terminal_verdict_rows = terminal_rows(&latest_rows, &repo, pr.number, &pr.head);
                let done = terminal_verdict_rows.last().cloned();

                // Cancel a stale head before handling the current head's verdict. The live
                // review uniqueness constraint otherwise blocks the required second review.
                if current
                    .as_ref()
                    .is_some_and(|row| row.head.as_deref() != Some(pr.head.as_str()))
                {
                    if let Some(stale) = current.take() {
                        let reason = format!(
                            "head moved to {}; delta review filed",
                            pr.head.chars().take(12).collect::<String>()
                        );
                        let cancelled = if dry_run {
                            true
                        } else {
                            self.cancel_review_if_live(&stale.id, &reason).await?
                        };
                        if cancelled {
                            append_report(&mut report, "cancelled", Value::String(stale.id));
                        }
                    }
                }

                if let Some(done_row) = done.as_ref() {
                    if let Some(current_row) = current.as_ref() {
                        if current_row.head.as_deref() == Some(pr.head.as_str()) && !dry_run {
                            let (accepts, needs_fix) = verdict_lanes(&terminal_verdict_rows);
                            let required = self.github.required_verdicts(&repo)?.max(1);
                            let final_verdict = needs_fix.is_empty() && accepts.len() >= required;
                            let still_needed = done_row.verdict.as_deref() == Some("accept")
                                && !final_verdict
                                && needs_fix.is_empty();
                            if still_needed
                                && !matches!(
                                    self.flock.task_status(&current_row.id).await?.as_str(),
                                    "done" | "cancelled"
                                )
                            {
                                continue;
                            }
                            let reason = format!("duplicate of judged review {}", done_row.id);
                            if self.cancel_review_if_live(&current_row.id, &reason).await? {
                                append_report(
                                    &mut report,
                                    "cancelled",
                                    Value::String(current_row.id.clone()),
                                );
                            }
                            current = None;
                        }
                    }
                }

                if current
                    .as_ref()
                    .is_some_and(|row| row.head.as_deref() == Some(pr.head.as_str()))
                {
                    continue;
                }

                if let Some(done) = done {
                    if done.verdict.as_deref() == Some("accept") && !dry_run {
                        let (accepts, needs_fix) = verdict_lanes(&terminal_verdict_rows);
                        let required = self.github.required_verdicts(&repo)?.max(1);
                        let final_verdict = needs_fix.is_empty() && accepts.len() >= required;
                        if final_verdict {
                            merger
                                .merge(&repo, pr.number, &pr.head, &token, &done.id, &mut report)
                                .await?;
                        } else if needs_fix.is_empty() && current.is_none() {
                            if let Some(first_lane) = accepts.first() {
                                let _ = self
                                    ._second_review(
                                        &repo,
                                        pr.number,
                                        &pr.head,
                                        first_lane,
                                        Some(&mut report),
                                    )
                                    .await;
                            }
                        }
                    }
                    continue;
                }

                let mut author =
                    attribute_author_at(Some(&pr.branch), pr.created_at.as_deref(), &lanes, &mints);
                if author.is_none() {
                    author = author_from_tasks(self.flock.authors_of(&repo, pr.number).await);
                }
                if author.is_none() {
                    author = self
                        .github
                        .author_from_mints(crate::clock::now_s() - 1800.0)?;
                }
                let detail = self
                    .raw_pull_detail(&repo, pr.number, &token)
                    .await
                    .unwrap_or(Value::Null);
                let detail = ReviewDetail {
                    additions: detail.get("additions").and_then(Value::as_i64),
                    deletions: detail.get("deletions").and_then(Value::as_i64),
                    changed_files: detail.get("changed_files").and_then(Value::as_i64),
                    body: detail
                        .get("body")
                        .filter(|value| python_truthy(value))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                };
                let title: String = format!(
                    "Review {} PR #{} {}: {}",
                    repo.rsplit('/').next().unwrap_or(&repo),
                    pr.number,
                    pr.head.chars().take(8).collect::<String>(),
                    pr.title
                )
                .chars()
                .take(120)
                .collect();
                if dry_run {
                    append_report(
                        &mut report,
                        "filed",
                        serde_json::json!({
                            "repo": repo,
                            "pr": pr.number,
                            "head": pr.head,
                            "author": author
                        }),
                    );
                    continue;
                }

                let latest_rows = self.flock.list_review_rows().await?;
                if !terminal_rows(&latest_rows, &repo, pr.number, &pr.head).is_empty() {
                    continue;
                }
                let expected_verdict_ids = Vec::new();
                let body = review_body(&pr, &detail, author.as_deref());
                let id = match self
                    .flock
                    .add_review_task(ReviewTaskRequest {
                        title: &title,
                        body: &body,
                        author: author.as_deref(),
                        repo: &repo,
                        number: pr.number,
                        head: &pr.head,
                        expected_verdict_ids: &expected_verdict_ids,
                    })
                    .await
                {
                    Ok(Some(id)) => id,
                    Ok(None) => continue,
                    Err(error) => {
                        append_report(
                            &mut report,
                            "errors",
                            Value::String(format!("{repo}#{}: {error}", pr.number)),
                        );
                        continue;
                    }
                };
                append_report(
                    &mut report,
                    "filed",
                    serde_json::json!({
                        "task": id,
                        "repo": repo,
                        "pr": pr.number,
                        "head": pr.head,
                        "author": author
                    }),
                );
                let description = format!("review task {id} open");
                self._status(
                    &token,
                    StatusUpdate {
                        repo: &repo,
                        sha: &pr.head,
                        state: "pending",
                        description: &description,
                        target_url: Some(&pr.url),
                    },
                    &mut report,
                )
                .await;
            }
        }

        for ((repo, number), row) in &live {
            if seen.contains(&(repo.clone(), *number)) || !listed.contains(repo) || dry_run {
                continue;
            }
            let detail = match self.raw_pull_detail(repo, *number, &token).await {
                Ok(detail) => detail,
                Err(error) => {
                    append_report(
                        &mut report,
                        "errors",
                        Value::String(format!("{repo}#{number} cleanup: {error}")),
                    );
                    continue;
                }
            };
            let state = detail.get("state");
            if state.and_then(Value::as_str) == Some("open") {
                continue;
            }
            if !matches!(state.and_then(Value::as_str), Some("closed" | "merged")) {
                append_report(
                    &mut report,
                    "errors",
                    Value::String(format!(
                        "{repo}#{number} cleanup: unknown PR state {}",
                        python_repr_value(state)
                    )),
                );
                continue;
            }
            if self
                .cancel_review_if_live(&row.id, "PR closed or merged on GitHub")
                .await?
            {
                append_report(&mut report, "cancelled", Value::String(row.id.clone()));
            }
        }
        Ok(report)
    }

    async fn raw_pull_detail(&self, repo: &str, number: i64, token: &str) -> Result<Value, String> {
        let url = format!("{}/repos/{repo}/pulls/{number}", self.api_root);
        crate::github::request(
            &self.transport,
            crate::github::RequestOptions {
                method: "GET",
                url: &url,
                token: Some(token),
                bearer: None,
                payload: None,
                timeout_s: 20,
                api_root: &self.api_root,
            },
        )
        .await
        .map(|(_, pull)| pull.unwrap_or(Value::Null))
        .map_err(|error| error.to_string())
    }

    async fn cancel_review_if_live(&self, id: &str, reason: &str) -> Result<bool, String> {
        let status = self.flock.task_status(id).await?;
        if matches!(status.as_str(), "done" | "cancelled") {
            return Ok(false);
        }
        match self.flock.cancel_review(id, reason).await {
            Ok(()) => Ok(true),
            Err(crate::error::DunkError::Refused(error)) => {
                let status = self.flock.task_status(id).await?;
                if matches!(status.as_str(), "done" | "cancelled") {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn parse_review_pull(repo: &str, value: &Value) -> Option<ReviewPull> {
    let number = value.get("number")?.as_i64()?;
    let get_text = |key: &str| {
        value
            .get(key)
            .filter(|item| python_truthy(item))
            .map(python_string)
    };
    Some(ReviewPull {
        repo: repo.to_string(),
        number,
        head: get_text("head").unwrap_or_default(),
        title: get_text("title").unwrap_or_default(),
        url: get_text("url").unwrap_or_default(),
        branch: get_text("branch").unwrap_or_default(),
        base: get_text("base").unwrap_or_default(),
        created_at: get_text("created_at"),
        updated_at: get_text("updated_at"),
    })
}

fn terminal_rows(
    rows: &[ReviewTaskRow],
    repo: &str,
    number: i64,
    head: &str,
) -> Vec<ReviewTaskRow> {
    rows.iter()
        .filter(|row| {
            row.status == "done"
                && row
                    .verdict
                    .as_deref()
                    .is_some_and(|verdict| !verdict.is_empty())
                && row.repo.as_deref() == Some(repo)
                && row.pr == Some(number)
                && row.head.as_deref() == Some(head)
        })
        .cloned()
        .collect()
}

fn verdict_lanes(rows: &[ReviewTaskRow]) -> (Vec<String>, Vec<String>) {
    let mut accepts = BTreeSet::new();
    let mut needs_fix = BTreeSet::new();
    for row in rows {
        let Some(lane) = row.completed_by.as_deref().filter(|lane| !lane.is_empty()) else {
            continue;
        };
        match row.verdict.as_deref() {
            Some("accept") => {
                accepts.insert(lane.to_string());
            }
            Some("needs-fix") => {
                needs_fix.insert(lane.to_string());
            }
            _ => {}
        }
    }
    (
        accepts.into_iter().collect(),
        needs_fix.into_iter().collect(),
    )
}

fn attribute_author_at(
    branch: Option<&str>,
    created_at: Option<&str>,
    lanes: &[(String, String)],
    mints: &[(String, f64)],
) -> Option<String> {
    let branch = branch.unwrap_or_default().to_lowercase();
    for (pane, label) in lanes {
        let label = label.trim().to_lowercase();
        if !label.is_empty()
            && (branch.starts_with(&format!("{label}/"))
                || branch.starts_with(&format!("{label}-")))
        {
            return Some(pane.clone());
        }
    }
    let created = _iso(created_at)? as f64;
    let recent = mints
        .iter()
        .filter(|(_, minted)| *minted <= created && created - *minted < 2.0 * 60.0 * 60.0)
        .map(|(pane, _)| pane)
        .collect::<Vec<_>>();
    (recent.len() == 1).then(|| recent[0].clone())
}

fn python_repr_value(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(value)) => {
            format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(value) => value.to_string(),
    }
}

fn append_report(report: &mut Value, key: &str, value: Value) {
    let Some(object) = report.as_object_mut() else {
        return;
    };
    if let Some(items) = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
    {
        items.push(value);
    }
}

fn optional_value(task: &Value, key: &str) -> Option<Value> {
    task.get(key).filter(|value| !value.is_null()).cloned()
}

fn insert_replacing<K: PartialEq, V>(entries: &mut Vec<(K, V)>, key: K, value: V) {
    if let Some(index) = entries.iter().position(|(existing, _)| existing == &key) {
        entries[index].1 = value;
    } else {
        entries.push((key, value));
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64() != Some(0) && value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

/// Python counterpart of `review_body`; truncation counts Unicode characters,
/// matching Python string slicing rather than byte slicing.
pub fn review_body(pr: &ReviewPull, detail: &ReviewDetail, author: Option<&str>) -> String {
    let author = author
        .filter(|author| !author.is_empty())
        .unwrap_or("unknown (not attributable; say so in the verdict)");
    let mut lines = vec![
        format!("REVIEW {}#{} @ {}", pr.repo, pr.number, pr.head),
        format!("title: {}", pr.title),
        format!("url: {}", pr.url),
        format!("branch: {} -> {}", pr.branch, pr.base),
        format!("author lane: {author}"),
        format!(
            "opened: {}  updated: {}",
            pr.created_at.as_deref().unwrap_or("None"),
            pr.updated_at.as_deref().unwrap_or("None")
        ),
        format!(
            "diff: +{} -{} in {} files",
            detail
                .additions
                .map_or_else(|| "None".to_string(), |n| n.to_string()),
            detail
                .deletions
                .map_or_else(|| "None".to_string(), |n| n.to_string()),
            detail
                .changed_files
                .map_or_else(|| "None".to_string(), |n| n.to_string())
        ),
        String::new(),
        "Verdict: `dunkingsheep task complete <id> --verdict accept|needs-fix` from your"
            .to_string(),
        "reviewer lane, with the evidence in `task note` first: commands run, what was".to_string(),
        "reproduced from the media, what was NOT verified. A verdict is bound to this".to_string(),
        "exact head; a new push files a new review.".to_string(),
    ];
    if let Some(body) = detail
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        let excerpt = body.chars().take(4000).collect::<String>();
        lines.extend([String::new(), "PR body:".to_string(), excerpt]);
    }
    lines.join("\n")
}

/// Pane id of the lane that opened a PR, or `None` when it cannot be
/// attributed without guessing. `lanes` and `mints` preserve Python mapping
/// iteration order as `(pane id, value)` pairs.
pub fn attribute_author(
    branch: Option<&str>,
    created_at: Option<&str>,
    lanes: &[(String, String)],
    mints: &[(String, String)],
) -> Option<String> {
    let branch = branch.unwrap_or_default().to_lowercase();
    for (pane_id, label) in lanes {
        let label = label.trim().to_lowercase();
        if !label.is_empty()
            && (branch.starts_with(&format!("{label}/"))
                || branch.starts_with(&format!("{label}-")))
        {
            return Some(pane_id.clone());
        }
    }

    let created = _iso(created_at)?;
    let recent: Vec<&String> = mints
        .iter()
        .filter_map(|(pane_id, minted_at)| {
            let minted = _iso(Some(minted_at))?;
            (minted <= created && created - minted < 2 * 60 * 60).then_some(pane_id)
        })
        .collect();
    (recent.len() == 1).then(|| recent[0].clone())
}

/// Attribute a PR to the sole lane found in its work tasks. Store errors and
/// zero or multiple authors remain unattributed, as in the Python helper.
pub fn author_from_tasks<E>(panes: Result<Vec<String>, E>) -> Option<String> {
    let mut panes = panes.ok()?.into_iter();
    let pane = panes.next()?;
    panes.next().is_none().then_some(pane)
}

/// Parse the strict UTC timestamp format accepted by `time.strptime` in the
/// Python review helper and return Unix seconds.
pub fn _iso(value: Option<&str>) -> Option<i64> {
    let value = value?;
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|time| time.and_utc().timestamp())
}

/// Every full row id named in `text`, in order of first appearance, deduped.
pub fn row_ids(text: Option<&str>) -> Vec<String> {
    let bytes = text.unwrap_or_default().as_bytes();
    let mut ids = Vec::new();
    let mut index = 0;
    while index + 13 <= bytes.len() {
        let start = index;
        let kind = &bytes[start..start + 3];
        let shape = (kind == b"OVL" || kind == b"RES")
            && bytes[start + 3] == b'_'
            && bytes[start + 8] == b'_'
            && bytes[start + 4..start + 8]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[start + 9..start + 13]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[start + 4..start + 13]
                .iter()
                .all(|byte| !byte.is_ascii_lowercase());
        let before_ok = start == 0 || !is_row_word(bytes[start - 1]);
        let after = start + 13;
        let after_ok = after == bytes.len() || !is_row_word(bytes[after]);
        if shape && before_ok && after_ok {
            let ident = String::from_utf8(bytes[start..after].to_vec()).unwrap();
            if !ids.contains(&ident) {
                ids.push(ident);
            }
            index = after;
        } else {
            index += 1;
        }
    }
    ids
}

fn is_row_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The final path component with its last extension removed.
pub fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or_default();
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

/// Whether a row id is named by this evidence document's stem.
pub fn own_row(row: Option<&str>, stem: &str) -> bool {
    let Some(row) = row else { return false };
    let bytes = row.as_bytes();
    if bytes.len() != 13
        || !(bytes.starts_with(b"OVL_") || bytes.starts_with(b"RES_"))
        || bytes[8] != b'_'
        || !bytes[4..8].iter().all(u8::is_ascii_hexdigit)
        || !bytes[9..13].iter().all(u8::is_ascii_hexdigit)
        || bytes[4..13].iter().any(u8::is_ascii_lowercase)
    {
        return false;
    }
    let parts: Vec<String> = stem.to_lowercase().split('_').map(str::to_string).collect();
    let offset = row[9..13].to_lowercase();
    let unit = row[4..8].to_lowercase();
    let short_unit = format!("u{}", unit.trim_start_matches('0'));
    let short_unit = if short_unit == "u" {
        "u0".to_string()
    } else {
        short_unit
    };
    parts.contains(&offset) && (parts.contains(&short_unit) || parts.contains(&unit))
}

/// `(id, evidence_doc)` pairs from an `id,evidence_doc` ledger, header skipped.
pub fn ledger_rows(text: Option<&str>) -> Vec<(String, String)> {
    text.unwrap_or_default()
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let (ident, doc) = line.split_once(',')?;
            Some((ident.trim().to_string(), doc.trim().to_string()))
        })
        .collect()
}

/// Insert a ledger row in sorted position while preserving LF or CRLF endings.
pub fn with_row(text: Option<&str>, ident: &str, doc: &str) -> Result<String, &'static str> {
    let text = text.unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.is_empty() {
        return Err("the ledger is empty: no header line to keep");
    }
    let sep = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let header = lines.remove(0);
    let insertion = lines
        .partition_point(|line| line.split_once(',').map_or(line.as_str(), |(key, _)| key) < ident);
    lines.insert(insertion, format!("{ident},{doc}"));
    let mut result = std::iter::once(header)
        .chain(lines)
        .collect::<Vec<_>>()
        .join(sep);
    if text.ends_with('\n') || text.ends_with('\r') {
        result.push_str(sep);
    }
    Ok(result)
}

/// Select the single ledger row an accepted body earns, or return its refusal reason.
pub fn choose_row(
    sources: &[(&str, Option<&str>)],
    added_docs: &[&str],
    ledger_text: Option<&str>,
) -> (Option<(String, String)>, String) {
    let mut ident = None;
    for (label, text) in sources {
        let ids = row_ids(*text);
        if ids.len() == 1 {
            ident = Some(ids[0].clone());
            break;
        }
        if ids.len() > 1 {
            return (
                None,
                format!(
                    "the {label} names {} row ids ({}); one ledger row is one id",
                    ids.len(),
                    ids.join(", ")
                ),
            );
        }
    }
    let Some(ident) = ident else {
        return (
            None,
            "no row id (OVL_xxxx_yyyy / RES_xxxx_yyyy) is named in the verdict notes, the review task title or the PR body".to_string(),
        );
    };

    let mut existing = std::collections::BTreeMap::new();
    for (row, doc) in ledger_rows(ledger_text) {
        existing.insert(row, doc);
    }
    if let Some(doc) = existing.get(&ident) {
        return (None, format!("{ident} is already credited as {doc}"));
    }

    let matched: Vec<&str> = added_docs
        .iter()
        .copied()
        .filter(|doc| own_row(Some(&ident), &stem_of(doc)))
        .collect();
    match matched.as_slice() {
        [] => {
            (
                None,
                format!(
                "{ident}: no evidence document the PR adds carries its unit and offset (added: {})",
                if added_docs.is_empty() { "none".to_string() } else { added_docs.join(", ") }
            ),
            )
        }
        [doc] => (Some((ident, (*doc).to_string())), String::new()),
        _ => {
            let mut docs = matched;
            docs.sort_unstable();
            (
                None,
                format!(
                    "{ident}: {} added documents match it ({}); refusing to pick one",
                    docs.len(),
                    docs.join(", ")
                ),
            )
        }
    }
}

/// Ids whose STATUS column is `partial` in `partial_callee_models.csv`. The status is the
/// discriminator, not presence: on main 49 of the 50 `full` rows are already strict
/// credits, so only `partial` may be refused. The header line is skipped.
pub fn partial_ids(text: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        let name = fields[0].trim();
        let status = fields
            .get(1)
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
        if !name.is_empty() && status == "partial" {
            ids.insert(name.to_string());
        }
    }
    ids
}

/// Ids carrying an evidence-kind label in `credit_evidence_kind.csv`: the first column of
/// every non-blank line after the header.
pub fn labelled_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split(',').next().unwrap_or("").trim().to_string())
        .collect()
}

/// GATE 2: a partial chain is not a strict full-C credit (poolrad#1637 credited
/// OVL_0038_0775, a partial model stopped at +0058, into the strict ledger). Returns the
/// refusal text when `ident` is partial.
pub fn partial_gate(ident: &str, partial_table: &str, partial_text: &str) -> Option<String> {
    partial_ids(partial_text).contains(ident).then(|| {
        format!(
            "{ident} is listed as a PARTIAL model in {partial_table}; a strict \
             full-C credit is not owed for it. Credit it in the partial ledger \
             when its chain closes, or close the gap first."
        )
    })
}

/// GATE 3: the strict table also requires an evidence-kind label, and that table is
/// GENERATED, so the automation refuses and names the tool rather than inventing a label.
///
/// `kind_text` is `None` when the table could not be read; the Python then records the
/// read error and does NOT refuse (it fails open here), and so does this.
pub fn kind_gate(ident: &str, kind_table: &str, kind_text: Option<&str>) -> Option<String> {
    let text = kind_text?;
    if labelled_ids(text).contains(ident) {
        return None;
    }
    Some(format!(
        "{ident} has no row in {kind_table}, which every strict credit \
         needs; that table is generated by \
         tools/overlay/credit_evidence_kind.py --write. The automation does \
         not guess the label - regenerate the table in the accounting branch \
         and re-run."
    ))
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod review_tests;
