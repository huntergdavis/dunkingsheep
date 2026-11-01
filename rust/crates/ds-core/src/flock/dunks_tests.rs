//! Focused tests for the T32 dunk persistence facade. Whole-file restart semantics belong to
//! the whole-Flock persistence leaf; these tests cover extraction and the snapshot callback.

use super::sends::SnapshotStore;
use super::Flock;
use crate::error::DunkError;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

type DunkRows = Vec<Vec<(&'static str, Value)>>;
type DunkSnapshot = (i64, DunkRows);

#[derive(Default)]
struct CapturingSnapshotStore {
    calls: AtomicUsize,
    dunks: Mutex<Option<DunkSnapshot>>,
}

impl SnapshotStore for CapturingSnapshotStore {
    fn save(&self, flock: &Flock) -> Result<(), DunkError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.dunks.lock().unwrap() = Some(flock.dunks.snapshot());
        Ok(())
    }
}

struct OfflineSnapshotStore;

impl SnapshotStore for OfflineSnapshotStore {
    fn save(&self, _flock: &Flock) -> Result<(), DunkError> {
        Err(DunkError::Refused("offline".into()))
    }
}

fn flock() -> (tempfile::TempDir, Flock) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    (dir, flock)
}

fn map(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

#[test]
fn save_dunks_invokes_the_whole_flock_snapshot_store_with_extracted_fields() {
    let (_dir, flock) = flock();
    flock._load_dunks(&map(json!({
        "next_id": 5,
        "dunkers": [
            {"id": "d4", "text": "continue", "running": true,
             "created_at": 100.0, "next_send_at": 1.0},
            "corrupt row"
        ]
    })));

    let store = CapturingSnapshotStore::default();
    flock._save_dunks(&store).unwrap();

    assert_eq!(1, store.calls.load(Ordering::SeqCst));
    let (next_id, rows) = store.dunks.lock().unwrap().clone().unwrap();
    assert_eq!(5, next_id);
    assert_eq!(1, rows.len(), "corrupt dunk rows are skipped");
    let row = &rows[0];
    let field = |name: &str| {
        row.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    };
    assert_eq!(Some(&json!("d4")), field("id"));
    assert_eq!(Some(&json!(true)), field("running"));
    assert!(
        field("status").is_none(),
        "volatile status is not persisted"
    );
    assert!(field("next_send_at").and_then(Value::as_f64).unwrap() > crate::clock::now_s());
}

#[test]
fn load_dunks_tolerates_a_corrupt_dunk_collection() {
    let (_dir, flock) = flock();
    flock._load_dunks(&map(json!({"next_id": "bad", "dunkers": "corrupt"})));

    let (next_id, rows) = flock.dunks.snapshot();
    assert_eq!(1, next_id);
    assert!(rows.is_empty());
}

#[test]
fn save_dunks_forwards_snapshot_store_errors() {
    let (_dir, flock) = flock();
    let error = flock._save_dunks(&OfflineSnapshotStore).unwrap_err();
    assert!(matches!(error, DunkError::Refused(message) if message == "offline"));
}
