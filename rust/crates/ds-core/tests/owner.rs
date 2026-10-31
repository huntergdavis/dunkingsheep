//! T16: the owner-push store, ported from test_dunk_owner_fds.py and dunk_owner.py.
//!
//! The property the Python's tests exist for: an undrained message is VISIBLE as a row
//! with `taken_at IS NULL`, and a poll heartbeat is separate from a take - a harness name
//! is not evidence that a drainer is running.

use ds_core::owner::OwnerPush;

const NOW: f64 = 1_000_000.0;

fn store() -> (tempfile::TempDir, OwnerPush) {
    let directory = tempfile::tempdir().expect("tempdir");
    let store = OwnerPush::open(directory.path().join("owner-push.db")).expect("open");
    (directory, store)
}

#[test]
fn a_push_is_pending_until_it_is_taken() {
    let (_directory, store) = store();
    let pushed = store
        .push("%2", "the brief", Some("%2"), Some("status"), NOW)
        .expect("push");
    assert_eq!("%2", pushed.pane_id);
    assert_eq!(
        1,
        store.pending(Some("%2")).expect("pending").len(),
        "an undrained message is visible as a pending row"
    );
    let taken = store
        .take("%2", Some("extension"), 20, NOW + 5.0)
        .expect("take");
    assert_eq!(1, taken.len());
    assert_eq!("the brief", taken[0].body);
    assert_eq!(Some(NOW + 5.0), taken[0].taken_at);
    assert_eq!(Some("extension"), taken[0].taken_by.as_deref());
    assert!(
        store.pending(Some("%2")).expect("pending").is_empty(),
        "a taken message is no longer pending"
    );
}

#[test]
fn an_empty_body_is_refused() {
    let (_directory, store) = store();
    let error = store.push("%2", "   ", None, None, NOW).unwrap_err();
    assert!(error.to_string().contains("needs a body"), "{error}");
    assert!(store.pending(None).expect("pending").is_empty());
}

#[test]
fn a_take_is_bounded_and_oldest_first() {
    let (_directory, store) = store();
    for index in 0..5 {
        store
            .push(
                "%2",
                &format!("message {index}"),
                None,
                None,
                NOW + index as f64,
            )
            .expect("push");
    }
    let first = store.take("%2", None, 2, NOW + 10.0).expect("take");
    assert_eq!(2, first.len(), "the limit is honoured");
    assert_eq!("message 0", first[0].body, "oldest first");
    assert_eq!("message 1", first[1].body);
    let rest = store.take("%2", None, 20, NOW + 11.0).expect("take");
    assert_eq!(3, rest.len(), "the rest come next");
}

#[test]
fn receipts_give_timestamps_and_nothing_about_the_content() {
    let (_directory, store) = store();
    let first = store.push("%2", "one", None, None, NOW).expect("push");
    let second = store.push("%2", "two", None, None, NOW).expect("push");
    store.take("%2", None, 1, NOW + 2.0).expect("take");
    let receipts = store
        .receipts(&[first.id, second.id, 9999])
        .expect("receipts");
    assert_eq!(3, receipts.requested);
    assert_eq!(
        2, receipts.returned,
        "an unknown id is ABSENT, not a null row"
    );
    assert!(!receipts.truncated);
    let taken = receipts
        .receipts
        .iter()
        .find(|r| r.id == first.id)
        .expect("first");
    assert_eq!(Some(NOW + 2.0), taken.taken_at);
    let pending = receipts
        .receipts
        .iter()
        .find(|r| r.id == second.id)
        .expect("second");
    assert_eq!(
        None, pending.taken_at,
        "a receipt never consumes what it reports on"
    );
}

#[test]
fn receipts_report_truncation_rather_than_hiding_it() {
    let (_directory, store) = store();
    let ids: Vec<i64> = (1..=150).collect();
    let receipts = store.receipts(&ids).expect("receipts");
    assert_eq!(150, receipts.requested);
    assert!(
        receipts.truncated,
        "more than 100 ids is reported, not silently cut"
    );
    let empty = store.receipts(&[]).expect("receipts");
    assert_eq!(0, empty.requested);
    assert!(!empty.truncated, "asking for nothing is not a truncation");
}

#[test]
fn a_poll_is_a_separate_heartbeat_from_a_take() {
    // A harness name is not evidence: %2 is pi and still went blind for a day because its
    // session started before the extension existed. Only a take proves a drainer runs.
    let (_directory, store) = store();
    store.push("%2", "brief", None, None, NOW).expect("push");
    assert_eq!(None, store.last_poll("%2").expect("poll"));
    assert_eq!(None, store.last_take("%2").expect("take"));
    store.poll("%2", None, NOW + 1.0).expect("poll");
    assert_eq!(Some(NOW + 1.0), store.last_poll("%2").expect("poll"));
    assert_eq!(
        None,
        store.last_take("%2").expect("take"),
        "a poll without a take is not a drain"
    );
    store.take("%2", None, 20, NOW + 2.0).expect("take");
    assert_eq!(Some(NOW + 2.0), store.last_take("%2").expect("take"));
    // The heartbeat is ONE row per pane: a second poll moves it, it does not accumulate.
    store.poll("%2", None, NOW + 3.0).expect("poll");
    assert_eq!(Some(NOW + 3.0), store.last_poll("%2").expect("poll"));
}

#[test]
fn stats_count_pushes_and_measure_drain_latency() {
    let (_directory, store) = store();
    store.push("%2", "a", None, None, NOW).expect("push");
    store.push("%2", "b", None, None, NOW + 10.0).expect("push");
    store.push("%3", "c", None, None, NOW + 20.0).expect("push");
    store.take("%2", None, 20, NOW + 40.0).expect("take");
    let stats = store.stats(3600.0, NOW + 60.0).expect("stats");
    assert_eq!(3, stats.pushed);
    assert_eq!(2, stats.taken);
    assert_eq!(1, stats.pending);
    assert_eq!(
        Some(40.0),
        stats.latency_s.max,
        "the slowest drain sets max"
    );
    // A window that starts after the pushes counts nothing.
    let empty = store.stats(1.0, NOW + 1000.0).expect("stats");
    assert_eq!(0, empty.pushed);
    assert_eq!(None, empty.latency_s.p50);
}

/// t991 finding 1: the Python's `_pct` is banker's-rounded, so the median with TWO
/// samples is the lower one (10.0), not the higher (20.0 as the first version gave).
#[test]
fn percentiles_use_pythons_round_half_to_even() {
    let (_directory, store) = store();
    // Two pushes drained 10 s and 30 s after they were created -> p50 must be the lower.
    let first = store.push("%2", "a", None, None, NOW).expect("push");
    let second = store.push("%2", "b", None, None, NOW).expect("push");
    store.take("%2", None, 1, NOW + 10.0).expect("take");
    store.take("%2", None, 1, NOW + 30.0).expect("take");
    let stats = store.stats(3600.0, NOW + 60.0).expect("stats");
    assert_eq!(2, stats.taken);
    assert_eq!(
        Some(10.0),
        stats.latency_s.p50,
        "n=2: Python gives the LOWER sample (10.0), half-away-from-zero gives 20.0"
    );
    assert_eq!(Some(30.0), stats.latency_s.max);
    let _ = (first, second);
}

/// t991 finding 2: the store creates its own directory, as the Python's constructor does.
#[test]
fn a_first_run_creates_the_directory_it_needs() {
    let directory = tempfile::tempdir().expect("tempdir");
    let nested = directory
        .path()
        .join("sub")
        .join("deeper")
        .join("owner-push.db");
    let store = OwnerPush::open(&nested).expect("open must create the parent directories");
    store
        .push("%2", "first run", None, None, NOW)
        .expect("push");
    assert_eq!(1, store.pending(None).expect("pending").len());
    store.close();
    assert!(
        nested.exists(),
        "the database file exists after the first run"
    );
}

#[test]
fn the_store_reopens_with_its_rows_intact() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("owner-push.db");
    {
        let store = OwnerPush::open(&path).expect("open");
        store.push("%2", "survives", None, None, NOW).expect("push");
        store.close();
    }
    let reopened = OwnerPush::open(&path).expect("reopen");
    let pending = reopened.pending(None).expect("pending");
    assert_eq!(1, pending.len());
    assert_eq!("survives", pending[0].body);
    reopened.close();
}

// ---- the t1180 review findings -------------------------------------------------------------

/// Python `round(x, 2)` rounds the EXACT binary value half-to-even: round(0.125, 2) == 0.12 and
/// round(0.375, 2) == 0.38. The old `(v*100).round()/100` gave 0.13 for the first. The last case
/// is a DIFFERENCE of floats, as production latencies are: (1000000.0 + 1.005) - 1000000.0 is
/// 1.0050000000046566, which Python rounds to 1.01 (checked by running it).
#[test]
fn latency_values_round_like_pythons_round_on_exact_binary_ties() {
    for (latency, expected) in [
        (0.125, 0.12),
        (0.375, 0.38),
        (0.625, 0.62),
        (0.875, 0.88),
        (1.005, 1.01),
    ] {
        let (_directory, store) = store();
        store.push("%2", "x", None, None, NOW).expect("push");
        store.take("%2", None, 20, NOW + latency).expect("take");
        let stats = store.stats(86_400.0, NOW + 10.0).expect("stats");
        assert_eq!(Some(expected), stats.latency_s.max, "max for {latency}");
        assert_eq!(Some(expected), stats.latency_s.p50, "p50 for {latency}");
    }
}

/// Python: `by or pane_id` - an EMPTY `by` is the pane id too.
#[test]
fn an_empty_taker_name_falls_back_to_the_pane_like_the_python() {
    let (_directory, store) = store();
    store.push("%2", "x", None, None, NOW).expect("push");
    let taken = store.take("%2", Some(""), 20, NOW + 1.0).expect("take");
    assert_eq!(Some("%2"), taken[0].taken_by.as_deref());
}

/// Python treats a falsy timestamp (0) as ABSENT in last_poll, last_take and stats.
#[test]
fn a_zero_timestamp_is_absent_not_a_drain() {
    let (_directory, store) = store();
    store.poll("%2", None, 0.0).expect("poll at 0");
    assert_eq!(
        None,
        store.last_poll("%2").expect("last_poll"),
        "a poll at 0 is not a poll"
    );
    store.push("%2", "x", None, None, 0.0).expect("push");
    store.take("%2", None, 20, 0.0).expect("take at 0");
    assert_eq!(
        None,
        store.last_take("%2").expect("last_take"),
        "a take at 0 is not a take"
    );
    let stats = store.stats(86_400.0, 10.0).expect("stats");
    assert_eq!(
        (1, 0, 1),
        (stats.pushed, stats.taken, stats.pending),
        "a take at 0 counts as pending"
    );
}

/// Python commits once: after a take, every row of the batch is taken and returned together.
#[test]
fn a_take_marks_the_whole_batch_in_one_step() {
    let (_directory, store) = store();
    for n in 0..5 {
        store
            .push("%2", &format!("m{n}"), None, None, NOW + f64::from(n))
            .expect("push");
    }
    let taken = store.take("%2", Some("ext"), 3, NOW + 10.0).expect("take");
    assert_eq!(
        vec!["m0", "m1", "m2"],
        taken.iter().map(|p| p.body.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(
        2,
        store.pending(Some("%2")).expect("pending").len(),
        "only the two untaken remain"
    );
    assert!(taken.iter().all(|p| p.taken_at == Some(NOW + 10.0)));
}
