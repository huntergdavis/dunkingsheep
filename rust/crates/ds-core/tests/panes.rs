//! T16 slice: retired panes, ported from tests/test_dunk_panes*.py.
//!
//! Retiring is BOOKKEEPING, not deletion: the mail stays, the pane stops counting as idle,
//! and the fact that it is retired is reported instead of silently vanishing.

use ds_core::panes::RetiredPanes;

const NOW: f64 = 1_000_000.0;

fn store(directory: &tempfile::TempDir) -> RetiredPanes {
    RetiredPanes::new(directory.path().join("retired-panes.json"))
}

#[test]
fn a_retired_pane_is_recorded_with_its_reason_and_mailboxes() {
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    let retired = panes
        .retire(
            "%110",
            "superseded-by:w1:p7",
            Some("%107"),
            &["reviewer-7".to_string()],
            &["crackdown-codex".to_string()],
            NOW,
        )
        .expect("retired");
    assert_eq!("%110", retired.pane_id);
    assert_eq!("superseded-by:w1:p7", retired.reason);
    assert_eq!(Some("%107"), retired.by.as_deref());
    assert_eq!(vec!["reviewer-7".to_string()], retired.labels);
    assert_eq!(
        vec!["crackdown-codex".to_string()],
        retired.mailboxes,
        "a mailbox NAME is recorded, not only the tab label"
    );
    assert_eq!(NOW, retired.retired_at);
    assert!(panes.is_retired("%110").expect("read"));
}

#[test]
fn a_retire_without_a_reason_is_refused() {
    // An unexplained silence is a bug: the reason is what a report shows instead of "gone".
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    let error = panes
        .retire("%110", "   ", None, &[], &[], NOW)
        .unwrap_err();
    assert!(
        error.to_string().contains("a reason is required"),
        "{error}"
    );
    let empty = panes
        .retire("  ", "superseded", None, &[], &[], NOW)
        .unwrap_err();
    assert!(
        empty.to_string().contains("a pane id is required"),
        "{empty}"
    );
    assert!(!panes.is_retired("%110").expect("read"));
}

#[test]
fn labels_and_mailboxes_are_sorted_and_deduplicated() {
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    let retired = panes
        .retire(
            "%110",
            "superseded",
            None,
            &[
                "b".to_string(),
                "a".to_string(),
                "b".to_string(),
                " ".to_string(),
            ],
            &["z".to_string(), "y".to_string(), "z".to_string()],
            NOW,
        )
        .expect("retired");
    assert_eq!(vec!["a".to_string(), "b".to_string()], retired.labels);
    assert_eq!(vec!["y".to_string(), "z".to_string()], retired.mailboxes);
}

#[test]
fn all_is_sorted_by_pane_id_and_survives_a_fresh_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    panes
        .retire("%9", "superseded", None, &[], &[], NOW)
        .expect("first");
    panes
        .retire("%2", "replaced-by-machinery", None, &[], &[], NOW)
        .expect("second");
    let all = panes.all().expect("read");
    assert_eq!(
        vec!["%2", "%9"],
        all.iter().map(|r| r.pane_id.as_str()).collect::<Vec<_>>()
    );
    // A new handle on the same file sees the same data: it is a file, not memory.
    let reopened = RetiredPanes::new(directory.path().join("retired-panes.json"));
    assert_eq!(2, reopened.all().expect("read").len());
    assert_eq!(
        Some("replaced-by-machinery".to_string()),
        reopened.reason_of("%2").expect("read")
    );
    assert_eq!(None, reopened.reason_of("%404").expect("read"));
}

#[test]
fn unretiring_returns_the_record_and_refuses_a_pane_that_was_not_retired() {
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    panes
        .retire("%110", "superseded", None, &[], &[], NOW)
        .expect("retired");
    let removed = panes.unretire("%110").expect("unretired");
    assert_eq!("%110", removed.pane_id);
    assert!(!panes.is_retired("%110").expect("read"));
    // A typo must not be a silent no-op.
    let error = panes.unretire("%110").unwrap_err();
    assert!(error.to_string().contains("is not retired"), "{error}");
}

#[test]
fn the_file_is_private_and_a_malformed_one_reads_as_empty() {
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    panes
        .retire("%110", "superseded", None, &[], &[], NOW)
        .expect("retired");
    let mode = ds_core::atomic_json::mode(&panes.path).expect("stat");
    assert_eq!(0o600, mode, "the record is not group/world readable");
    // A corrupt file is bookkeeping, not a startup failure: it reads as empty.
    std::fs::write(&panes.path, "{not json").expect("corrupt it");
    assert!(panes.all().expect("read").is_empty());
    assert!(!panes.is_retired("%110").expect("read"));
}

// -- review t985: two Python behaviours the first port lost --------------------

#[test]
fn a_pane_id_is_matched_through_surrounding_whitespace() {
    // Python: `str(pane_id or "").strip() in _load(path)`. The first port used the id
    // verbatim, so `is_retired(" %110 ")` was false where Python answers true - and an
    // id pasted out of a log line carries whitespace, which is the normal case.
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    panes
        .retire("%110", "superseded", None, &[], &[], NOW)
        .expect("retired");
    for spelling in ["%110", " %110", "%110 ", "\t%110\n", "  %110  "] {
        assert!(
            panes.is_retired(spelling).expect("read"),
            "is_retired({spelling:?}) must find %110"
        );
        assert_eq!(
            Some("superseded".to_string()),
            panes.reason_of(spelling).expect("read"),
            "reason_of({spelling:?}) must find %110 too"
        );
    }
    assert!(!panes.is_retired(" ").expect("read"), "empty is not a pane");
}

#[test]
fn a_zero_timestamp_is_stamped_now_not_written_as_zero() {
    // Python: `"retired_at": now or time.time()`, so 0 means "stamp now"; and
    // origin/main's valid_retirements REFUSES a record whose at is 0, because an
    // undated retirement must not withdraw identity protection. Writing the 0 through
    // produced a record its own consumer rejects.
    let directory = tempfile::tempdir().expect("tempdir");
    let panes = store(&directory);
    let before = ds_core::clock::now_s();
    let record = panes
        .retire("%110", "superseded", None, &[], &[], 0.0)
        .expect("retired");
    let after = ds_core::clock::now_s();
    assert!(
        record.retired_at > 0.0,
        "a zero stamp must not reach the record"
    );
    assert!(
        record.retired_at >= before && record.retired_at <= after,
        "the stamp must be the clock, not a guess: {} not in [{before}, {after}]",
        record.retired_at
    );
    // and a caller-supplied stamp is still honoured exactly
    let stamped = panes
        .retire("%111", "superseded", None, &[], &[], 1234.5)
        .expect("retired");
    assert_eq!(1234.5, stamped.retired_at);
}
