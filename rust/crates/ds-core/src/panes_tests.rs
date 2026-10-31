//! `panes` tests (T16 slice): the seven from `tests/test_dunk_panes.py`, ported 1:1
//! with their names, plus the two public functions the Python leaves untested
//! (`valid_retirements`, `retired_mailboxes`) - the ones whose whole point is what
//! they REFUSE to infer.

use std::path::PathBuf;

use super::*;

fn store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("retired.json");
    (dir, path)
}

#[test]
fn a_reason_is_required() {
    let (_dir, path) = store();
    assert!(retire("wW:p4", "", Some("w9:pH"), &[], &[], &path, None).is_err());
    assert!(retire("", "superseded", Some("w9:pH"), &[], &[], &path, None).is_err());
}

#[test]
fn all_spellings_of_the_mailbox_are_retired() {
    let (_dir, path) = store();
    retire(
        "wW:p4",
        "superseded-by:wW:p7",
        Some("w9:pH"),
        &["par-d"],
        &[],
        &path,
        None,
    )
    .expect("retire");
    for mailbox in ["wW:p4", "par-d (wW:p4)", "par-d"] {
        assert!(mailbox_retired(mailbox, &path), "{mailbox}");
    }
    assert!(!mailbox_retired("par-d2 (wW:p7)", &path));
    assert!(!mailbox_retired("wW:p7", &path));
}

#[test]
fn retiring_a_pane_that_no_longer_exists_still_records_it() {
    let (_dir, path) = store();
    let record = retire(
        "wW:p4",
        "superseded-by:wW:p7",
        Some("w9:pH"),
        &["par-d"],
        &[],
        &path,
        None,
    )
    .expect("retire");
    assert_eq!("wW:p4", record.pane_id);
    assert!(mailbox_retired("par-d (wW:p4)", &path));
}

#[test]
fn a_mailbox_name_that_outlives_its_pane_is_retired_too() {
    let (_dir, path) = store();
    retire(
        "wS:p2",
        "shelved",
        Some("w9:pH"),
        &["crackdown-codex"],
        &["crackdown-codex"],
        &path,
        None,
    )
    .expect("retire");
    assert!(mailbox_retired("crackdown-codex", &path));
    assert!(mailbox_retired("wS:p2", &path));
}

#[test]
fn unretire_brings_a_pane_back() {
    let (_dir, path) = store();
    retire(
        "wW:p1",
        "replaced-by:dispatcher",
        Some("w9:pH"),
        &[],
        &[],
        &path,
        None,
    )
    .expect("retire");
    assert!(is_retired("wW:p1", &path));
    unretire("wW:p1", &path).expect("unretire");
    assert!(!is_retired("wW:p1", &path));
}

#[test]
fn unretiring_an_unknown_pane_is_a_named_refusal() {
    let (_dir, path) = store();
    let error = unretire("wW:p9", &path).expect_err("not retired");
    assert!(error.to_string().contains("is not retired"), "{error}");
}

#[test]
fn the_file_is_written_atomically_and_reads_back() {
    let (_dir, path) = store();
    retire("wW:p4", "superseded", Some("w9:pH"), &[], &[], &path, None).expect("retire");
    // atomic_json leaves no temp file behind; check the directory, not a guessed name.
    let leftovers: Vec<String> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
    let records = all_retired(&path);
    assert_eq!(
        vec!["wW:p4".to_string()],
        records
            .iter()
            .map(|r| r.pane_id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!("superseded", records[0].reason);
}

#[test]
fn an_absent_file_means_nothing_is_retired() {
    let (_dir, path) = store();
    assert!(all_retired(&path).is_empty());
    assert!(!mailbox_retired("wW:p4", &path));
}

#[test]
fn a_reasonless_or_undated_record_cannot_withdraw_identity_protection() {
    let good = Retired {
        pane_id: "wW:p4".into(),
        reason: "superseded".into(),
        by: None,
        labels: vec![],
        mailboxes: vec![],
        retired_at: 1_700_000_000.0,
    };
    let reasonless = Retired {
        reason: "  ".into(),
        ..good.clone()
    };
    let undated = Retired {
        retired_at: 0.0,
        ..good.clone()
    };
    let infinite = Retired {
        retired_at: f64::INFINITY,
        ..good.clone()
    };
    let valid = valid_retirements(&[good.clone(), reasonless, undated, infinite]);
    assert_eq!(1, valid.len());
    assert_eq!("wW:p4", valid[0].pane_id);
}

#[test]
fn archiving_never_infers_from_a_name_prefix_or_an_age() {
    let record = Retired {
        pane_id: "wW:p4".into(),
        reason: "superseded".into(),
        by: None,
        labels: vec!["par-d".into()],
        mailboxes: vec!["crackdown-codex".into()],
        retired_at: 1_700_000_000.0,
    };
    let candidates: Vec<String> = [
        "wW:p4",
        "par-d (wW:p4)",
        "par-d",
        "crackdown-codex",
        "par-d2 (wW:p7)",
        "par-d2",
        "unrouted:crackdown-codex",
    ]
    .iter()
    .map(|value| value.to_string())
    .collect();
    let mut selected = retired_mailboxes(&candidates, std::slice::from_ref(&record), &[], &[]);
    // The Python returns a SET; this port returns it sorted, so the assertion is on
    // membership and dedupe, not on an order the Python never promised (t980).
    selected.sort();
    assert_eq!(
        vec![
            "crackdown-codex".to_string(),
            "par-d".to_string(),
            "par-d (wW:p4)".to_string(),
            "wW:p4".to_string()
        ],
        selected
    );
}

#[test]
fn a_live_pane_and_a_protected_name_stay_unresolved() {
    let record = Retired {
        pane_id: "wW:p4".into(),
        reason: "superseded".into(),
        by: None,
        labels: vec!["par-d".into()],
        mailboxes: vec![],
        retired_at: 1_700_000_000.0,
    };
    // The pane is live again: its aliases must be protected, not archived.
    let candidates: Vec<String> = ["wW:p4", "par-d (wW:p4)", "par-d"]
        .iter()
        .map(|value| value.to_string())
        .collect();
    let selected = retired_mailboxes(
        &candidates,
        std::slice::from_ref(&record),
        &["wW:p4".to_string()],
        &[],
    );
    assert!(selected.is_empty(), "{selected:?}");

    // A quarantined recipient is protected even when its pane is retired.
    let protected = vec!["par-d".to_string()];
    let selected = retired_mailboxes(&candidates, std::slice::from_ref(&record), &[], &protected);
    assert!(
        !selected.iter().any(|value| value == "par-d"),
        "{selected:?}"
    );
}

#[test]
fn the_written_json_matches_the_pythons_bytes() {
    // Parity, measured rather than assumed: `json.dumps(data, indent=2,
    // sort_keys=True)` for the same record produces exactly this text (no trailing
    // newline), so a Python reader sees the same file after either implementation
    // wrote it. `atomic_json::write` adds ONE trailing newline, which is a deliberate
    // deviation recorded in the PR: it is whitespace after a complete JSON document.
    let mut records = Records::new();
    records.insert(
        "wW:p4".to_string(),
        Retired {
            pane_id: "wW:p4".into(),
            reason: "superseded-by:wW:p7".into(),
            by: Some("w9:pH".into()),
            labels: vec!["par-d".into()],
            mailboxes: vec!["crackdown-codex".into()],
            retired_at: 1_700_000_000.0,
        },
    );
    let text = serde_json::to_string_pretty(&records).expect("serialize");
    let expected = "{\n  \"wW:p4\": {\n    \"by\": \"w9:pH\",\n    \"labels\": [\n      \"par-d\"\n    ],\n    \"mailboxes\": [\n      \"crackdown-codex\"\n    ],\n    \"pane_id\": \"wW:p4\",\n    \"reason\": \"superseded-by:wW:p7\",\n    \"retired_at\": 1700000000.0\n  }\n}";
    assert_eq!(
        expected, text,
        "the JSON text differs from json.dumps(indent=2, sort_keys=True)"
    );
}

/// t980 finding 1: `mailbox_retired` strips ANY trailing parenthetical, not only a pane
/// id. The first version used the strict archive rule and diverged on 40 of 107 live
/// spellings - always "Python suppresses the alert, Rust keeps it".
#[test]
fn mailbox_retired_strips_any_trailing_parenthetical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("retired-panes.json");
    let record = Retired {
        pane_id: "par-d".to_string(),
        reason: "superseded-by:par-a".to_string(),
        by: None,
        labels: vec!["Par-D".to_string()],
        mailboxes: vec!["par-d".to_string()],
        retired_at: 1_000.0,
    };
    let mut records: Records = Records::new();
    records.insert("par-d".to_string(), record);
    write(&path, &records).expect("write");
    // A non-pane parenthetical: the Python strips it and matches the mailbox name.
    assert!(
        mailbox_retired("par-d (old)", &path),
        "a non-pane parenthetical is stripped, so the mailbox is retired"
    );
    assert!(
        mailbox_retired("Par-D (beta)", &path),
        "the label match is on the plain form"
    );
    // A pane-id parenthetical still resolves through the pane id.
    assert!(mailbox_retired("par-d (wW:p4)", &path));
    // And a name that only CONTAINS brackets in the middle is not stripped.
    assert!(!mailbox_retired("x (old) y", &path));
}

/// t980 finding 2: `now or time.time()` treats a zero timestamp as FALSY, so a retire
/// call with 0 stores a REAL time rather than a record `valid_retirements` would drop.
#[test]
fn a_zero_timestamp_is_filled_in_rather_than_written_as_undated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("retired-panes.json");
    let retired = retire("%9", "superseded", None, &[], &[], &path, Some(0.0)).expect("retired");
    assert!(retired.retired_at > 0.0, "a zero now is replaced by the clock, as the Python's                                        'now or time.time()' does");
}

/// t980 finding 3: the write path takes the Python's BOUNDED wait (5 s) rather than
/// waiting for ever - a wedged holder must turn into a refusal.
#[test]
fn the_write_path_gives_up_rather_than_waiting_for_ever() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("busy.json");
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let held = crate::lock::Guard::try_acquire(&lock_path, crate::lock::Mode::Exclusive)
        .expect("lock")
        .expect("acquired");
    let started = std::time::Instant::now();
    let error = write(&path, &Records::new()).unwrap_err();
    let waited = started.elapsed();
    assert!(
        error.to_string().contains("identity record is busy"),
        "{error}"
    );
    assert!(
        waited >= std::time::Duration::from_secs(4),
        "it waited about the timeout: {waited:?}"
    );
    assert!(
        waited < std::time::Duration::from_secs(20),
        "and did not wait for ever: {waited:?}"
    );
    drop(held);
}

/// t980 smaller note: the result is deduplicated, as the Python's set is.
#[test]
fn the_archive_selection_is_a_set_not_a_list() {
    let record = Retired {
        pane_id: "wW:p4".to_string(),
        reason: "superseded".to_string(),
        by: None,
        labels: vec!["par-d (wW:p4)".to_string()],
        mailboxes: vec!["par-d (wW:p4)".to_string()],
        retired_at: 1_000.0,
    };
    let candidates = vec!["par-d (wW:p4)".to_string(), "par-d (wW:p4)".to_string()];
    let selected = retired_mailboxes(&candidates, std::slice::from_ref(&record), &[], &[]);
    assert_eq!(
        1,
        selected.len(),
        "the same key twice is one archive target"
    );
}

#[test]
fn one_malformed_record_keeps_the_good_ones_effective() {
    let (_dir, path) = store();
    std::fs::write(
        &path,
        r#"{"wW:p4": {"pane_id": "wW:p4", "reason": "superseded", "retired_at": 1.0},
            "wW:p5": "hand-edited into nonsense"}"#,
    )
    .expect("write");
    let records = load(&path);
    assert_eq!(records.len(), 1);
    assert!(records.contains_key("wW:p4"));
    assert!(mailbox_retired("wW:p4", &path));
}

#[test]
fn a_wholly_unparseable_file_still_reads_as_empty() {
    let (_dir, path) = store();
    std::fs::write(&path, "{not json").expect("write");
    assert!(load(&path).is_empty());
}

/// t1153 review: `load` may skip a malformed record for READS, but `retire`/`unretire` write the
/// file back, and the Python round-trips the raw dict - so a hand-edited entry must SURVIVE.
const HAND_EDITED: &str = r#"{"w1:p1": "hand edited note", "w2:p2": {"pane_id": "w2:p2"}}"#;

fn raw_file(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
}

#[test]
fn retiring_keeps_a_hand_edited_or_malformed_entry_in_the_file() {
    let (_dir, path) = store();
    std::fs::write(&path, HAND_EDITED).expect("write");
    retire(
        "w3:p3",
        "superseded",
        Some("%95"),
        &[],
        &[],
        &path,
        Some(5.0),
    )
    .expect("retire");
    let file = raw_file(&path);
    assert_eq!(serde_json::json!("hand edited note"), file["w1:p1"]);
    assert_eq!(serde_json::json!({"pane_id": "w2:p2"}), file["w2:p2"]);
    assert_eq!("superseded", file["w3:p3"]["reason"]);
    assert_eq!(1, load(&path).len(), "reads still skip the malformed ones");
}

#[test]
fn unretiring_one_pane_leaves_the_other_entries_untouched() {
    let (_dir, path) = store();
    std::fs::write(&path, HAND_EDITED).expect("write");
    retire("w3:p3", "x", None, &[], &[], &path, Some(5.0)).expect("retire");
    unretire("w3:p3", &path).expect("unretire");
    let file = raw_file(&path);
    assert!(file.get("w3:p3").is_none());
    assert_eq!(serde_json::json!("hand edited note"), file["w1:p1"]);
    assert_eq!(serde_json::json!({"pane_id": "w2:p2"}), file["w2:p2"]);
}

#[test]
fn unretiring_a_malformed_entry_removes_it_like_the_python_pop() {
    let (_dir, path) = store();
    std::fs::write(&path, HAND_EDITED).expect("write");
    let removed = unretire("w1:p1", &path).expect("pop");
    assert_eq!("(malformed record removed)", removed.reason);
    assert!(raw_file(&path).get("w1:p1").is_none());
    assert!(
        unretire("nope", &path).is_err(),
        "an absent pane is still refused"
    );
}
