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

/// The live event store (`dunk_metrics.Metrics`: `__init__`, `record`, `prune`, `earliest`).
/// Writes go through the blocking `db::SyncWriter` (t978), so the flock's synchronous command
/// handlers call it directly. NOT here: the read side (`summary` over a window) is the pure
/// aggregation above, fed by `events_between`.
#[derive(Debug)]
pub struct MetricsStore {
    writer: crate::db::SyncWriter,
    pub enabled: bool,
    pub retention_s: f64,
    /// Per-command counters not yet written (t4706).
    commands: crate::command_stats::Pending,
}

impl MetricsStore {
    /// `Metrics(path)`: creates the directory and the schema. `":memory:"` is an in-memory db.
    pub fn open(
        path: impl AsRef<std::path::Path>,
    ) -> Result<MetricsStore, crate::error::DunkError> {
        let path = path.as_ref();
        if path != std::path::Path::new(":memory:") {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| {
                    crate::error::DunkError::Refused(format!("{}: {e}", parent.display()))
                })?;
            }
        }
        let writer = crate::db::SyncWriter::spawn(path)?;
        writer.call(|conn| conn.execute_batch(SCHEMA).map_err(crate::db::map_sqlite))?;
        writer.call(|conn| {
            conn.execute_batch(crate::command_stats::SCHEMA)
                .map_err(crate::db::map_sqlite)
        })?;
        Ok(MetricsStore {
            writer,
            enabled: true,
            retention_s: RETENTION_S,
            commands: crate::command_stats::Pending::default(),
        })
    }

    /// Count one dispatched command (t4706). The cell stays in memory for its minute; the cells
    /// of earlier minutes are written here, one row per minute and key. Never fails the caller.
    pub fn record_command(
        &self,
        at: f64,
        command: &str,
        role: &str,
        duration_ms: i64,
        bytes: i64,
        ok: bool,
    ) {
        if !self.enabled {
            return;
        }
        let closed = self.commands.add(at, command, role, duration_ms, bytes, ok);
        if closed.is_empty() {
            return;
        }
        let rows = closed.clone();
        let written = self.writer.call(move |conn| {
            let tx = conn.transaction().map_err(crate::db::map_sqlite)?;
            for ((minute, command, role), cell) in &rows {
                tx.execute(
                    "INSERT INTO command_minutes (minute, command, role, count, errors, \
                     total_ms, max_ms, bytes, buckets) VALUES (?,?,?,?,?,?,?,?,?)",
                    rusqlite::params![
                        minute,
                        command,
                        role,
                        cell.count,
                        cell.errors,
                        cell.total_ms,
                        cell.max_ms,
                        cell.bytes,
                        cell.buckets_text()
                    ],
                )
                .map_err(crate::db::map_sqlite)?;
            }
            tx.commit().map_err(crate::db::map_sqlite)
        });
        if written.is_err() {
            self.commands.restore(closed);
        }
    }

    /// Per-command cells lost because failed writes filled the backlog.
    pub fn commands_dropped(&self) -> i64 {
        self.commands.dropped()
    }

    /// The per-command cells of every minute overlapping `[start, end)`: the written rows plus
    /// the ones still in memory.
    pub fn command_cells(
        &self,
        start: f64,
        end: f64,
    ) -> Result<Vec<(crate::command_stats::Key, crate::command_stats::Cell)>, crate::error::DunkError>
    {
        let first = crate::command_stats::minute_of(start);
        let last = crate::command_stats::minute_of(end);
        let mut cells: Vec<(crate::command_stats::Key, crate::command_stats::Cell)> =
            self.writer.call(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT minute, command, role, count, errors, total_ms, max_ms, bytes, \
                     buckets FROM command_minutes WHERE minute >= ? AND minute <= ? \
                     ORDER BY minute",
                    )
                    .map_err(crate::db::map_sqlite)?;
                let rows = stmt
                    .query_map([first, last], |r| {
                        let buckets: String = r.get(8)?;
                        Ok((
                            (r.get(0)?, r.get(1)?, r.get(2)?),
                            crate::command_stats::Cell {
                                count: r.get(3)?,
                                errors: r.get(4)?,
                                total_ms: r.get(5)?,
                                max_ms: r.get(6)?,
                                bytes: r.get(7)?,
                                buckets: crate::command_stats::Cell::buckets_from_text(&buckets),
                            },
                        ))
                    })
                    .map_err(crate::db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(crate::db::map_sqlite)
            })?;
        cells.extend(self.commands.snapshot());
        Ok(cells)
    }

    /// `record`: never fails the caller (telemetry must not break delivery).
    pub fn record(&self, event: &Event) {
        if !self.enabled {
            return;
        }
        let e = event.clone();
        let at = if e.at == 0.0 {
            crate::clock::now_s()
        } else {
            e.at
        };
        let _ =
            self.writer.call(move |conn| {
                conn.execute(
                "INSERT INTO events (at, kind, pane_id, mailbox, sender, source, outcome, reason, \
                 duration_ms, attempts, bytes) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    at, e.kind, e.pane_id, e.mailbox, e.sender, e.source, e.outcome, e.reason,
                    e.duration_ms, e.attempts, e.bytes
                ],
            )
            .map_err(crate::db::map_sqlite)?;
                Ok(())
            });
    }

    /// `prune`: drop events older than the retention, then the oldest beyond `max_events`.
    /// Returns how many rows went.
    pub fn prune(
        &self,
        older_than_s: Option<f64>,
        max_events: i64,
    ) -> Result<i64, crate::error::DunkError> {
        let cutoff = crate::clock::now_s() - older_than_s.unwrap_or(self.retention_s);
        self.writer.call(move |conn| {
            let mut removed = conn
                .execute("DELETE FROM events WHERE at < ?", [cutoff])
                .map_err(crate::db::map_sqlite)? as i64;
            conn.execute(
                "DELETE FROM command_minutes WHERE minute < ?",
                [crate::command_stats::minute_of(cutoff)],
            )
            .map_err(crate::db::map_sqlite)?;
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .map_err(crate::db::map_sqlite)?;
            if total > max_events {
                let over = total - max_events;
                conn.execute(
                    "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY at LIMIT ?)",
                    [over],
                )
                .map_err(crate::db::map_sqlite)?;
                removed += over;
            }
            Ok(removed)
        })
    }

    /// `earliest`: the oldest event time, if any.
    pub fn earliest(&self) -> Result<Option<f64>, crate::error::DunkError> {
        self.writer.call(|conn| {
            conn.query_row("SELECT MIN(at) FROM events", [], |r| r.get(0))
                .map_err(crate::db::map_sqlite)
        })
    }

    /// `_rows(start, end)`: the events in `[start, end)`, oldest first.
    pub fn events_between(
        &self,
        start: f64,
        end: f64,
    ) -> Result<Vec<Event>, crate::error::DunkError> {
        self.writer.call(move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT at, kind, pane_id, mailbox, sender, source, outcome, reason, \
                     duration_ms, attempts, bytes FROM events WHERE at >= ? AND at < ? ORDER BY at",
                )
                .map_err(crate::db::map_sqlite)?;
            let rows = stmt
                .query_map([start, end], |r| {
                    Ok(Event {
                        at: r.get(0)?,
                        kind: r.get(1)?,
                        pane_id: r.get(2)?,
                        mailbox: r.get(3)?,
                        sender: r.get(4)?,
                        source: r.get(5)?,
                        outcome: r.get(6)?,
                        reason: r.get(7)?,
                        duration_ms: r.get(8)?,
                        attempts: r.get(9)?,
                        bytes: r.get(10)?,
                    })
                })
                .map_err(crate::db::map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(crate::db::map_sqlite)
        })
    }
}

/// What each event means. Returned with every summary, so a consumer never guesses.
pub static KINDS: &[(&str, &str)] = &[
    (
        "message.sent",
        "text was typed into a pane and confirmed submitted",
    ),
    (
        "message.failed",
        "text was typed but could not be confirmed submitted",
    ),
    (
        "message.held",
        "a send was not attempted because the pane was not ready",
    ),
    (
        "message.cancelled",
        "a queued send was withdrawn before delivery",
    ),
    (
        "submit.attempt",
        "one confirm-and-retry sequence, with the Enter presses it took",
    ),
    ("inbox.sent", "a message was written to a mailbox"),
    (
        "inbox.read",
        "a recipient read a message, with how long it had waited",
    ),
    (
        "inbox.acked",
        "a recipient finished with a message, with how long it held it",
    ),
    ("dunk.sent", "a scheduled send went out"),
    (
        "dunk.skipped",
        "backpressure skipped a send: pending or unrecognised input",
    ),
    (
        "dunk.held",
        "a scheduled send was held because the pane was not ready",
    ),
    (
        "panel.blocking",
        "a dialog was open with a delivery waiting behind it",
    ),
    (
        "turn.end.dispatch",
        "the turn-end trigger fired: a lane that ended a turn was \
                          prompted, or skipped with the reason it named",
    ),
    // t1011: the map was 13 of the recorded kinds. These thirteen (each meaning taken from
    // the record() call sites) close the gap; the test asserts this map EQUALS the Python's.
    (
        "turn.start",
        "a lane declared the start of a turn (source names the producer)",
    ),
    (
        "turn.end",
        "a lane declared the end of a turn, with the reason the trigger gave",
    ),
    (
        "turn.expired",
        "a turn's TTL ran out without an end: the agent declared work and \
                      never reported finishing",
    ),
    (
        "identity.mismatch",
        "a caller's pane/pid claim did not match the live binding; the \
                           request was refused as unverified",
    ),
    (
        "inbox.archived",
        "a message was aged out of a mailbox by retention",
    ),
    (
        "message.undeliverable",
        "a queued send was given up on: the pane was dead or the \
                               message could not be delivered at all",
    ),
    (
        "next_job.decision",
        "the turn-end trigger decided what a finished lane should do next: \
                           claim, prompt, or skip with a reason",
    ),
    (
        "pane.respawn",
        "a managed pane's process was replaced in place (respawn-pane)",
    ),
    (
        "pane.respawn.rollback",
        "a respawn was rolled back to the previous argv after the new \
                               command failed to come up",
    ),
    (
        "stuck.recover",
        "the stuck-message machine pressed Enter on an unsubmitted composer, \
                       with the attempt number",
    ),
    (
        "autocodeword",
        "the desk auto-entered a codeword answer into the running game (the \
                      gameplay bridge's own event)",
    ),
    (
        "loadstate",
        "a save slot was loaded through the gameplay bridge, verified or not",
    ),
    (
        "savestate",
        "a save slot was written through the gameplay bridge",
    ),
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

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Retries {
    /// How many submits took 1, 2, 3... Enter presses; `None` when none measured.
    pub enter_presses: Option<BTreeMap<i64, i64>>,
    /// How many submits were measured at all (`None`, not 0, when none were).
    pub submits: Option<i64>,
    /// Share of submits that worked first time, rounded to 4 places.
    pub first_press_success_rate: Option<f64>,
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

#[cfg(test)]
mod command_tests {
    use super::*;

    fn written(store: &MetricsStore) -> i64 {
        store
            .writer
            .call(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_minutes", [], |r| r.get(0))
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap_or(-1)
    }

    /// A write that fails keeps the cells, is retried once per minute and not on every request,
    /// and loses nothing once the database answers again.
    #[test]
    fn a_failing_command_write_is_retried_once_per_minute_and_keeps_the_cells() {
        let store = MetricsStore::open(":memory:").unwrap();
        store.record_command(600.0, "task_list", "worker", 3, 100, true);
        store
            .writer
            .call(|conn| {
                conn.execute_batch("ALTER TABLE command_minutes RENAME TO gone")
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap();
        // Minute 11 closes minute 10; the write fails and the cell is kept.
        store.record_command(660.0, "roster", "owner", 1, 10, true);
        assert_eq!(
            1,
            store
                .commands
                .snapshot()
                .iter()
                .filter(|(k, _)| k.0 == 10)
                .count()
        );
        store
            .writer
            .call(|conn| {
                conn.execute_batch("ALTER TABLE gone RENAME TO command_minutes")
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap();
        // The database is back, but the same minute makes no second attempt.
        store.record_command(661.0, "roster", "owner", 1, 10, true);
        assert_eq!(0, written(&store));
        // The next minute writes everything that was held.
        store.record_command(720.0, "roster", "owner", 1, 10, true);
        assert_eq!(2, written(&store));
        let counts: i64 = store
            .command_cells(0.0, 800.0)
            .unwrap()
            .iter()
            .map(|(_, cell)| cell.count)
            .sum();
        assert_eq!(4, counts);
    }
}
