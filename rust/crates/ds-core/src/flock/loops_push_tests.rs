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

use super::loop_env::LoopFuture;
use super::loops_push::{claim_filters, run_push, PushDecision, PushEnv};
use super::panes::Stores;
use super::Flock;
use crate::config::Env;
use crate::error::DunkError;
use crate::roles::Roles;
use crate::server_runtime::LoopRuntimeAdapter;
use crate::tasks::{ClaimOptions, Task, Tasks};

const IDLE_WORKER: &str = "w2:p1";
const BUSY_CLAUDE: &str = "w2:p3";
const IDLE_SHELL: &str = "w2:p2";

#[derive(Default)]
struct Fake {
    /// pane -> agent_status
    panes: Mutex<BTreeMap<String, String>>,
    /// pane -> resolved role (a pane absent here is "anonymous": not a lane)
    roles: Mutex<BTreeMap<String, (String, Option<i64>)>>,
    pane_pids: Mutex<BTreeMap<String, i64>>,
    fail_pane_list: Mutex<bool>,
    pane_list_calls: Mutex<usize>,
    may_review: Mutex<BTreeSet<String>>,
    alive: Mutex<BTreeMap<String, bool>>,
    unread: Mutex<BTreeSet<String>>,
    announced: Mutex<Vec<(String, String)>>,
    calls: Mutex<Vec<&'static str>>,
    awaited: Mutex<Vec<&'static str>>,
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
        fake.pane_pids.lock().unwrap().extend([
            (IDLE_WORKER.into(), 1101),
            (IDLE_SHELL.into(), 2202),
            (BUSY_CLAUDE.into(), 3303),
        ]);
        fake
    }
    fn register(&self, pane: &str, role: &str) {
        self.register_pid(pane, role, None);
    }
    fn register_pid(&self, pane: &str, role: &str, pane_pid: Option<i64>) {
        self.roles
            .lock()
            .unwrap()
            .insert(pane.into(), (role.into(), pane_pid));
    }
}

impl PushEnv for Fake {
    fn list_panes<'a>(&'a self) -> LoopFuture<'a, Result<Vec<Value>, DunkError>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("list_panes");
            *self.pane_list_calls.lock().unwrap() += 1;
            if *self.fail_pane_list.lock().unwrap() {
                return Err(DunkError::Refused("pane listing unavailable".into()));
            }
            Ok(self
            .panes
            .lock()
            .unwrap()
            .iter()
            .map(|(id, status)| {
                json!({"pane_id": id, "agent_status": status, "pane_pid": self.pane_pids.lock().unwrap().get(id).copied()})
            })
            .collect())
        })
    }
    fn caller_role(&self, pane_id: &str, pane_info: Option<&Value>) -> String {
        let Some((role, wanted_pid)) = self.roles.lock().unwrap().get(pane_id).cloned() else {
            return "anonymous".into();
        };
        if let Some(wanted_pid) = wanted_pid {
            let live_pid = pane_info
                .and_then(|info| info.get("pane_pid"))
                .and_then(Value::as_i64);
            if live_pid != Some(wanted_pid) {
                return "anonymous".into();
            }
        }
        role
    }
    fn registered_lanes(&self) -> Option<BTreeSet<String>> {
        Some(self.roles.lock().unwrap().keys().cloned().collect())
    }
    fn may_review(&self, pane_id: &str, _role: &str) -> bool {
        self.may_review.lock().unwrap().contains(pane_id)
    }
    fn provider_of(&self, _: &str) -> Option<String> {
        None
    }
    fn lane_agent_alive(&self, pane_id: &str) -> Option<bool> {
        self.alive.lock().unwrap().get(pane_id).copied()
    }
    fn unread_mail<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, bool> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("unread_mail");
            self.unread.lock().unwrap().contains(pane_id)
        })
    }
    fn announce_task<'a>(&'a self, pane_id: &'a str, task: &'a Task, _: f64) -> LoopFuture<'a, ()> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("announce_task");
            self.announced
                .lock()
                .unwrap()
                .push((pane_id.into(), task.id.clone()));
        })
    }
    fn recover_next_jobs<'a>(&'a self) -> LoopFuture<'a, ()> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("recover_next_jobs");
            self.calls.lock().unwrap().push("recover");
        })
    }
    fn allocator_tick<'a>(&'a self) -> LoopFuture<'a, ()> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("allocator_tick");
            self.calls.lock().unwrap().push("allocator");
        })
    }
    fn next_jobs_tick<'a>(&'a self) -> LoopFuture<'a, ()> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.awaited.lock().unwrap().push("next_jobs_tick");
            self.calls.lock().unwrap().push("next_jobs");
        })
    }
}

struct RuntimePushEnv<'a> {
    adapter: &'a LoopRuntimeAdapter,
    panes: Vec<Value>,
}

impl PushEnv for RuntimePushEnv<'_> {
    fn list_panes<'a>(&'a self) -> LoopFuture<'a, Result<Vec<Value>, DunkError>> {
        Box::pin(async move { Ok(self.panes.clone()) })
    }
    fn caller_role(&self, pane_id: &str, pane_info: Option<&Value>) -> String {
        self.adapter.helper_pushenv_caller_role(pane_id, pane_info)
    }
    fn registered_lanes(&self) -> Option<BTreeSet<String>> {
        self.adapter.helper_pushenv_registered_lanes()
    }
    fn may_review(&self, _: &str, _: &str) -> bool {
        false
    }
    fn provider_of(&self, _: &str) -> Option<String> {
        None
    }
    fn lane_agent_alive(&self, _: &str) -> Option<bool> {
        None
    }
    fn unread_mail<'a>(&'a self, _: &'a str) -> LoopFuture<'a, bool> {
        Box::pin(async move { false })
    }
    fn announce_task<'a>(&'a self, _: &'a str, _: &'a Task, _: f64) -> LoopFuture<'a, ()> {
        Box::pin(async move {})
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

#[tokio::test]
async fn production_env_push_core_uses_pid_bound_pane_info() {
    let dir = crate::testsupport::dir("push-pid-bound");
    let flock = Arc::new(Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    ));
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("roles store");
    roles
        .register("w1:p1", "worker", Some(1101), None, None, None, None, None)
        .unwrap();
    roles
        .register(
            "w2:p2",
            "reviewer",
            Some(2202),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    roles
        .register("w3:p3", "worker", Some(9999), None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    let adapter = LoopRuntimeAdapter::new(
        Arc::clone(&flock),
        Arc::new(|| Box::new(crate::terminal::fakes::FakeTerminal::new())),
    );
    let panes = adapter
        .helper_pushenv_list_panes()
        .await
        .expect("list live fake panes");
    let pane_map: BTreeMap<String, Value> = panes
        .iter()
        .filter_map(|pane| {
            pane.get("pane_id")
                .and_then(Value::as_str)
                .map(|id| (id.to_owned(), pane.clone()))
        })
        .collect();
    let env = RuntimePushEnv {
        adapter: &adapter,
        panes,
    };

    assert_eq!(env.caller_role("w1:p1", pane_map.get("w1:p1")), "worker");
    assert_eq!(env.caller_role("w2:p2", pane_map.get("w2:p2")), "reviewer");
    assert_eq!(env.caller_role("w1:p1", None), crate::roles::DEFAULT_ROLE);
    assert_eq!(
        env.caller_role("w3:p3", pane_map.get("w3:p3")),
        crate::roles::DEFAULT_ROLE
    );

    // Owner decision on t3554: only subscribers are targets. The registered worker subscribes; the
    // registered reviewer does not, so it is not a target.
    subscribe(&flock, "w1:p1");
    let targets = flock.push_target_map(&env).await;
    assert!(targets.contains_key("w1:p1"));
    assert!(
        !targets.contains_key("w2:p2"),
        "an unsubscribed reviewer is not fed"
    );
    assert!(!targets.contains_key("w3:p3"));
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

/// A lane that subscribed with `task follow` (or claimed work): the only way a lane is fed now (t3554).
fn subscribe(flock: &Flock, pane: &str) {
    flock
        .task_waiters
        .lock()
        .unwrap()
        .insert(pane.to_string(), "global".to_string());
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
async fn test_unavailable_pane_listing_makes_no_push_claims() {
    let (flock, tasks, _dir) = setup("pane-list-unavailable").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    *fake.fail_pane_list.lock().unwrap() = true;
    let task = tasks.add("global", "waiting work", "").await.unwrap();

    assert!(flock.push_tasks(&fake).await.is_empty());
    assert_eq!(status(&tasks, &task.id).await, "open");
    assert_eq!(*fake.pane_list_calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn test_a_registered_lane_that_never_subscribed_is_not_pushed() {
    // Owner decision on t3554: reading mail never claims or assigns, so a registered worker that did
    // not subscribe (task follow) or claim work is not fed, even when it is idle and registered.
    let (flock, tasks, _dir) = setup("registered-unsubscribed").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    assert!(flock.task_waiters.lock().unwrap().is_empty());
    let decisions = flock.push_tasks(&fake).await;
    assert!(
        decisions.is_empty(),
        "no decision: the lane is not a target"
    );
    assert_eq!(status(&tasks, &task.id).await, "open");
    assert!(fake.announced.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_a_subscribed_lane_is_pushed_via_follower() {
    let (flock, tasks, _dir) = setup("subscribed").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "worker");
    subscribe(&flock, IDLE_WORKER);
    let task = tasks.add("global", "waiting work", "").await.unwrap();
    let decisions = flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
    assert_eq!(decisions[0].action, "claim");
    assert_eq!(decisions[0].detail, format!("{} via=follower", task.id));
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
    subscribe(&flock, BUSY_CLAUDE);
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
    subscribe(&flock, IDLE_WORKER);
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
    subscribe(&flock, IDLE_WORKER);
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
    subscribe(&flock, IDLE_WORKER);
    let review = file_review(&tasks, "review something").await;
    flock.push_tasks(&fake).await;
    assert_eq!(status(&tasks, &review.id).await, "open");
}

#[tokio::test]
async fn test_a_reviewer_lane_is_pushed_a_review_and_not_work() {
    let (flock, tasks, _dir) = setup("reviewer").await;
    let fake = Fake::standard();
    fake.register(IDLE_WORKER, "reviewer");
    subscribe(&flock, IDLE_WORKER);
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
    subscribe(&flock, IDLE_WORKER);
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
    subscribe(&flock, IDLE_SHELL);
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
    subscribe(&flock, IDLE_SHELL);
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
    subscribe(&flock, IDLE_SHELL);
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
    subscribe(&flock, IDLE_WORKER);
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
    subscribe(&flock, IDLE_WORKER);
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

#[tokio::test]
async fn loop_push_env_futures_are_awaited_in_source_order() {
    let (flock, tasks, _dir) = setup("awaited-order").await;
    let fake = Arc::new(Fake::standard());
    fake.register(IDLE_WORKER, "worker");
    subscribe(&flock, IDLE_WORKER);
    let task = tasks.add("global", "ordered work", "").await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    flock.wake_push();
    let handle = tokio::spawn(run_push(
        flock.clone(),
        fake.clone(),
        Duration::from_secs(3600),
        stop_rx,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !fake.announced.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("announcement future must complete");
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop stops")
        .unwrap();
    assert_eq!(
        *fake.awaited.lock().unwrap(),
        vec![
            "recover_next_jobs",
            "allocator_tick",
            "next_jobs_tick",
            "list_panes",
            "unread_mail",
            "announce_task",
        ]
    );
    assert_eq!(
        *fake.announced.lock().unwrap(),
        vec![(IDLE_WORKER.into(), task.id.clone())]
    );
    assert_eq!(status(&tasks, &task.id).await, "in_progress");
    // The public target helper reads the subscribers only: no pane listing is awaited.
    assert!(flock
        .push_target_map(fake.as_ref())
        .await
        .contains_key(IDLE_WORKER));
}
