//! Restart probes for queued outbox rows. A row restored from a snapshot must deliver through
//! the real pool without a new enqueue, and a refused restored row must settle exactly like the
//! same row enqueued after startup. Every fixture is a temp directory and a fake terminal.
use super::*;
use crate::flock::panes::Stores;
use crate::flock::sends::{message_snapshot, outbox_lock};
use crate::terminal::fakes::FakeTerminal;

fn flock_with_stores(dir: &tempfile::TempDir) -> Arc<Flock> {
    let flock = Arc::new(Flock::new(
        dir.path().join("launches"),
        dir.path().join("lanes"),
        false,
    ));
    let store = Arc::new(
        crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
            .unwrap(),
    );
    flock.attach_stores(Stores {
        inbox: Some(store),
        ..Stores::default()
    });
    flock
}

/// Rows exactly as a restart finds them: encoded by the snapshot writer, restored before any
/// runtime exists, and never enqueued again.
fn restored_flock(dir: &tempfile::TempDir, rows: &[Message]) -> Arc<Flock> {
    let flock = flock_with_stores(dir);
    let outbox: Vec<serde_json::Value> = rows.iter().map(message_snapshot).collect();
    flock.restore_snapshot(&serde_json::json!({ "outbox": outbox }));
    flock
}

fn sendable(id: &str, pane: &str) -> Message {
    let mut message = Message::new(id, pane, "lane", "restored body", "test");
    message.hold_while_typing = false;
    message.press_enter = false;
    message
}

fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn start_outbox(flock: &Arc<Flock>, runtime: &tokio::runtime::Runtime) -> OutboxRuntime {
    OutboxRuntime::new(
        Arc::clone(flock),
        runtime.handle().clone(),
        Arc::new(|| Box::new(FakeTerminal::new())),
    )
    .unwrap()
}

fn history(flock: &Flock) -> Vec<(String, String)> {
    flock
        .message_history
        .lock()
        .unwrap()
        .iter()
        .map(|message| (message.id.clone(), message.status.clone()))
        .collect()
}

/// (history status, queue status, hold count) for one message, read under the queue lock.
fn settled(flock: &Flock, id: &str, pane: &str) -> (Option<String>, Option<String>, i64) {
    let history_status = flock
        .message_history
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message.id == id)
        .map(|message| message.status.clone());
    let outbox = outbox_lock(&flock.outbox);
    let row = outbox
        .get(pane)
        .and_then(|queue| queue.iter().find(|message| message.id == id));
    (
        history_status,
        row.map(|message| message.status.clone()),
        row.map_or(0, |message| message.holds),
    )
}

/// True once a refusal has left a trace: a history entry or a recorded hold.
fn refusal_recorded(flock: &Flock) -> bool {
    let state = settled(flock, "m-missing", "w9:p9");
    state.0.is_some() || state.2 >= 1
}

fn wait_for(limit: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn restored_queued_row_delivers_without_new_enqueue() {
    let dir = tempfile::tempdir().unwrap();
    let flock = restored_flock(&dir, &[sendable("m-restored", "w1:p1")]);
    let runtime = new_runtime();
    let owner = start_outbox(&flock, &runtime);
    // Nothing is enqueued here: delivery must come from the startup schedule alone.
    let delivered = wait_for(Duration::from_secs(5), || {
        history(&flock)
            .iter()
            .any(|(id, status)| id == "m-restored" && status == "sent")
    });
    owner.stop_and_join();
    assert!(
        delivered,
        "restored row never delivered: {:?}",
        history(&flock)
    );
    let deliveries = history(&flock)
        .iter()
        .filter(|(id, _)| id == "m-restored")
        .count();
    assert_eq!(1, deliveries, "restored row must deliver exactly once");
    assert!(outbox_lock(&flock.outbox)
        .values()
        .flatten()
        .all(|row| row.status != "queued"));
}

#[test]
fn restored_rows_keep_fifo_per_pane_and_deliver_once() {
    let dir = tempfile::tempdir().unwrap();
    let flock = restored_flock(
        &dir,
        &[
            sendable("m1", "w1:p1"),
            sendable("m2", "w1:p1"),
            sendable("m3", "w2:p2"),
        ],
    );
    let runtime = new_runtime();
    let owner = start_outbox(&flock, &runtime);
    let delivered = wait_for(Duration::from_secs(5), || history(&flock).len() >= 3);
    owner.stop_and_join();
    assert!(
        delivered,
        "restored rows not all delivered: {:?}",
        history(&flock)
    );
    let order: Vec<String> = history(&flock).into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        3,
        order.len(),
        "each restored row delivers exactly once: {order:?}"
    );
    let first_pane: Vec<&str> = order
        .iter()
        .map(String::as_str)
        .filter(|id| *id != "m3")
        .collect();
    assert_eq!(vec!["m1", "m2"], first_pane);
}

#[test]
fn restored_refusal_settles_like_an_enqueued_one_and_keeps_the_row() {
    let row = || {
        let mut message = Message::new("m-missing", "w9:p9", "lane", "body", "test");
        message.hold_while_typing = true;
        message
    };
    let runtime = new_runtime();

    // Control: the enqueue path, which the pre-restart daemon used for this same row.
    let enqueued_dir = tempfile::tempdir().unwrap();
    let enqueued = flock_with_stores(&enqueued_dir);
    let enqueued_owner = start_outbox(&enqueued, &runtime);
    enqueued
        .enqueue(row(), enqueued_owner.pool.as_ref(), &FlockSnapshot)
        .unwrap();
    let control_settled = wait_for(Duration::from_secs(10), || refusal_recorded(&enqueued));
    enqueued_owner.stop_and_join();
    let expected = settled(&enqueued, "m-missing", "w9:p9");

    let restored_dir = tempfile::tempdir().unwrap();
    let restored = restored_flock(&restored_dir, &[row()]);
    let restored_owner = start_outbox(&restored, &runtime);
    let restored_settled = wait_for(Duration::from_secs(10), || refusal_recorded(&restored));
    restored_owner.stop_and_join();
    let actual = settled(&restored, "m-missing", "w9:p9");

    assert!(
        control_settled,
        "control enqueue never settled: {expected:?}"
    );
    assert!(
        restored_settled,
        "restored refusal never settled: {actual:?}"
    );
    assert_eq!(
        expected, actual,
        "restored refusal must settle like an enqueued one"
    );
    assert!(
        actual.0.is_some() || actual.1.is_some(),
        "restored row vanished after startup"
    );
}
