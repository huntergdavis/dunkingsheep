//! The append-only metrics event store (T16 slice, `dunk_metrics.py`).
//!
//! Two halves, and this file is honest about which one is here:
//!
//! * **The schema and the vocabulary** - the `events` table, and the KINDS /
//!   OUTCOMES / REASONS maps that are returned with every summary so a consumer
//!   never has to guess what an event means. Complete.
//! * **The aggregation** - `summary`, `tally`, `histogram`, `distribution`,
//!   `percentile`, and the `Event` row type they work on. Complete, pure, and
//!   tested against rows rather than against a live database.
//!
//! **Not wired to `db::Writer`/`db::ReadPool` yet**: those are async (`Writer::call`
//! is `async fn`), so `record`/`prune`/`earliest` need a runtime owner, which is a
//! separate step with its own tests. Saying that here rather than shipping a store
//! that quietly does not write.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: i64 = 1;
/// 30 days, the Python's `RETENTION_S`.
pub const RETENTION_S: f64 = 30.0 * 24.0 * 3600.0;
pub const MAX_EVENTS: i64 = 2_000_000;

/// The table, verbatim from `dunk_metrics.SCHEMA`.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          REAL NOT NULL,
    kind        TEXT NOT NULL,
    pane_id     TEXT,
    mailbox     TEXT,
    sender      TEXT,
    source      TEXT,
    outcome     TEXT,
    reason      TEXT,
    duration_ms INTEGER,
    attempts    INTEGER,
    bytes       INTEGER
);
CREATE INDEX IF NOT EXISTS events_by_time ON events (at);
CREATE INDEX IF NOT EXISTS events_by_kind ON events (kind, at);";

/// What each event means. Returned with every summary, so a consumer never guesses.
pub static KINDS: &[(&str, &str)] = &[
    ("message.sent", "text was typed into a pane and confirmed submitted"),
    ("message.failed", "text was typed but could not be confirmed submitted"),
    ("message.held", "a send was not attempted because the pane was not ready"),
    ("message.cancelled", "a queued send was withdrawn before delivery"),
    ("submit.attempt", "one confirm-and-retry sequence, with the Enter presses it took"),
    ("inbox.sent", "a message was written to a mailbox"),
    ("inbox.read", "a recipient read a message, with how long it had waited"),
    ("inbox.acked", "a recipient finished with a message, with how long it held it"),
    ("dunk.sent", "a scheduled send went out"),
    ("dunk.skipped", "backpressure skipped a send: pending or unrecognised input"),
    ("dunk.held", "a scheduled send was held because the pane was not ready"),
    ("panel.blocking", "a dialog was open with a delivery waiting behind it"),
    ("turn.end.dispatch", "the turn-end trigger fired: a lane that ended a turn was                            prompted, or skipped with the reason it named"),
];

pub static OUTCOMES: &[(&str, &str)] = &[
    ("ok", "completed as intended"),
    ("failed", "attempted and did not complete"),
    ("held", "not attempted; the pane was not ready"),
    ("cancelled", "withdrawn before it was attempted"),
];

pub static REASONS: &[(&str, &str)] = &[
    ("draft", "someone was writing in the input box"),
    (
        "dialog",
        "a dialog was open, waiting for a person to answer",
    ),
    (
        "stranded",
        "our own earlier text was still sitting unsubmitted",
    ),
    (
        "unconsumed",
        "the previous scheduled send is still pending in the input box",
    ),
    (
        "unrecognised",
        "backpressure could not read or recognise the input box",
    ),
    (
        "unconfirmed",
        "the submit could not be confirmed from the pane",
    ),
    (
        "no_open_work",
        "the lane had no CLAIMED (leased) task, so it was left alone",
    ),
    (
        "unread_mail",
        "the lane already had unread mail: it was fed",
    ),
    (
        "unread_unknown",
        "the lane's unread count could not be read, so it was not fed",
    ),
    ("cooldown", "the same pane was prompted moments ago"),
    ("task_cooldown", "every claimed task was prompted recently"),
    (
        "held_delivery_pending",
        "a prompt for the pane was already queued undelivered",
    ),
    ("disabled", "the turn-end trigger is switched off"),
];

/// One row of `events`, as the aggregation sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub at: f64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<i64>,
}

/// Counts by one column, ordered by NAME.
///
/// The Python's `_tally` sorts by frequency then name; this port returns a `BTreeMap`,
/// so the COUNTS are identical and the ORDER is by key. The tie order is therefore not
/// reproduced, which is a difference a consumer comparing serialised output would see -
/// stated here, in the PR body, and asserted by the test (t979 defect 3).
pub fn tally(events: &[Event], column: &str) -> BTreeMap<String, i64> {
    let mut raw: BTreeMap<String, i64> = BTreeMap::new();
    for event in events {
        let value = match column {
            "kind" => Some(event.kind.clone()),
            "sender" => event.sender.clone(),
            "mailbox" => event.mailbox.clone(),
            "pane_id" => event.pane_id.clone(),
            "source" => event.source.clone(),
            "outcome" => event.outcome.clone(),
            "reason" => event.reason.clone(),
            other => panic!("unknown tally column {other}"),
        };
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            *raw.entry(value).or_insert(0) += 1;
        }
    }
    let mut ordered: Vec<(String, i64)> = raw.into_iter().collect();
    ordered.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    ordered.into_iter().collect()
}

/// How many events carried each attempt count, ordered by the count.
pub fn histogram(values: &[i64]) -> Option<BTreeMap<i64, i64>> {
    if values.is_empty() {
        return None;
    }
    let mut out: BTreeMap<i64, i64> = BTreeMap::new();
    for value in values {
        *out.entry(*value).or_insert(0) += 1;
    }
    Some(out)
}

/// Nearest-rank percentile.
///
/// With few samples an interpolated figure would imply a precision the data does not
/// have, which is why the Python rounds and clamps rather than interpolating.
pub fn percentile(sorted_values: &[i64], fraction: f64) -> Option<i64> {
    if sorted_values.is_empty() {
        return None;
    }
    // Python's round() is HALF-TO-EVEN, and f64::round() is half-away-from-zero, so
    // the two disagree exactly where a sample count makes the scaled rank land on .5 -
    // i.e. every n divisible by 4 for the median (4, 8, 12... sample windows). t979
    // measured p50 n=4 as 20 in Python and 30 here before this fix. The formula is the
    // Python's own: round(fraction * n + 0.5) - 1, clamped.
    let scaled = fraction * sorted_values.len() as f64 + 0.5;
    let index = round_half_even(scaled) - 1;
    let index = index.clamp(0, sorted_values.len() as i64 - 1) as usize;
    Some(sorted_values[index])
}

/// `round()` as Python means it: a tie goes to the EVEN neighbour.
///
/// Rust's `f64::round` breaks ties away from zero, so 2.5 -> 3 where Python gives 2.
/// Nearest-rank percentiles then select a different sample, which is a silent numeric
/// divergence in the one aggregation this module exists to port (t979).
fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    let rounded = if (fraction - 0.5).abs() < f64::EPSILON {
        // A tie: pick the even neighbour.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        value.round()
    };
    rounded as i64
}

/// One duration distribution: n, p50, p90, p99, max - or `None` when the window has
/// no event of that kind that measured a duration.
///
/// `None` rather than zeros so a consumer can tell "nothing happened" from "not
/// measured here".
pub fn distribution(events: &[Event], kind: &str) -> Option<Duration5> {
    let mut values: Vec<i64> = events
        .iter()
        .filter(|event| event.kind == kind)
        .filter_map(|event| event.duration_ms)
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(Duration5 {
        n: values.len() as i64,
        p50: percentile(&values, 0.50)?,
        p90: percentile(&values, 0.90)?,
        p99: percentile(&values, 0.99)?,
        max: *values.last()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Duration5 {
    pub n: i64,
    pub p50: i64,
    pub p90: i64,
    pub p99: i64,
    pub max: i64,
}

/// The three blocks `summary()` returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    pub counts: Counts,
    pub durations: Durations,
    pub retries: Retries,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub events: i64,
    pub by_kind: BTreeMap<String, i64>,
    pub by_sender: BTreeMap<String, i64>,
    pub by_recipient: BTreeMap<String, i64>,
    pub by_pane: BTreeMap<String, i64>,
    pub by_source: BTreeMap<String, i64>,
    pub by_outcome: BTreeMap<String, i64>,
    pub by_reason: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Durations {
    pub queue_wait: Option<Duration5>,
    pub delivery: Option<Duration5>,
    pub handling: Option<Duration5>,
    pub blocked: Option<Duration5>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Retries {
    /// How many submits took 1, 2, 3... Enter presses; `None` when none measured.
    pub enter_presses: Option<BTreeMap<i64, i64>>,
    /// How many submits were measured at all (`None`, not 0, when none were).
    pub submits: Option<i64>,
    /// Share of submits that worked first time, rounded to 4 places.
    pub first_press_success_rate: Option<f64>,
}

impl Default for Retries {
    fn default() -> Self {
        Retries {
            enter_presses: None,
            submits: None,
            first_press_success_rate: None,
        }
    }
}

/// Counts and duration distributions for one window of rows.
///
/// The rows are the window: the caller (the wired store, later) selects
/// `at >= start AND at < end ORDER BY at`, which is what the Python's `_rows` does.
pub fn summary(events: &[Event]) -> Summary {
    let counts = Counts {
        events: events.len() as i64,
        by_kind: tally(events, "kind"),
        by_sender: tally(events, "sender"),
        by_recipient: tally(events, "mailbox"),
        by_pane: tally(events, "pane_id"),
        by_source: tally(events, "source"),
        by_outcome: tally(events, "outcome"),
        by_reason: tally(events, "reason"),
    };
    let durations = Durations {
        queue_wait: distribution(events, "inbox.read"),
        delivery: distribution(events, "message.sent"),
        handling: distribution(events, "inbox.acked"),
        blocked: distribution(events, "panel.blocking"),
    };
    let presses: Vec<i64> = events
        .iter()
        .filter(|event| event.kind == "submit.attempt")
        .filter_map(|event| event.attempts)
        .filter(|attempts| *attempts != 0)
        .collect();
    let retries = if presses.is_empty() {
        Retries::default()
    } else {
        let first = presses.iter().filter(|attempts| **attempts == 1).count();
        Retries {
            enter_presses: histogram(&presses),
            submits: Some(presses.len() as i64),
            first_press_success_rate: Some(
                (first as f64 / presses.len() as f64 * 10_000.0).round() / 10_000.0,
            ),
        }
    };
    Summary {
        counts,
        durations,
        retries,
    }
}
