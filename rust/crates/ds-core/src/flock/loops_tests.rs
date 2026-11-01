//! T36 slice 3 tests: one reaper tick over a fake backend, and the loop's stop signal. Ports the
//! behaviour the Python tests pin around `_task_reaper_loop` (a fixture backend never reaps, t247;
//! a live one requeues an expired claim; a gone pane's follower is dropped).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::loops::{run_reaper, ReaperEnv};
use super::panes::Stores;
use super::Flock;
use crate::error::DunkError;
use crate::tasks::{ClaimOptions, Tasks};

struct Fake {
    panes: Result<Vec<Value>, String>,
    live: bool,
    log: Mutex<Vec<String>>,
}

impl Fake {
    fn new(panes: &[&str], live: bool) -> Fake {
        Fake {
            panes: Ok(panes.iter().map(|p| json!({ "pane_id": p })).collect()),
            live,
            log: Mutex::new(Vec::new()),
        }
    }
    fn saw(&self, what: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l.starts_with(what))
    }
}

impl ReaperEnv for Fake {
    fn list_panes(&self) -> Result<Vec<Value>, DunkError> {
        self.panes.clone().map_err(DunkError::Refused)
    }
    fn is_live(&self) -> bool {
        self.live
    }
    fn clear_historical_errors(&self, panes: &BTreeSet<String>) {
        self.log
            .lock()
            .unwrap()
            .push(format!("clear {}", panes.len()));
    }
    fn inbox_maintenance(&self, panes: &BTreeSet<String>, _: &[Value]) {
        self.log
            .lock()
            .unwrap()
            .push(format!("inbox {}", panes.len()));
    }
    fn memory_maintenance(&self) {
        self.log.lock().unwrap().push("memory".into());
    }
    fn set_last_message(&self, message: &str) {
        self.log.lock().unwrap().push(format!("last {message}"));
    }
}

async fn flock_with_tasks(tag: &str) -> (Arc<Flock>, Arc<Tasks>) {
    let dir = std::env::temp_dir().join(format!("ds-loops-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
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
    (flock, tasks)
}

#[tokio::test]
async fn a_fixture_backend_never_reaps_but_still_runs_the_maintenance() {
    let (flock, tasks) = flock_with_tasks("fixture").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let env = Fake::new(&["%1"], false);
    let tick = flock.reaper_tick(&env).await;
    assert!(tick.skipped_not_live);
    assert!(
        tick.requeued.is_empty() && tick.reap_error.is_none(),
        "no refusal logged as a fault (t247)"
    );
    assert!(
        env.saw("clear 1") && env.saw("inbox 1"),
        "the sweeps ran before the live check"
    );
    assert!(!env.saw("memory"));
    assert_eq!(
        tasks.get(&task.id).await.unwrap().status,
        "in_progress",
        "nothing moved"
    );
}

#[tokio::test]
async fn a_live_backend_requeues_a_dead_callers_claim_and_says_so() {
    let (flock, tasks) = flock_with_tasks("live").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    // A claim whose recorded caller process is gone: pid 2^31-1 with a start no process has.
    let options = ClaimOptions {
        caller: Some((i32::MAX as i64, Some("1".into()), Some("pi".into()))),
        ..Default::default()
    };
    tasks.claim_next("global", "%1", options).await.unwrap();
    let env = Fake::new(&["%1"], true);
    let tick = flock.reaper_tick(&env).await;
    assert_eq!(tick.panes, Some(1));
    assert_eq!(tick.requeued.len(), 1);
    assert_eq!(tick.requeued[0].reason, "caller_dead");
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");
    assert!(env.saw(&format!("last Requeued 1 task(s) ({})", task.id)));
    assert!(env.saw("memory"));
}

#[tokio::test]
async fn a_live_pane_with_a_live_caller_keeps_its_claim() {
    let (flock, tasks) = flock_with_tasks("alive").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let tick = flock.reaper_tick(&Fake::new(&["%1"], true)).await;
    assert!(tick.requeued.is_empty());
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "in_progress");
}

#[tokio::test]
async fn a_followers_gone_pane_is_dropped_and_a_listed_one_kept() {
    let (flock, _tasks) = flock_with_tasks("waiters").await;
    {
        let mut waiters = flock.task_waiters.lock().unwrap();
        waiters.insert("%1".into(), "global".into());
        waiters.insert("%9".into(), "global".into());
    }
    flock.reaper_tick(&Fake::new(&["%1"], true)).await;
    let waiters = flock.task_waiters.lock().unwrap().clone();
    assert_eq!(
        waiters.keys().cloned().collect::<Vec<_>>(),
        vec!["%1".to_string()]
    );
}

#[tokio::test]
async fn a_failed_pane_listing_does_not_drop_followers_or_stop_the_reap() {
    let (flock, _tasks) = flock_with_tasks("listing").await;
    flock
        .task_waiters
        .lock()
        .unwrap()
        .insert("%9".into(), "global".into());
    let mut env = Fake::new(&[], true);
    env.panes = Err("backend busy".into());
    let tick = flock.reaper_tick(&env).await;
    assert_eq!(tick.panes, None);
    assert!(tick.reap_error.is_none());
    assert_eq!(
        flock.task_waiters.lock().unwrap().len(),
        1,
        "no pane list, no cleanup (panes is None)"
    );
}

#[tokio::test]
async fn the_loop_ticks_and_stops_on_the_signal() {
    let (flock, _tasks) = flock_with_tasks("runner").await;
    let env = Arc::new(Fake::new(&["%1"], true));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(run_reaper(
        flock,
        env.clone(),
        Duration::from_millis(10),
        stop_rx,
    ));
    // Wait for the EVENT (a tick), not for a fixed time: an 80 ms sleep was a flake under load
    // (t1440). The deadline is only a backstop.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !env.saw("inbox 1") {
        assert!(
            std::time::Instant::now() < deadline,
            "the loop never ticked"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("the loop stops")
        .unwrap();
}
