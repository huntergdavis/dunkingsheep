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

// ---- t1273: tests/test_dunk_cancelled_handoff.py, the advance_next_job half, by NAME -------
//
// These pin what the STORE does around the planner: it must offer the planner
// `pending_delivery = true` for any non-cancelled handoff naming an owned open task (and for
// a held lease), `false` for a cancelled one (exact string only), and it must execute the
// planner's `claim`. The plan's `reason` strings ("pending_delivery", "no_assigned_work")
// belong to `dunk_dispatch.next_job_plan`, which is NOT ported; `stand_in_plan` below
// reproduces only the three rules these tests rely on, so a pass proves the store half, not
// the real planner.

use crate::tasks::next_job::NON_BLOCKING_HANDOFF_STATES;
use crate::tasks::{ClaimOptions, FieldEdit};

const PANE: &str = "%28";

fn stand_in_plan() -> Arc<Planner> {
    Arc::new(|input| {
        let mut plan = Plan::new();
        let rank = |p: &str| match p {
            "urgent" => 0,
            "high" => 1,
            "medium" => 2,
            _ => 3,
        };
        if input.pending_delivery {
            plan.insert("action".into(), json!("skip"));
            plan.insert("reason".into(), json!("pending_delivery"));
            return plan;
        }
        let mut owned: Vec<&Decorated> = input
            .tasks
            .iter()
            .filter(|t| t.eligible && t.assignees.iter().any(|a| a == input.pane_id))
            .collect();
        owned.sort_by(|a, b| {
            rank(&a.task.priority)
                .cmp(&rank(&b.task.priority))
                .then(a.task.created_at.partial_cmp(&b.task.created_at).unwrap())
        });
        match owned.first() {
            Some(t) => {
                plan.insert("action".into(), json!("claim"));
                plan.insert("task_id".into(), json!(t.task.id));
            }
            None => {
                plan.insert("action".into(), json!("skip"));
                plan.insert("reason".into(), json!("no_assigned_work"));
            }
        }
        plan
    })
}

async fn mine(store: &Tasks, title: &str, priority: &str) -> Task {
    let t = store.add("global", title, "acceptance").await.unwrap();
    store
        .update(
            &t.id,
            FieldEdit {
                priority: Some(priority.into()),
                assignees: Some(vec![PANE.into()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap()
}

async fn record(store: &Tasks, task_id: &str, state: &str, message_id: &str) {
    let mut details = Map::new();
    details.insert("state".into(), json!(state));
    store
        .record_next_job_handoff(
            task_id,
            PANE,
            Some("t0"),
            Some(message_id),
            1,
            Some(state),
            None,
            Some(details),
        )
        .await
        .unwrap();
}

fn caller() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("pid".into(), json!(4321));
    m.insert("start".into(), json!("1.0"));
    m.insert("name".into(), json!("t461"));
    m
}

async fn advance_from(store: &Tasks, finished: &Task) -> Plan {
    let options = AdvanceOptions {
        from_task: Some(finished.id.clone()),
        caller: caller(),
        ..Default::default()
    };
    store
        .advance_next_job(PANE, "completion", options, stand_in_plan())
        .await
        .unwrap()
        .plan
}

fn text<'a>(plan: &'a Plan, key: &str) -> &'a str {
    plan.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The shared body of the "still blocks" cases: finished A, waiting B, a historical intent
/// H in `state`; the planner must see pending_delivery and skip.
async fn blocked_by_state(tag: &str, state: &str) {
    let store = store(tag).await;
    let finished = mine(&store, "A finish me", "urgent").await;
    mine(&store, "B next job", "high").await;
    let old = mine(&store, "H historical intent", "low").await;
    record(&store, &old.id, state, "m-hist").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("skip", text(&plan, "action"), "{plan:?}");
    assert_eq!("pending_delivery", text(&plan, "reason"), "state {state:?}");
}

#[tokio::test]
async fn test_cancelled_historical_intent_does_not_block_the_next_job() {
    let store = store("py-cancelled-ok").await;
    let finished = mine(&store, "A finish me", "urgent").await;
    let waiting = mine(&store, "B next job", "high").await;
    let old = mine(&store, "H historical intent", "low").await;
    record(&store, &old.id, "cancelled", "m-hist").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("claim", text(&plan, "action"), "{plan:?}");
    assert_eq!(waiting.id, text(&plan, "task_id"));
    assert_eq!("in_progress", store.get(&waiting.id).await.unwrap().status);
}

#[tokio::test]
async fn test_cancelled_intent_on_the_completed_task_does_not_block() {
    let store = store("py-cancelled-on-completed").await;
    let finished = mine(&store, "A finish me", "urgent").await;
    let waiting = mine(&store, "B next job", "high").await;
    record(&store, &finished.id, "cancelled", "m-hist").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("claim", text(&plan, "action"), "{plan:?}");
    assert_eq!(waiting.id, text(&plan, "task_id"));
}

#[tokio::test]
async fn test_control_without_any_historical_intent_also_claims() {
    let store = store("py-control").await;
    let finished = mine(&store, "A finish me", "urgent").await;
    let waiting = mine(&store, "B next job", "high").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("claim", text(&plan, "action"), "{plan:?}");
    assert_eq!(waiting.id, text(&plan, "task_id"));
}

#[tokio::test]
async fn test_pending_intent_still_blocks() {
    blocked_by_state("py-pending", "pending").await;
}

#[tokio::test]
async fn test_sent_intent_still_blocks() {
    blocked_by_state("py-sent", "sent").await;
}

#[tokio::test]
async fn test_started_intent_still_blocks() {
    blocked_by_state("py-started", "started").await;
}

#[tokio::test]
async fn test_failed_intent_still_blocks() {
    blocked_by_state("py-failed", "failed").await;
}

#[tokio::test]
async fn test_uncertain_intent_still_blocks() {
    blocked_by_state("py-uncertain", "uncertain").await;
}

#[tokio::test]
async fn test_case_variant_of_cancelled_still_blocks() {
    blocked_by_state("py-case-variant", "CANCELLED").await;
}

#[tokio::test]
async fn test_whitespace_padded_cancelled_still_blocks() {
    blocked_by_state("py-padded", " cancelled ").await;
}

#[tokio::test]
async fn test_mixed_case_cancelled_still_blocks() {
    blocked_by_state("py-mixed", "Cancelled").await;
}

#[tokio::test]
async fn test_empty_state_still_blocks() {
    blocked_by_state("py-empty", "").await;
}

#[tokio::test]
async fn test_unknown_state_blocks_because_it_is_not_proven_terminal() {
    blocked_by_state("py-unknown", "delivered-to-a-place-we-do-not-know").await;
}

#[test]
fn test_active_handoffs_matches_the_delivery_workers_skip_set() {
    // The Python reads dunk_next_job's source for ("started", "undeliverable", "cancelled")
    // and asserts the non-blocking set is {"cancelled"}. Rust has no worker source to read
    // (NextJobs is unported); the second half is the part that is portable.
    assert_eq!(vec!["cancelled"], NON_BLOCKING_HANDOFF_STATES.to_vec());
}

#[tokio::test]
async fn test_held_lease_still_blocks() {
    let store = store("py-held-lease").await;
    let held = mine(&store, "B held by me", "high").await;
    store
        .claim_next(
            "global",
            PANE,
            ClaimOptions {
                caller: Some((4321, Some("1.0".into()), Some("t461".into()))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!("in_progress", store.get(&held.id).await.unwrap().status);
    let finished = mine(&store, "A finish me", "urgent").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("skip", text(&plan, "action"), "{plan:?}");
    assert_eq!("in_progress", store.get(&held.id).await.unwrap().status);
}

#[tokio::test]
async fn test_no_assigned_work_is_still_an_honest_skip() {
    let store = store("py-no-work").await;
    let finished = mine(&store, "A finish me", "urgent").await;
    let plan = advance_from(&store, &finished).await;
    assert_eq!("skip", text(&plan, "action"), "{plan:?}");
    assert!(
        ["no_assigned_work", "no_work"].contains(&text(&plan, "reason")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn test_active_handoffs_excludes_only_cancelled() {
    let store = store("py-active-excludes").await;
    let keep = mine(&store, "keep", "high").await;
    let drop = mine(&store, "drop", "high").await;
    record(&store, &keep.id, "sent", "m-keep").await;
    record(&store, &drop.id, "cancelled", "m-drop").await;
    let owned = vec![keep.id.clone(), drop.id.clone()];
    let active = store.active_handoffs(PANE, &owned).await.unwrap();
    let ids: Vec<&str> = active
        .iter()
        .filter_map(|h| h.get("message_id").and_then(Value::as_str))
        .collect();
    assert_eq!(vec!["m-keep"], ids);
}

#[tokio::test]
async fn test_active_handoffs_ignores_tasks_the_pane_does_not_own() {
    let store = store("py-active-ignores").await;
    let other = store.add("global", "not mine", "x").await.unwrap();
    store
        .update(
            &other.id,
            FieldEdit {
                priority: Some("high".into()),
                assignees: Some(vec!["%99".into()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    record(&store, &other.id, "sent", "m-other").await;
    assert!(store.active_handoffs(PANE, &[]).await.unwrap().is_empty());
}
