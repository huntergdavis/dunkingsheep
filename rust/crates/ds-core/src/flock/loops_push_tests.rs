//! T36c slice 1 tests: one push sweep over a fake env and the loop's wake/stop. Ported from the
//! sweep-level cases of tests/test_push_on_idle.py (same names where the case is the same).
//!
//! NOT ported here: the cases that exercise `_unread_mail` / `_lane_agent_alive` internals
//! (`test_an_agent_deeper_than_a_direct_child_counts_as_alive`,
//! `test_the_sweeps_own_announcements_never_hold_a_lane_back`,
//! `test_fresh_lane_mail_holds_a_lane_but_stale_mail_does_not`: those decisions are `push.rs`'s and
//! are covered in push_tests.rs), `test_capability_reports_process_evidence_...`
//! (`delivery_capability`, not ported) and `test_a_pid_bound_reviewer_...` (a `caller_role`
//! resolution inside the env, not the sweep).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::loops_push::{claim_filters, run_push, PushDecision, PushEnv};
use super::panes::Stores;
use super::Flock;
use crate::error::DunkError;
use crate::tasks::{ClaimOptions, Task, Tasks};

const IDLE_WORKER: &str = "w2:p1";
const BUSY_CLAUDE: &str = "w2:p3";
const IDLE_SHELL: &str = "w2:p2";

#[derive(Default)]
struct Fake {
    /// pane -> agent_status
    panes: Mutex<BTreeMap<String, String>>,
    /// pane -> resolved role (a pane absent here is "anonymous": not a lane)
    roles: Mutex<BTreeMap<String, String>>,
    may_review: Mutex<BTreeSet<String>>,
    alive: Mutex<BTreeMap<String, bool>>,
    unread: Mutex<BTreeSet<String>>,
    announced: Mutex<Vec<(String, String)>>,
    calls: Mutex<Vec<&'static str>>,
}

impl Fake {
    fn standard() -> Fake {
        let fake = Fake::default();
        {
            let mut panes = fake.panes.lock().unwrap();
            panes.insert(IDLE_WORKER.into(), "idle".into());
            panes.insert(IDLE_SHELL.into(), "unknown".into());
            panes.insert(BUSY_CLAUDE.into(), "working".into());
        }
        fake
    }
    fn register(&self, pane: &str, role: &str) {
        self.roles.lock().unwrap().insert(pane.into(), role.into());
    }
}

impl PushEnv for Fake {
    fn list_panes(&self) -> Result<Vec<Value>, DunkError> {
        Ok(self
            .panes
            .lock()
            .unwrap()
            .iter()
            .map(|(id, status)| json!({"pane_id": id, "agent_status": status}))
            .collect())
    }
    fn caller_role(&self, pane_id: &str) -> String {
        self.roles
            .lock()
            .unwrap()
            .get(pane_id)
            .cloned()
            .unwrap_or_else(|| "anonymous".into())
    }
    fn registered_lanes(&self) -> Option<BTreeSet<String>> {
        Some(self.roles.lock().unwrap().keys().cloned().collect())
    }
    fn may_review(&self, pane_id: &str) -> bool {
        self.may_review.lock().unwrap().contains(pane_id)
    }
    fn provider_of(&self, _: &str) -> Option<String> {
        None
    }
    fn lane_agent_alive(&self, pane_id: &str) -> Option<bool> {
        self.alive.lock().unwrap().get(pane_id).copied()
    }
    fn unread_mail(&self, pane_id: &str) -> bool {
        self.unread.lock().unwrap().contains(pane_id)
    }
    fn announce_task(&self, pane_id: &str, task: &Task, _: f64) {
        self.announced
            .lock()
            .unwrap()
            .push((pane_id.into(), task.id.clone()));
    }
    fn recover_next_jobs(&self) {
        self.calls.lock().unwrap().push("recover");
    }
    fn allocator_tick(&self) {
        self.calls.lock().unwrap().push("allocator");
    }
    fn next_jobs_tick(&self) {
        self.calls.lock().unwrap().push("next_jobs");
    }
}

async fn setup(tag: &str) -> (Arc<Flock>, Arc<Tasks>, tempfile::TempDir) {
    let scratch = crate::testsupport::dir(&format!("push-{tag}"));
    let dir = scratch.path().to_path_buf();
    let flock = Arc::new(Flock::new(
        dir.join("launches.json"),
        dir.join("lanes.json"),
        false,
    ));
    let tasks = Arc::new(Tasks::open(dir.join("tasks.db")).await.unwrap());
    flock.attach_stores(Stores {
        tasks: Some(tasks.clone()),
        ..Stores::default()
    });
    (flock, tasks, scratch)
}

async fn status(tasks: &Tasks, id: &str) -> String {
    tasks.get(id).await.unwrap().status
}

async fn file_review(tasks: &Tasks, title: &str) -> Task {
    let task = tasks.add("global", title, "").await.unwrap();
    // `Tasks.writer` is private to the tasks module: set the kind through a second connection.
    let conn = rusqlite::Connection::open(tasks.path()).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute(
        "UPDATE tasks SET kind = 'review', author = 'review-sync' WHERE id = ?",
        rusqlite::params![task.id],
    )
    .unwrap();
    task
}

fn skip_reasons(decisions: &[PushDecision], pane: &str) -> Vec<String> {
    decisions
        .iter()
        .filter(|d| d.pane_id == pane && d.action == "skip")
        .map(|d| d.detail.clone())
        .collect()
}

#[tokio::test]
async fn test_a_stale_registration_is_not_a_push_target() {
    // t975: a stale binding resolves to "anonymous" through caller_role, so it is no lane.
    let (flock, tasks, _dir) = setup("stale").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "anonymous");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    assert!(flock.push_tasks(&fake).await.is_empty());
    assert_eq!(status(&tasks, &task.id).await, "open");
}

#[tokio::test]
async fn test_a_registered_lane_is_pushed_without_ever_calling_follow() {
    let (flock, tasks, _dir) = setup("registered").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    assert!(flock.task_waiters.lock().unwrap().is_empty());
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
    assert_eq!(decisions[0].action, "claim");
    assert_eq!(decisions[0].detail, format!("{} via=registered", task.id));
    assert_eq!(
        *fake.announced.lock().unwrap(),
        vec![(IDLE_WORKER.to_string(), task.id.clone())]
    );
}

#[tokio::test]
async fn test_an_unregistered_pane_is_not_pushed() {
    let (flock, tasks, _dir) = setup("unregistered").await;
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    flock.push_tasks(&Fake::standard()).await;
    assert_eq!(status(&tasks, &task.id).await, "open");
}

#[tokio::test]
async fn test_a_busy_registered_lane_is_skipped() {
    let (flock, tasks, _dir) = setup("busy").await;
    let fake = Fake::standard();
    fake.register(BUSY_CLAUDE, "worker");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "open");
    assert_eq!(skip_reasons(&decisions, BUSY_CLAUDE), ["busy_status"]);
}

#[tokio::test]
async fn test_a_lane_holding_a_lease_is_not_given_a_second_task() {
    let (flock, tasks, _dir) = setup("holding").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    let first = tasks.add("global", "first", "").await.unwrap();
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &first.id).await, "in_progress");
    let second = tasks.add("global", "second", "").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &second.id).await, "open");
    assert_eq!(skip_reasons(&decisions, IDLE_WORKER), ["holding_work"]);
    assert_eq!(
        tasks
            .leases_for_pane(IDLE_WORKER, None)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_unread_mail_is_read_before_new_work() {
    let (flock, tasks, _dir) = setup("mail").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    fake.unread.lock().unwrap().insert(IDLE_WORKER.into());
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(
        status(&tasks, &task.id).await,
        "open",
        "a lane with unread mail is not handed new work"
    );
    assert_eq!(skip_reasons(&decisions, IDLE_WORKER), ["unread_mail"]);
    // Once the mail is read, the same sweep does push it.
    fake.unread.lock().unwrap().clear();
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
}

#[tokio::test]
async fn test_a_worker_is_never_pushed_a_review() {
    // t595: the push path used to claim without the role->kind filter.
    let (flock, tasks, _dir) = setup("worker-review").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    let review = file_review(&tasks, "review something").await;
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &review.id).await, "open");
}

#[tokio::test]
async fn test_a_reviewer_lane_is_pushed_a_review_and_not_work() {
    let (flock, tasks, _dir) = setup("reviewer").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "reviewer");
    fake.may_review.lock().unwrap().insert(IDLE_WORKER.into());
    let work = tasks.add("global", "ordinary work", "").await.unwrap();
    let review = file_review(&tasks, "review something").await;
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &review.id).await, "in_progress");
    assert_eq!(status(&tasks, &work.id).await, "open");
}

#[tokio::test]
async fn test_a_reviewer_on_a_provider_that_may_not_review_is_skipped() {
    // Free models are the worker tier: a pi reviewer is not handed reviews.
    let (flock, tasks, _dir) = setup("reviewer-pi").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "reviewer");
    let review = file_review(&tasks, "review something").await;
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &review.id).await, "open");
    assert_eq!(
        skip_reasons(&decisions, IDLE_WORKER),
        ["provider_may_not_review provider=unknown"]
    );
}

#[tokio::test]
async fn test_a_registered_worker_whose_agent_exited_is_not_fed() {
    // Review t1010 finding 1: the wrapper ends in `exec sh`; the sweep must not feed a bare shell.
    let (flock, tasks, _dir) = setup("exited").await;
    let fake = Fake::standard();
    fake.register(IDLE_SHELL, "worker");
    fake.alive.lock().unwrap().insert(IDLE_SHELL.into(), false);
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "open");
    assert_eq!(skip_reasons(&decisions, IDLE_SHELL), ["no_live_agent"]);
}

#[tokio::test]
async fn test_a_worker_with_a_live_agent_is_fed() {
    let (flock, tasks, _dir) = setup("alive").await;
    let fake = Fake::standard();
    fake.register(IDLE_SHELL, "worker");
    fake.alive.lock().unwrap().insert(IDLE_SHELL.into(), true);
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
}

#[tokio::test]
async fn test_an_unknown_process_table_never_blocks_a_push() {
    let (flock, tasks, _dir) = setup("unknown-proc").await;
    let fake = Fake::standard();
    fake.register(IDLE_SHELL, "worker"); // lane_agent_alive -> None: cannot say
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
}

#[tokio::test]
async fn test_desk_and_planner_lanes_are_never_pushed_work() {
    let (flock, tasks, _dir) = setup("desk").await;
    let fake = Fake::standard();
    fake.register(IDLE_SHELL, "desk");
    fake.register(IDLE_WORKER, "planner");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    assert!(flock.push_tasks(&fake).await.is_empty());
    assert_eq!(status(&tasks, &task.id).await, "open");
}

#[tokio::test]
async fn test_an_explicit_follower_keeps_its_scope() {
    // `task follow mine` still means mine: the sweep must not widen it.
    let (flock, tasks, _dir) = setup("follower").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    flock
        .task_waiters
        .lock()
        .unwrap()
        .insert(IDLE_WORKER.into(), "mine".into());
    let unowned = tasks.add("global", "unowned queue work", "").await.unwrap();
    let mine = tasks.add("global", "my own work", "").await.unwrap();
    tasks.assign(&mine.id, &[IDLE_WORKER], "set").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &mine.id).await, "in_progress");
    assert_eq!(status(&tasks, &unowned.id).await, "open");
    assert!(decisions[0].detail.ends_with("via=follower"));
}

// -- beyond the Python tests ---------------------------------------------------------------

#[test]
fn test_claim_filters_by_role() {
    assert_eq!(
        claim_filters("reviewer", "%4"),
        (Some(vec!["review".to_string()]), Some("%4".to_string()))
    );
    assert_eq!(claim_filters("owner", "%4"), (None, None));
    let worker = claim_filters("worker", "%4");
    assert_eq!(
        worker.0,
        Some(vec!["work".into(), "verify".into(), "report".into()])
    );
    assert_eq!(worker.1, None);
    let _ = ClaimOptions::default();
}

#[tokio::test]
async fn test_the_loop_recovers_once_then_sweeps_on_wake_and_stops() {
    let (flock, tasks, _dir) = setup("loop").await;
    let fake = Arc::new(Fake::standard());
    fake.register(IDLE_WORKER, "worker");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // A 1-hour interval: only the wake can trigger the sweep inside this test.
    let handle = tokio::spawn(run_push(
        flock.clone(),
        fake.clone(),
        Duration::from_secs(3600),
        stop_rx,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        status(&tasks, &task.id).await,
        "open",
        "no wake yet, no sweep"
    );
    flock.wake_push();
    for _ in 0..100 {
        if status(&tasks, &task.id).await == "in_progress" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        status(&tasks, &task.id).await,
        "in_progress",
        "a turn-end wake sweeps at once"
    );
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec!["recover", "allocator", "next_jobs"]
    );
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("stops")
        .unwrap();
}

#[tokio::test]
async fn test_the_loop_also_sweeps_on_the_interval() {
    let (flock, tasks, _dir) = setup("loop-interval").await;
    let fake = Arc::new(Fake::standard());
    fake.register(IDLE_WORKER, "worker");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(run_push(flock, fake, Duration::from_millis(30), stop_rx));
    for _ in 0..100 {
        if status(&tasks, &task.id).await == "in_progress" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        status(&tasks, &task.id).await,
        "in_progress",
        "the sweep fires without a wake"
    );
    stop_tx.send(true).unwrap();
    handle.await.unwrap();
}
