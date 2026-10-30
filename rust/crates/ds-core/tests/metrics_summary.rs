//! T16 slice: the metrics aggregation, against rows (not a live database).

use ds_core::metrics::{self, Event, RETENTION_S, SCHEMA, SCHEMA_VERSION};

fn event(kind: &str, duration_ms: Option<i64>) -> Event {
    Event {
        at: 1_000.0,
        kind: kind.to_string(),
        duration_ms,
        ..Event::default()
    }
}

#[test]
fn the_schema_and_vocabulary_are_the_pythons() {
    assert_eq!(1, SCHEMA_VERSION);
    assert_eq!(30.0 * 24.0 * 3600.0, RETENTION_S);
    assert!(SCHEMA.contains("CREATE TABLE IF NOT EXISTS events"));
    for column in [
        "at",
        "kind",
        "pane_id",
        "mailbox",
        "sender",
        "source",
        "outcome",
        "reason",
        "duration_ms",
        "attempts",
        "bytes",
    ] {
        assert!(
            SCHEMA.contains(column),
            "the events table lost the {column} column"
        );
    }
    for kind in [
        "message.sent",
        "message.failed",
        "message.held",
        "submit.attempt",
        "inbox.sent",
        "inbox.read",
        "inbox.acked",
        "dunk.sent",
        "dunk.skipped",
        "dunk.held",
        "panel.blocking",
        "turn.end.dispatch",
    ] {
        assert!(
            metrics::KINDS.iter().any(|(name, _)| *name == kind),
            "{kind} undocumented"
        );
    }
    assert_eq!(4, metrics::OUTCOMES.len());
}

#[test]
fn a_tally_counts_every_value_and_orders_by_name() {
    let events = vec![
        event("message.sent", None),
        event("message.sent", None),
        event("inbox.read", None),
        event("dunk.sent", None),
    ];
    let tally = metrics::tally(&events, "kind");
    let ordered: Vec<(&String, &i64)> = tally.iter().collect();
    // The Python sorts by frequency then name; a BTreeMap cannot express that by key,
    // so this port returns NAME order (t979 defect 3). The counts are identical; the
    // ORDER is a real difference a consumer comparing serialised output would see, so
    // it is asserted rather than described.
    let names: Vec<&str> = tally.keys().map(String::as_str).collect();
    assert_eq!(
        vec!["dunk.sent", "inbox.read", "message.sent"],
        names,
        "name order, not frequency order"
    );
    assert_eq!(2, *tally.get("message.sent").expect("sent"));
    assert_eq!(1, *tally.get("inbox.read").expect("read"));
    assert_eq!(1, *tally.get("dunk.sent").expect("dunk"));
}

#[test]
fn a_duration_that_was_never_measured_is_none_not_zero() {
    let events = vec![event("message.sent", None), event("inbox.read", Some(250))];
    let summary = metrics::summary(&events);
    assert_eq!(
        None, summary.durations.delivery,
        "no message.sent measured a duration: that is not 'zero ms'"
    );
    let queue = summary.durations.queue_wait.expect("inbox.read measured");
    assert_eq!(1, queue.n);
    assert_eq!(250, queue.max);
}

#[test]
fn percentiles_match_pythons_round_half_to_even_sample_counts() {
    // t979 measured these against the Python's _percentile: every n divisible by 4
    // diverged for p50 while both suites only used n=5 (the one case that agrees).
    let values: Vec<i64> = (1..=4).collect(); // 1 2 3 4
    assert_eq!(
        Some(2),
        metrics::percentile(&values, 0.50),
        "n=4 (Python: 2, old Rust: 3)"
    );
    let values: Vec<i64> = (1..=8).collect();
    assert_eq!(
        Some(4),
        metrics::percentile(&values, 0.50),
        "n=8 (Python: 4, old Rust: 5)"
    );
    let values: Vec<i64> = (1..=12).map(|n| n * 10).collect();
    assert_eq!(
        Some(60),
        metrics::percentile(&values, 0.50),
        "n=12 (Python: 60, old: 70)"
    );
    let values: Vec<i64> = (1..=20).collect();
    assert_eq!(
        Some(18),
        metrics::percentile(&values, 0.90),
        "n=20 p90 (Python 18, old 19)"
    );
    let values: Vec<i64> = (1..=200).collect();
    assert_eq!(
        Some(198),
        metrics::percentile(&values, 0.99),
        "n=200 p99 (Python 198, old 199)"
    );
    // And the case both suites used, which agrees either way.
    let values: Vec<i64> = (10..=50).step_by(10).collect();
    assert_eq!(Some(30), metrics::percentile(&values, 0.50));
}

#[test]
fn percentiles_are_nearest_rank_not_interpolated() {
    let sorted = [10, 20, 30, 40, 50];
    assert_eq!(Some(30), metrics::percentile(&sorted, 0.50));
    assert_eq!(Some(50), metrics::percentile(&sorted, 0.99));
    assert_eq!(Some(10), metrics::percentile(&sorted, 0.0));
    assert_eq!(None, metrics::percentile(&[], 0.5));
}

#[test]
fn the_enter_press_histogram_and_first_press_rate() {
    let mut events = Vec::new();
    for attempts in [1, 1, 1, 2, 3] {
        let mut submit = event("submit.attempt", None);
        submit.attempts = Some(attempts);
        events.push(submit);
    }
    let retries = metrics::summary(&events).retries;
    assert_eq!(Some(5), retries.submits);
    let histogram = retries.enter_presses.expect("presses");
    assert_eq!(Some(&3), histogram.get(&1), "three submits took one press");
    assert_eq!(Some(&1), histogram.get(&3));
    assert_eq!(
        Some(0.6),
        retries.first_press_success_rate,
        "3 of 5 worked first time"
    );
}

#[test]
fn a_window_with_no_submits_reports_none_not_zero() {
    let summary = metrics::summary(&[event("message.sent", Some(10))]);
    assert_eq!(None, summary.retries.submits);
    assert_eq!(None, summary.retries.enter_presses);
    assert_eq!(None, summary.retries.first_press_success_rate);
}

#[test]
fn counts_carry_every_column_the_endpoint_publishes() {
    let mut sent = event("message.sent", Some(5));
    sent.pane_id = Some("%2".to_string());
    sent.sender = Some("%7".to_string());
    sent.mailbox = Some("owner".to_string());
    sent.source = Some("dunk".to_string());
    sent.outcome = Some("ok".to_string());
    sent.reason = Some("draft".to_string());
    let summary = metrics::summary(&[sent]);
    assert_eq!(1, summary.counts.events);
    assert_eq!(Some(&1), summary.counts.by_kind.get("message.sent"));
    assert_eq!(Some(&1), summary.counts.by_pane.get("%2"));
    assert_eq!(Some(&1), summary.counts.by_sender.get("%7"));
    assert_eq!(Some(&1), summary.counts.by_recipient.get("owner"));
    assert_eq!(Some(&1), summary.counts.by_source.get("dunk"));
    assert_eq!(Some(&1), summary.counts.by_outcome.get("ok"));
    assert_eq!(Some(&1), summary.counts.by_reason.get("draft"));
}

#[test]
fn an_empty_window_is_all_zeros_and_nulls() {
    let summary = metrics::summary(&[]);
    assert_eq!(0, summary.counts.events);
    assert!(summary.counts.by_kind.is_empty());
    assert_eq!(None, summary.durations.queue_wait);
    assert_eq!(None, summary.retries.submits);
}
