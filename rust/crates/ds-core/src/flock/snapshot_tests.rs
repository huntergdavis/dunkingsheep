use super::{sends, status_runtime::StatusState, Flock};
use crate::config::Env;
use crate::stuck::{Config, Record};
use serde_json::json;
use std::sync::atomic::Ordering;

fn flock(dir: &std::path::Path, persist: bool) -> Flock {
    Flock::new(dir.join("launches.json"), dir.join("lanes.json"), persist)
}

#[test]
fn whole_flock_snapshot_restores_queued_and_recovery_state() {
    let dir = tempfile::tempdir().unwrap();
    let source = flock(dir.path(), true);
    let executable = dir.path().join("daemon");
    std::fs::write(&executable, b"fixture").unwrap();
    let state_path = dir.path().join("state.json");
    source
        .attach_status_state(StatusState::at_start(
            "test",
            &state_path,
            executable,
            Env::default(),
        ))
        .unwrap();
    source.next_message_id.store(44, Ordering::Relaxed);
    {
        let mut message = sends::Message::new("m43", "%1", "worker", "held text", "d7");
        message.created_at = 50.0;
        message.schedule_template = Some("daily {date}".into());
        message.holds = 3;
        message.next_attempt_at = Some(80.0);
        message.hold_signature = Some(("box".into(), "composer signature".into()));
        message.hold_while_typing = false;
        sends::outbox_lock(&source.outbox)
            .entry("%1".into())
            .or_default()
            .push(message);
    }
    let mut decided_message = sends::Message::new("m42", "%1", "worker", "failed", "send_text");
    decided_message.created_at = 42.0;
    decided_message.status = "failed".into();
    decided_message.attempt_generation = Some("pid:12:start:34".into());
    decided_message.typed_at = Some(41.5);
    source.note_stuck_candidate(&decided_message);
    {
        let mut candidates = source.stuck_candidates.lock().unwrap();
        let candidate = candidates.get_mut("%1").unwrap();
        candidate.native_binding = Some((12, "start-34".into(), "session-42".into()));
        candidate.recovery_outcome = Some("recovered".into());
        candidate.recovery_detail = Some("matched receipt".into());
        candidate.recovered_at = Some(43.25);
        candidate.receipts_reconciled = Some(true);
        candidate.native_refusal = Some("ambiguous boundary".into());
        candidate.unobservable_logged = Some(true);
        candidate.native_recovery = Some(false);
        candidate.schedule_template = Some("scheduled {date}".into());
        candidate.retired_at = Some(44.5);
        candidate.consumed_at = Some(45.75);
        candidate
            .extra_fields
            .insert("future_marker".into(), json!({"version": 7}));
    }
    source.remember(&decided_message);
    {
        let mut tracker = source.stuck_tracker.lock().unwrap();
        tracker.records.insert(
            "%1".into(),
            Record {
                message_id: Some("m42".into()),
                generation: Some("pid:12:start:34".into()),
                fingerprint: Some("abc".into()),
                first_seen: Some(1.0),
                last_attempt_at: Some(2.0),
                attempts: 1,
                terminal: None,
            },
        );
        tracker.spent.insert("m42:42".into(), 1);
    }
    *source.stuck_config.lock().unwrap() = Config {
        enabled: false,
        threshold_s: 90.0,
        max_attempts: 4,
        cooldown_s: 30.0,
        observation_interval_s: 10.0,
    };
    source.save().unwrap();
    let payload: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(
        json!(["box", "composer signature"]),
        payload["outbox"][0]["hold_signature"],
        "held signature uses the Python list wire shape"
    );

    let restored = flock(dir.path(), false);
    restored.restore_snapshot(&payload);

    assert_eq!(44, restored.next_message_id.load(Ordering::Relaxed));
    let queue = restored.outbox_snapshot();
    let message = &queue["%1"][0];
    assert_eq!("held text", message.text);
    assert_eq!(Some("daily {date}"), message.schedule_template.as_deref());
    assert_eq!(3, message.holds);
    assert_eq!(Some(80.0), message.next_attempt_at);
    assert_eq!(
        Some(&("box".into(), "composer signature".into())),
        message.hold_signature.as_ref()
    );
    assert!(!message.hold_while_typing);
    let candidate = restored.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!("failed", candidate.status);
    assert_eq!(Some(41.5), candidate.typed_at);
    assert_eq!(
        Some((12, "start-34".into(), "session-42".into())),
        candidate.native_binding
    );
    assert_eq!(Some("recovered"), candidate.recovery_outcome.as_deref());
    assert_eq!(
        Some("matched receipt"),
        candidate.recovery_detail.as_deref()
    );
    assert_eq!(Some(43.25), candidate.recovered_at);
    assert_eq!(Some(true), candidate.receipts_reconciled);
    assert_eq!(
        Some("ambiguous boundary"),
        candidate.native_refusal.as_deref()
    );
    assert_eq!(Some(true), candidate.unobservable_logged);
    assert_eq!(Some(false), candidate.native_recovery);
    assert_eq!(
        Some("scheduled {date}"),
        candidate.schedule_template.as_deref()
    );
    assert_eq!(Some(44.5), candidate.retired_at);
    assert_eq!(Some(45.75), candidate.consumed_at);
    assert_eq!(
        json!({"version": 7}),
        candidate.extra_fields["future_marker"]
    );
    assert_eq!(1, restored.stuck_tracker.lock().unwrap().spent["m42:42"]);
    assert_eq!(90.0, restored.stuck_config.lock().unwrap().threshold_s);
    let tombstone = restored.decided.lock().unwrap().by_id["m42"].clone();
    assert_eq!("failed", tombstone.status);
    assert_eq!(Some(42.0), tombstone.created_at);
}

#[test]
fn python_hold_signature_list_survives_restore_and_resnapshot() {
    let python_row = json!({
        "id": "m-held",
        "pane_id": "%1",
        "target_label": "worker",
        "text": "held text",
        "source": "test",
        "hold_signature": ["modal", "Press Enter to confirm"],
        "holds": 2,
        "next_attempt_at": 80.0
    });

    let message = sends::message_from_snapshot(&python_row).unwrap();
    assert_eq!(
        Some(&("modal".to_owned(), "Press Enter to confirm".to_owned())),
        message.hold_signature.as_ref()
    );
    assert_eq!(
        json!(["modal", "Press Enter to confirm"]),
        sends::message_snapshot(&message)["hold_signature"]
    );
}

#[test]
fn restore_suppresses_only_a_decided_row_with_matching_id_and_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let make_row = |id: &str, created_at| {
        let mut message = sends::Message::new(id, "%1", "worker", id, "send_text");
        message.created_at = created_at;
        sends::message_snapshot(&message)
    };
    let payload = json!({
        "decided": {
            "same": {"status": "sent", "created_at": 1.0},
            "reused": {"status": "failed", "created_at": 9.0},
            "unknown-time": {"status": "sent", "created_at": null}
        },
        "decided_order": ["same", "reused", "unknown-time"],
        "outbox": [make_row("same", 1.0), make_row("reused", 10.0), make_row("unknown-time", 20.0)]
    });
    let restored = flock(dir.path(), false);
    restored.restore_snapshot(&payload);
    let rows = restored.outbox_snapshot();
    let ids = rows["%1"]
        .iter()
        .map(|message| message.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(vec!["reused", "unknown-time"], ids);
}

#[test]
fn missing_outbox_created_at_cannot_match_a_fallback_message_timestamp() {
    let mut row = json!({
        "id": "missing-stamp",
        "pane_id": "%1",
        "target_label": "worker",
        "text": "queued",
        "source": "send_text"
    });
    let decoded = sends::message_from_snapshot(&row).unwrap();
    let tombstone = sends::DecidedTombstone {
        status: "sent".into(),
        created_at: Some(decoded.created_at),
    };

    // The pre-fix comparison against Message::created_at would match this exact value and
    // could suppress the row, even though the persisted outbox row had no timestamp.
    assert!(
        (tombstone.created_at.unwrap() - decoded.created_at).abs() <= 1e-3,
        "fixture must reproduce the old fabricated-timestamp match"
    );
    assert!(!sends::decided_tombstone_matches_row(
        &tombstone,
        row.get("created_at").and_then(serde_json::Value::as_f64),
    ));

    row["created_at"] = json!(decoded.created_at);
    assert!(sends::decided_tombstone_matches_row(
        &tombstone,
        row.get("created_at").and_then(serde_json::Value::as_f64),
    ));
}

#[test]
fn an_inbox_status_read_error_keeps_the_queued_notice() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = std::sync::Arc::new(
        crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
            .unwrap(),
    );
    let restored = flock(dir.path(), false);
    restored.attach_stores(super::panes::Stores {
        inbox: Some(inbox),
        ..super::panes::Stores::default()
    });
    let mut message = sends::Message::new("queued", "%1", "worker", "mail", "notice");
    message.notify_for = vec!["missing-inbox-row".into()];
    restored.restore_outbox(&[sends::message_snapshot(&message)]);
    assert_eq!(1, restored.outbox_snapshot()["%1"].len());
}

#[test]
fn save_writes_a_complete_atomic_snapshot_to_the_attached_state_path() {
    let dir = tempfile::tempdir().unwrap();
    let flock = flock(dir.path(), true);
    let executable = dir.path().join("daemon");
    std::fs::write(&executable, b"fixture").unwrap();
    let path = dir.path().join("state.json");
    flock
        .attach_status_state(StatusState::at_start(
            "test",
            &path,
            executable,
            Env::default(),
        ))
        .unwrap();
    flock.save().unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    for field in [
        "version",
        "next_id",
        "next_message_id",
        "dunkers",
        "stuck_candidates",
        "stuck",
        "stuck_config",
        "outbox",
        "decided",
        "decided_order",
    ] {
        assert!(payload.get(field).is_some(), "missing {field}");
    }
}
