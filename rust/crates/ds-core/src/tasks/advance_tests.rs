//! Ports of the `advance_next_job` halves of tests/test_dunk_complete_ownership.py (P2
//! reassignment, operator survives reassignment, revoked operator, t430 late revocation
//! rolls everything back). The planner and the allowlist are fakes; the real ones are
//! NOT ported (dispatch.rs, dunk_superusers), so nothing here proves their behaviour.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const OWNER: &str = "w1:p1";
const FOREIGN: &str = "w9:p9";
const OPERATOR: &str = "w5:p5";

async fn store(tag: &str) -> Tasks {
    let dir = std::env::temp_dir().join(format!("ds-adv-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    Tasks::open(&path).await.unwrap()
}

/// A planner that never claims.
fn skip_planner() -> Arc<Planner> {
    Arc::new(|_input| {
        let mut plan = Plan::new();
        plan.insert("action".into(), json!("skip"));
        plan.insert("reason".into(), json!("test"));
        plan
    })
}

/// A planner that claims the first eligible task it is offered.
fn claim_first() -> Arc<Planner> {
    Arc::new(|input| {
        let mut plan = Plan::new();
        match input.tasks.iter().find(|t| t.eligible) {
            Some(t) => {
                plan.insert("action".into(), json!("claim"));
                plan.insert("task_id".into(), json!(t.task.id));
            }
            None => {
                plan.insert("action".into(), json!("skip"));
                plan.insert("reason".into(), json!("none"));
            }
        }
        plan
    })
}

/// An allowlist that grants `pane` for the first `grants` matcher calls, then revokes.
struct Fake {
    pane: String,
    grants: usize,
    calls: AtomicUsize,
    corrupt: bool,
}

impl Allowlist for Fake {
    fn with_locked(
        &self,
        body: &mut dyn FnMut(&dyn Fn(&str, Option<&Value>) -> bool) -> Result<(), DunkError>,
    ) -> Result<(), DunkError> {
        if self.corrupt {
            return Err(DunkError::Refused("allowlist corrupt".into()));
        }
        body(&|pane, _info| {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            pane == self.pane && n < self.grants
        })
    }
}

fn proof(pane: &str, grants: usize, corrupt: bool) -> Option<OperatorProof> {
    Some(OperatorProof {
        pane_id: pane.into(),
        pane_info: None,
        allowlist: Arc::new(Fake {
            pane: pane.into(),
            grants,
            calls: AtomicUsize::new(0),
            corrupt,
        }),
    })
}

async fn owned_task(store: &Tasks, title: &str) -> Task {
    let t = store.add("global", title, "").await.unwrap();
    store.assign(&t.id, &[OWNER], "set").await.unwrap();
    store.get(&t.id).await.unwrap()
}

async fn count(store: &Tasks, task: &str, event: &str) -> i64 {
    let (task, event) = (task.to_string(), event.to_string());
    store
        .writer
        .call(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM task_events WHERE task_id = ? AND event = ?",
                params![task, event],
                |r| r.get(0),
            )
            .map_err(db::map_sqlite)
        })
        .await
        .unwrap()
}

fn completion(from: &str, proof: Option<OperatorProof>) -> AdvanceOptions {
    AdvanceOptions {
        from_task: Some(from.into()),
        operator_proof: proof,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_reassignment_before_transaction_is_refused() {
    let store = store("reassign").await;
    let task = owned_task(&store, "mine").await;
    store.assign(&task.id, &[FOREIGN], "set").await.unwrap();
    let err = store
        .advance_next_job(
            OWNER,
            "completion",
            completion(&task.id, None),
            skip_planner(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not owned"), "{err}");
    assert_ne!("done", store.get(&task.id).await.unwrap().status);
    assert_eq!(0, count(&store, &task.id, "completed").await);
    assert!(store
        .next_job_handoffs(Some(OWNER))
        .await
        .unwrap()
        .iter()
        .all(|h| h.get("from_task").and_then(Value::as_str) != Some(task.id.as_str())));
}

#[tokio::test]
async fn test_operator_survives_reassignment() {
    let store = store("operator").await;
    let task = owned_task(&store, "mine").await;
    store.assign(&task.id, &[FOREIGN], "set").await.unwrap();
    let out = store
        .advance_next_job(
            OPERATOR,
            "completion",
            completion(&task.id, proof(OPERATOR, 10, false)),
            skip_planner(),
        )
        .await
        .unwrap();
    assert_eq!("done", out.task.unwrap().status);
}

#[tokio::test]
async fn test_revoked_operator_is_refused_in_transaction() {
    let store = store("revoked").await;
    let task = owned_task(&store, "mine").await;
    store.assign(&task.id, &[FOREIGN], "set").await.unwrap();
    let err = store
        .advance_next_job(
            OPERATOR,
            "completion",
            completion(&task.id, proof(OPERATOR, 0, false)),
            skip_planner(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not owned"), "{err}");
    assert_ne!("done", store.get(&task.id).await.unwrap().status);
    assert_eq!(0, count(&store, &task.id, "completed").await);
}

#[tokio::test]
async fn a_corrupt_allowlist_grants_nothing() {
    let store = store("corrupt").await;
    let task = owned_task(&store, "mine").await;
    store.assign(&task.id, &[FOREIGN], "set").await.unwrap();
    let err = store
        .advance_next_job(
            OPERATOR,
            "completion",
            completion(&task.id, proof(OPERATOR, 10, true)),
            skip_planner(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not owned"), "{err}");
    assert_ne!("done", store.get(&task.id).await.unwrap().status);
}

#[tokio::test]
async fn test_revocation_between_event_and_commit_rolls_back_everything() {
    let store = store("late").await;
    let task = owned_task(&store, "other work").await;
    store.assign(&task.id, &[FOREIGN], "set").await.unwrap();
    // An eligible next task so the completion records a handoff (else this is vacuous).
    let extra = store.add("global", "next work", "").await.unwrap();
    // Granted by the early check (call 1), revoked at the late re-read (call 2).
    let err = store
        .advance_next_job(
            OPERATOR,
            "completion",
            completion(&task.id, proof(OPERATOR, 1, false)),
            claim_first(),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("authorization changed"), "{err}");
    assert_ne!("done", store.get(&task.id).await.unwrap().status);
    assert_eq!(0, count(&store, &task.id, "completed").await);
    assert_eq!("open", store.get(&extra.id).await.unwrap().status);
    assert!(store
        .next_job_handoffs(Some(OPERATOR))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_completion_by_the_owner_claims_the_next_task_and_journals_one_handoff() {
    let store = store("claim").await;
    let task = owned_task(&store, "A").await;
    let next = store.add("global", "B", "").await.unwrap();
    let out = store
        .advance_next_job(
            OWNER,
            "completion",
            completion(&task.id, None),
            claim_first(),
        )
        .await
        .unwrap();
    assert_eq!(Some(&json!("claim")), out.plan.get("action"));
    assert_eq!("done", out.task.unwrap().status);
    assert_eq!("in_progress", store.get(&next.id).await.unwrap().status);
    let handoffs = store.next_job_handoffs(Some(OWNER)).await.unwrap();
    assert_eq!(1, handoffs.len());
    assert_eq!(Some(&json!(next.id)), handoffs[0].get("task_id"));
    assert_eq!(Some(&json!(task.id)), handoffs[0].get("from_task"));
    assert_eq!(Some(&json!("pending")), handoffs[0].get("state"));
    assert_eq!(1, count(&store, &task.id, "next_job_decision").await);
}

#[tokio::test]
async fn completing_an_already_done_task_skips_without_a_second_event() {
    let store = store("already").await;
    let task = owned_task(&store, "A").await;
    store
        .advance_next_job(
            OWNER,
            "completion",
            completion(&task.id, None),
            skip_planner(),
        )
        .await
        .unwrap();
    let out = store
        .advance_next_job(
            OWNER,
            "completion",
            completion(&task.id, None),
            skip_planner(),
        )
        .await
        .unwrap();
    assert_eq!(Some(&json!("already_completed")), out.plan.get("reason"));
    assert_eq!(1, count(&store, &task.id, "completed").await);
}
