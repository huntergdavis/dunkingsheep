//! t3504 phase fixture, Rust side. A dunk whose target pane cannot be read runs the same three
//! consumed checks as the Python fixture (`_previous_send_consumed` with `read_pane` returning None):
//! the unknown-skip counter goes 1, 2, and the third check resets it to 0, sets the detector fallback
//! and lets the send through. The next cycle starts at 1, and the fallback stays set until a send
//! confirms it. The Python fixture printed exactly this sequence.

use super::DunkList;
use crate::flock::Flock;
use serde_json::{json, Value};

#[test]
fn a_dunk_at_a_missing_pane_cycles_like_the_python_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let Value::Object(payload) = json!({"next_id": 2, "dunkers": [{
        "id": "d1",
        "target_pane_id": "w1:p9",
        "target_tab": "Agent",
        "text": "OWNER CHECK-IN",
        "created_at": 100.0,
        "running": true,
        "send_count": 142,
        "unknown_skips": 0,
        "last_sent_text": "OWNER CHECK-IN",
        "only_when": "idle",
        "skip_if_unconsumed": true
    }]}) else {
        unreachable!("the payload is an object")
    };
    flock._load_dunks(&payload);
    let handle = flock.dunks.handle("d1").unwrap();

    // (check, proceed, unknown_skips after the check, detector_fallback after the check)
    let mut phases = Vec::new();
    for check in 1..=4 {
        let skipped = DunkList::timer_unknown_skip(&handle);
        let dunker = handle.lock().unwrap();
        phases.push((
            check,
            !skipped,
            dunker.unknown_skips,
            dunker.detector_fallback,
        ));
    }
    assert_eq!(
        phases,
        vec![
            (1, false, 1, false),
            (2, false, 2, false),
            (3, true, 0, true),
            (4, false, 1, true),
        ]
    );
}
