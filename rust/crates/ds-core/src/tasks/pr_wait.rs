//! t4704 slice 2: a task blocked with `--waiting-on pr:<repo>#<n>` returns to the queue when
//! that pull request merges, and is told once when it closes without merging.
//!
//! The store only records the outcome; review-sync observes the pull request and calls
//! [`Tasks::resolve_pr_wait`]. Reopening never merges, approves or assigns anything.

use rusqlite::{params, OptionalExtension};
use serde_json::json;

use super::block_kind::{pr_wait_ref, PR_CLOSED_PREFIX};
use super::Tasks;
use crate::db;
use crate::error::DunkError;

/// One blocked task waiting on a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrWait {
    pub id: String,
    pub repo: String,
    pub number: i64,
    /// The exact block reason read, so the outcome is applied only if it is still that block.
    pub reason: String,
    /// Durable generation of this block, including re-blocks with identical text.
    /// Legacy rows without a recorded transition use None until their next block.
    pub block_event_id: Option<i64>,
}

/// What review-sync saw of the pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrOutcome {
    Merged,
    ClosedUnmerged,
}

impl Tasks {
    /// Every blocked task whose reason is a live `waiting-pr:<repo>#<n>` wait, oldest first.
    /// A wait already marked closed-unmerged is not listed again.
    pub async fn blocked_pr_waits(&self) -> Result<Vec<PrWait>, DunkError> {
        self.writer
            .call(|conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, block_reason, (SELECT MAX(e.id) FROM task_events e \
                         WHERE e.task_id = tasks.id AND e.status_to = 'blocked') \
                         FROM tasks WHERE status = 'blocked' \
                         AND block_reason LIKE 'waiting-pr:%' ORDER BY created_at, id",
                    )
                    .map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    })
                    .map_err(db::map_sqlite)?;
                let mut waits = Vec::new();
                for row in rows {
                    let (id, reason, block_event_id) = row.map_err(db::map_sqlite)?;
                    if let Some((repo, number)) = pr_wait_ref(&reason) {
                        waits.push(PrWait {
                            id,
                            repo,
                            number,
                            reason,
                            block_event_id,
                        });
                    }
                }
                Ok(waits)
            })
            .await
    }

    /// Apply what review-sync observed. Returns whether anything was written: `false` when
    /// the task is no longer at the same block event with exactly `wait.reason` (someone unblocked, re-blocked
    /// or finished it meanwhile, or an earlier sweep already applied this outcome).
    ///
    /// * merged: the task is opened and an `unblocked` event says which pull request merged;
    /// * closed unmerged: the task STAYS blocked, its reason changes to
    ///   `waiting-pr-closed:<repo>#<n>: ...` (so no later sweep repeats this), and a note on
    ///   the task says the pull request closed without merging.
    pub async fn resolve_pr_wait(
        &self,
        wait: &PrWait,
        outcome: PrOutcome,
    ) -> Result<bool, DunkError> {
        let wait = wait.clone();
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                type Row = (String, Option<String>, Option<String>, Option<String>, Option<i64>);
                let found: Option<Row> = tx
                    .query_row(
                        "SELECT status, block_reason, scope, priority, \
                         (SELECT MAX(e.id) FROM task_events e WHERE e.task_id = tasks.id \
                          AND e.status_to = 'blocked') FROM tasks WHERE id = ?",
                        params![wait.id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?;
                let Some((status, reason, scope, priority, block_event_id)) = found else {
                    return Ok(false);
                };
                if status != "blocked" || reason.as_deref() != Some(wait.reason.as_str())
                    || block_event_id != wait.block_event_id {
                    return Ok(false);
                }
                let pr = format!("{}#{}", wait.repo, wait.number);
                match outcome {
                    PrOutcome::Merged => {
                        tx.execute(
                            "UPDATE tasks SET status = 'open', block_reason = NULL, updated_at = ? WHERE id = ?",
                            params![now, wait.id],
                        )
                        .map_err(db::map_sqlite)?;
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, meta) VALUES (?,?,?,?,?,?,?,?,?)",
                            params![wait.id, "unblocked", now, "system", scope, priority, "blocked", "open",
                                    json!({"reason": format!("auto: {pr} merged")}).to_string()],
                        )
                        .map_err(db::map_sqlite)?;
                    }
                    PrOutcome::ClosedUnmerged => {
                        let closed = format!(
                            "{PR_CLOSED_PREFIX}{pr}: the pull request closed without merging; unblock or re-block this task by hand"
                        );
                        tx.execute(
                            "UPDATE tasks SET block_reason = ?, updated_at = ? WHERE id = ?",
                            params![closed, now, wait.id],
                        )
                        .map_err(db::map_sqlite)?;
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, meta) VALUES (?,?,?,?,?,?,?)",
                            params![wait.id, "note", now, "system", scope, priority,
                                    json!({"reason": format!("{pr} closed without merging; this task waited on it and stays blocked (was: {})", wait.reason)}).to_string()],
                        )
                        .map_err(db::map_sqlite)?;
                    }
                }
                tx.commit().map_err(db::map_sqlite)?;
                Ok(true)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
        let dir = crate::testsupport::dir(&format!("pr-wait-{tag}"));
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        crate::testsupport::Scratch::new(tasks, dir)
    }

    async fn waiting(store: &Tasks, title: &str, reason: &str) -> String {
        let task = store.add("global", title, "").await.unwrap();
        store.block(&task.id, reason).await.unwrap();
        task.id
    }

    #[tokio::test]
    async fn only_live_pr_waits_are_listed_with_their_repo_and_number() {
        let store = store("list").await;
        let a = waiting(&store, "a", "waiting-pr:owner/repo#42: review").await;
        let b = waiting(&store, "b", "waiting-pr:other/thing#7:").await;
        waiting(&store, "release", "waiting-release: next daemon").await;
        waiting(&store, "free text", "waiting on PR 12").await;
        waiting(&store, "malformed", "waiting-pr:norepo").await;
        waiting(
            &store,
            "closed",
            "waiting-pr-closed:owner/repo#9: the pull request closed",
        )
        .await;
        let waits = store.blocked_pr_waits().await.unwrap();
        let seen: Vec<(&str, &str, i64)> = waits
            .iter()
            .map(|w| (w.id.as_str(), w.repo.as_str(), w.number))
            .collect();
        assert_eq!(
            seen,
            [
                (a.as_str(), "owner/repo", 42),
                (b.as_str(), "other/thing", 7)
            ]
        );
    }

    #[tokio::test]
    async fn a_merge_opens_the_task_once_and_records_which_pr() {
        let store = store("merged").await;
        let id = waiting(&store, "a", "waiting-pr:owner/repo#42: review").await;
        let wait = store.blocked_pr_waits().await.unwrap().remove(0);
        assert!(store
            .resolve_pr_wait(&wait, PrOutcome::Merged)
            .await
            .unwrap());
        let task = store.get(&id).await.unwrap();
        assert_eq!((task.status.as_str(), task.block_reason), ("open", None));
        // The same observation again writes nothing, and nothing is left to sweep.
        assert!(!store
            .resolve_pr_wait(&wait, PrOutcome::Merged)
            .await
            .unwrap());
        assert!(store.blocked_pr_waits().await.unwrap().is_empty());
        let notes = store.recent_notes(&id, 5).await.unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.event == "unblocked" && n.reason == "auto: owner/repo#42 merged"),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn a_close_without_merge_keeps_the_task_blocked_and_says_so_once() {
        let store = store("closed").await;
        let id = waiting(&store, "a", "waiting-pr:owner/repo#42: review").await;
        let wait = store.blocked_pr_waits().await.unwrap().remove(0);
        assert!(store
            .resolve_pr_wait(&wait, PrOutcome::ClosedUnmerged)
            .await
            .unwrap());
        let task = store.get(&id).await.unwrap();
        assert_eq!(task.status, "blocked");
        let reason = task.block_reason.unwrap();
        assert!(
            reason.starts_with("waiting-pr-closed:owner/repo#42: "),
            "{reason}"
        );
        assert_eq!(super::super::block_kind::kind_of(Some(&reason)), "pr");
        // Told once: a second sweep neither lists it nor writes again.
        assert!(store.blocked_pr_waits().await.unwrap().is_empty());
        assert!(!store
            .resolve_pr_wait(&wait, PrOutcome::ClosedUnmerged)
            .await
            .unwrap());
        let notes = store.recent_notes(&id, 10).await.unwrap();
        let told = notes
            .iter()
            .filter(|n| n.reason.contains("closed without merging"))
            .count();
        assert_eq!(told, 1, "{notes:?}");
    }

    #[tokio::test]
    async fn an_outcome_is_not_applied_to_a_task_that_moved_on() {
        let store = store("moved").await;
        let id = waiting(&store, "a", "waiting-pr:owner/repo#42: review").await;
        let wait = store.blocked_pr_waits().await.unwrap().remove(0);
        // Re-blocked for another reason between the listing and the outcome.
        store.block(&id, "needs-owner: decide").await.unwrap();
        assert!(!store
            .resolve_pr_wait(&wait, PrOutcome::Merged)
            .await
            .unwrap());
        assert_eq!(store.get(&id).await.unwrap().status, "blocked");
        // Unblocked by hand, then finished: still nothing to apply.
        store.unblock(&id).await.unwrap();
        assert!(!store
            .resolve_pr_wait(&wait, PrOutcome::ClosedUnmerged)
            .await
            .unwrap());
        assert_eq!(store.get(&id).await.unwrap().status, "open");
        // A task that no longer exists.
        let gone = PrWait {
            id: "t999".into(),
            ..wait
        };
        assert!(!store
            .resolve_pr_wait(&gone, PrOutcome::Merged)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_same_reason_reblock_rejects_stale_outcomes_even_with_identical_timestamps() {
        async fn freeze_clock(store: &Tasks, id: &str) {
            let id = id.to_owned();
            store
                .writer
                .call(move |conn| {
                    conn.execute("UPDATE tasks SET updated_at = 42 WHERE id = ?", [&id])
                        .map_err(db::map_sqlite)?;
                    conn.execute("UPDATE task_events SET at = 42 WHERE task_id = ?", [&id])
                        .map_err(db::map_sqlite)?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        for outcome in [PrOutcome::Merged, PrOutcome::ClosedUnmerged] {
            let store = store(&format!("same-reason-{outcome:?}")).await;
            let reason = "waiting-pr:owner/repo#42: review";
            let id = waiting(&store, "a", reason).await;
            freeze_clock(&store, &id).await;
            let wait = store.blocked_pr_waits().await.unwrap().remove(0);
            let observed = store.get(&id).await.unwrap();
            // GitHub detail is in flight while the user starts a new, identical block.
            store.unblock(&id).await.unwrap();
            store.block(&id, reason).await.unwrap();
            freeze_clock(&store, &id).await;
            let fresh = store.blocked_pr_waits().await.unwrap().remove(0);
            assert_eq!(
                store.get(&id).await.unwrap(),
                observed,
                "row values and timestamps are the same"
            );
            assert!(fresh.block_event_id.unwrap() > wait.block_event_id.unwrap());
            let events = store.events(&id).await;
            assert!(!store.resolve_pr_wait(&wait, outcome).await.unwrap());
            assert_eq!(
                store.events(&id).await,
                events,
                "stale result writes no event"
            );
            assert_eq!(
                store.get(&id).await.unwrap(),
                observed,
                "new block is untouched"
            );
            assert!(
                store.resolve_pr_wait(&fresh, outcome).await.unwrap(),
                "a fresh observation still resolves"
            );
        }
    }

    #[tokio::test]
    async fn a_free_form_blocked_note_is_not_a_new_block_generation() {
        let store = store("note-generation").await;
        let id = waiting(&store, "a", "waiting-pr:owner/repo#42: review").await;
        let wait = store.blocked_pr_waits().await.unwrap().remove(0);
        // Raw note event names are not transitions: their status_to column is NULL.
        store
            .note(&id, "blocked", "%311", Some("ordinary coordination note"))
            .await
            .unwrap();
        let after_note = store.blocked_pr_waits().await.unwrap().remove(0);
        assert_eq!(after_note.block_event_id, wait.block_event_id);
        assert!(store
            .resolve_pr_wait(&wait, PrOutcome::Merged)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn the_wait_survives_a_store_reopen() {
        let dir = crate::testsupport::dir("pr-wait-reopen");
        let path = dir.path().join("tasks.db");
        let id = {
            let store = Tasks::open(&path).await.unwrap();
            waiting(&store, "a", "waiting-pr:owner/repo#42: review").await
        };
        // A daemon restart: a new store on the same file still sees the wait and resolves it.
        let store = Tasks::open(&path).await.unwrap();
        let wait = store.blocked_pr_waits().await.unwrap().remove(0);
        assert_eq!(wait.id, id);
        assert!(store
            .resolve_pr_wait(&wait, PrOutcome::Merged)
            .await
            .unwrap());
        assert_eq!(store.get(&id).await.unwrap().status, "open");
    }
}
