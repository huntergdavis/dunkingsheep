//! T35 review service adapters. These invoke the accepted `ReviewSync` engine;
//! only the service inputs are injected so tests can use a fake HTTP transport.

use serde_json::{json, Value};

use super::Flock;
use crate::error::DunkError;
use crate::github::{GitHubRequest, GitHubResponse, GitHubTransport, PinBoxFuture};
use crate::review::{
    ReviewFuture, ReviewPolicy, ReviewRepoConfig, ReviewRunContext, ReviewSync, ReviewTaskActor,
    ReviewTaskCreator, ReviewTaskRequest, ReviewTokenSource,
};

struct BorrowedReviewTokens<'a>(&'a dyn ReviewTokenSource);

impl ReviewTokenSource for BorrowedReviewTokens<'_> {
    fn reviewer_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        self.0.reviewer_token()
    }

    fn author_token(&self) -> ReviewFuture<'_, Result<String, String>> {
        self.0.author_token()
    }
}

pub(crate) struct BorrowedGitHubTransport<'a>(pub(crate) &'a dyn GitHubTransport);

impl GitHubTransport for BorrowedGitHubTransport<'_> {
    fn send<'a>(
        &'a self,
        request: GitHubRequest,
    ) -> PinBoxFuture<'a, Result<GitHubResponse, String>> {
        self.0.send(request)
    }
}

impl ReviewTaskActor for &Flock {
    fn set_accounting_pr<'a>(
        &'a self,
        id: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<(), String>> {
        <Flock as ReviewTaskActor>::set_accounting_pr(self, id, number)
    }

    fn note_with_meta<'a>(
        &'a self,
        id: &'a str,
        event: &'a str,
        actor: &'a str,
        meta: &'a Value,
    ) -> ReviewFuture<'a, Result<(), String>> {
        <Flock as ReviewTaskActor>::note_with_meta(self, id, event, actor, meta)
    }

    fn list_review_rows(
        &self,
    ) -> ReviewFuture<'_, Result<Vec<crate::tasks::ReviewTaskRow>, String>> {
        <Flock as ReviewTaskActor>::list_review_rows(self)
    }

    fn task_status<'a>(&'a self, id: &'a str) -> ReviewFuture<'a, Result<String, String>> {
        <Flock as ReviewTaskActor>::task_status(self, id)
    }

    fn cancel_review<'a>(
        &'a self,
        id: &'a str,
        reason: &'a str,
    ) -> ReviewFuture<'a, Result<(), DunkError>> {
        <Flock as ReviewTaskActor>::cancel_review(self, id, reason)
    }

    fn authors_of<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
    ) -> ReviewFuture<'a, Result<Vec<String>, String>> {
        <Flock as ReviewTaskActor>::authors_of(self, repo, number)
    }

    fn notes<'a>(
        &'a self,
        id: &'a str,
    ) -> ReviewFuture<'a, Result<Vec<crate::tasks::TaskNote>, String>> {
        <Flock as ReviewTaskActor>::notes(self, id)
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
        <Flock as ReviewTaskActor>::add_followup_task(self, title, body, author, repo, number, head)
    }

    fn full_task_wire<'a>(
        &'a self,
        id: &'a str,
    ) -> ReviewFuture<'a, Result<crate::tasks::wire::TaskWire, String>> {
        <Flock as ReviewTaskActor>::full_task_wire(self, id)
    }

    fn mark_landed_for_pr<'a>(
        &'a self,
        repo: &'a str,
        number: i64,
        by: &'a str,
    ) -> ReviewFuture<'a, Result<Vec<String>, String>> {
        <Flock as ReviewTaskActor>::mark_landed_for_pr(self, repo, number, by)
    }
}

impl ReviewTaskCreator for &Flock {
    fn add_review_task<'a>(
        &'a self,
        request: ReviewTaskRequest<'a>,
    ) -> ReviewFuture<'a, Result<Option<String>, String>> {
        <Flock as ReviewTaskCreator>::add_review_task(self, request)
    }
}

impl ReviewRunContext for &Flock {
    fn lane_labels(&self) -> Result<Vec<(String, String)>, String> {
        <Flock as ReviewRunContext>::lane_labels(self)
    }

    fn author_token_mints(&self) -> Result<Vec<(String, f64)>, String> {
        <Flock as ReviewRunContext>::author_token_mints(self)
    }
}

impl Flock {
    /// `Flock.review_sync`: run the canonical review engine with this Flock's
    /// attached task actor, role snapshot, and token-mint attribution.
    pub async fn review_sync<T, G, X>(
        &self,
        tokens: &T,
        github: G,
        api_root: &str,
        transport: X,
        dry_run: bool,
    ) -> Result<Value, String>
    where
        T: ReviewTokenSource,
        G: ReviewPolicy + ReviewRepoConfig,
        X: GitHubTransport,
    {
        ReviewSync::with_transport(
            self,
            BorrowedReviewTokens(tokens),
            github,
            api_root,
            transport,
        )
        .run(dry_run)
        .await
    }

    /// `Flock.review_list`: expose the Python review summary from one decorated
    /// task-list snapshot, keeping Python's stable oldest-first age ordering.
    pub async fn review_list(
        &self,
        self_pane_id: Option<&str>,
        now: f64,
    ) -> Result<Value, DunkError> {
        let tasks = self.task_store()?;
        let rows = tasks
            .list_visible_wire(
                None,
                None,
                None,
                false,
                Some(500),
                self_pane_id,
                Some("review"),
                false,
            )
            .await?;

        let mut reviews: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let wire = row.wire;
                let reviewer = if row.typed.claimed_by.is_empty() {
                    row.typed.assignees
                } else {
                    row.typed.claimed_by
                };
                let age_s = (now - wire["created_at"].as_f64().unwrap_or_default())
                    .round_ties_even() as i64;
                json!({
                    "task": wire["id"].clone(),
                    "status": wire["status"].clone(),
                    "repo": wire.get("repo").cloned().unwrap_or(Value::Null),
                    "pr": wire.get("pr").cloned().unwrap_or(Value::Null),
                    "head": wire.get("head").and_then(Value::as_str).unwrap_or_default().chars().take(12).collect::<String>(),
                    "author": wire.get("author").cloned().unwrap_or(Value::Null),
                    "reviewer": reviewer,
                    "age_s": age_s,
                    "verdict": wire.get("verdict").cloned().unwrap_or(Value::Null),
                    "title": wire["title"].clone(),
                })
            })
            .collect();
        reviews.sort_by(|left, right| right["age_s"].as_i64().cmp(&left["age_s"].as_i64()));
        let count = reviews.len();
        let oldest_age_s = reviews
            .first()
            .and_then(|row| row.get("age_s"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(json!({
            "reviews": reviews,
            "count": count,
            "oldest_age_s": oldest_age_s,
        }))
    }
}

impl Flock {
    /// `_after_review_verdict`: invoke the accepted review engine after durable
    /// task completion. GitHub/service errors are logged and returned without
    /// undoing the recorded verdict.
    #[allow(clippy::too_many_arguments)]
    pub async fn after_review_verdict<T, G, X>(
        &self,
        tokens: &T,
        github: G,
        api_root: &str,
        transport: X,
        task: &Value,
        verdict: &str,
        by: Option<&str>,
    ) -> Value
    where
        T: ReviewTokenSource,
        G: ReviewPolicy,
        X: GitHubTransport,
    {
        let sync = ReviewSync::with_transport(
            self,
            BorrowedReviewTokens(tokens),
            github,
            api_root,
            transport,
        );
        match sync.on_verdict(task, verdict, by).await {
            Ok(result) => result,
            Err(error) => {
                let task_id = task
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| "None".into());
                eprintln!("review verdict side effects failed for {task_id}: {error}");
                json!({"errors": [error]})
            }
        }
    }
}

#[cfg(test)]
#[path = "commands_review_tests.rs"]
mod tests;
