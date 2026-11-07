//! t3370: the seven keys a restart used to drop from a queued outbox row. The Python half is
//! tests/test_dunk_outbox_restart_keys.py. Each test builds a message with one key set, writes the
//! row with message_snapshot (what a save writes), and restores it with message_from_snapshot (what
//! a restart reads). The row must come back with the same value, not the constructor default.

use serde_json::Value;

use super::sends::{message_from_snapshot, message_snapshot, Message};

fn round_trip(edit: impl FnOnce(&mut Message)) -> Message {
    let mut message = Message::new("m1", "w1:p2", "Claude Proj", "held text", "test");
    edit(&mut message);
    message_from_snapshot(&message_snapshot(&message)).expect("the row restores")
}

#[test]
fn press_enter_false_survives_a_restart() {
    // The reviewer's case: a no-Enter send must stay a no-Enter send after a restart.
    assert!(!round_trip(|m| m.press_enter = false).press_enter);
}

#[test]
fn scheduled_survives_a_restart() {
    assert!(round_trip(|m| m.scheduled = true).scheduled);
}

#[test]
fn schedule_template_survives_a_restart() {
    let restored = round_trip(|m| m.schedule_template = Some("run the nightly check".into()));
    assert_eq!(
        restored.schedule_template.as_deref(),
        Some("run the nightly check")
    );
}

#[test]
fn attempt_generation_survives_a_restart() {
    let restored = round_trip(|m| m.attempt_generation = Some("pane-gen-7".into()));
    assert_eq!(restored.attempt_generation.as_deref(), Some("pane-gen-7"));
}

#[test]
fn typed_at_survives_a_restart() {
    assert_eq!(
        round_trip(|m| m.typed_at = Some(1_793_000_123.5)).typed_at,
        Some(1_793_000_123.5)
    );
}

#[test]
fn detail_survives_a_restart() {
    let restored = round_trip(|m| m.detail = Some("waiting for the box".into()));
    assert_eq!(restored.detail.as_deref(), Some("waiting for the box"));
}

#[test]
fn finished_at_survives_a_restart() {
    assert_eq!(
        round_trip(|m| m.finished_at = Some(1_793_000_456.25)).finished_at,
        Some(1_793_000_456.25)
    );
}

#[test]
fn a_row_from_before_the_change_keeps_the_defaults() {
    // The 13 keys the older writer produced: no press_enter, scheduled, schedule_template,
    // attempt_generation, typed_at, detail or finished_at.
    let mut row = message_snapshot(&Message::new(
        "m7",
        "w1:p2",
        "Claude Proj",
        "old row",
        "test",
    ));
    let object = row.as_object_mut().expect("a snapshot is an object");
    for key in [
        "press_enter",
        "scheduled",
        "schedule_template",
        "attempt_generation",
        "typed_at",
        "detail",
        "finished_at",
    ] {
        object.remove(key);
    }
    let restored = message_from_snapshot(&row).expect("the old row restores");
    assert!(restored.press_enter, "the old default is submit with Enter");
    assert!(!restored.scheduled);
    assert_eq!(restored.schedule_template, None);
    assert_eq!(restored.attempt_generation, None);
    assert_eq!(restored.typed_at, None);
    assert_eq!(restored.detail, None);
    assert_eq!(restored.finished_at, None);
}

#[test]
fn the_row_writes_the_seven_restart_keys() {
    let row: Value = message_snapshot(&Message::new("m1", "w1:p2", "L", "t", "test"));
    let keys: Vec<&str> = row
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    for key in [
        "press_enter",
        "scheduled",
        "schedule_template",
        "attempt_generation",
        "typed_at",
        "detail",
        "finished_at",
    ] {
        assert!(keys.contains(&key), "the row writes {key}");
    }
}
