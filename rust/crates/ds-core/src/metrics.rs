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
    writer: std::sync::Arc<crate::db::SyncWriter>,
    pub enabled: bool,
    pub retention_s: f64,
    /// Per-command counters not yet written (t4706).
    commands: std::sync::Arc<crate::command_stats::Pending>,
    /// Wakes the thread that writes complete counter cells. Sending never blocks.
    command_wake: std::sync::mpsc::SyncSender<()>,
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
        let writer = std::sync::Arc::new(crate::db::SyncWriter::spawn(path)?);
        writer.call(|conn| conn.execute_batch(SCHEMA).map_err(crate::db::map_sqlite))?;
        writer.call(|conn| {
            conn.execute_batch(crate::command_stats::SCHEMA)
                .map_err(crate::db::map_sqlite)
        })?;
        let commands = std::sync::Arc::new(crate::command_stats::Pending::default());
        let (command_wake, wakes) = std::sync::mpsc::sync_channel::<()>(1);
        {
            // The only code that waits for the database on behalf of the counters. It ends when
            // the store, and with it the sender, is dropped.
            let writer = std::sync::Arc::clone(&writer);
            let commands = std::sync::Arc::clone(&commands);
            let _ = std::thread::Builder::new()
                .name("ds-command-counters".into())
                .spawn(move || {
                    while wakes.recv().is_ok() {
                        if let Some(line) = flush_command_cells(&writer, &commands) {
                            tracing::warn!("{line}");
                        }
                    }
                });
        }
        Ok(MetricsStore {
            writer,
            enabled: true,
            retention_s: RETENTION_S,
            commands,
            command_wake,
        })
    }

    /// Count one dispatched command (t4706). This is on the request path, so it only adds to
    /// an in-memory cell and, once a minute, wakes the writing thread without waiting for it:
    /// a slow, locked or failing metrics database never delays a command.
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
        if self.commands.add(at, command, role, duration_ms, bytes, ok) {
            // A full channel means a wake-up is already waiting; nothing is lost.
            let _ = self.command_wake.try_send(());
        }
    }

    /// Write the complete cells now, on this thread (tests drive the write themselves).
    #[cfg(test)]
    fn flush_commands(&self) -> Option<String> {
        flush_command_cells(&self.writer, &self.commands)
    }

    /// Per-command cells lost because the backlog was full.
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
        // The cells still in memory are read inside the writer's job: a counter write commits
        // and releases its cells in one job too, so a cell is never seen twice or not at all.
        let pending = std::sync::Arc::clone(&self.commands);
        let cells: Vec<(crate::command_stats::Key, crate::command_stats::Cell)> =
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
                let mut cells = rows
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(crate::db::map_sqlite)?;
                cells.extend(pending.snapshot());
                Ok(cells)
            })?;
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

/// How long a command-counter write waits for a locked database before it counts as failed.
pub const COMMAND_WRITE_BUSY: std::time::Duration = std::time::Duration::from_millis(50);

/// Write every complete cell, waiting at most `COMMAND_WRITE_BUSY` for a locked database. A
/// failed write keeps its cells (bounded) for the next minute. Returns the one log line an
/// outage gets when it starts and when it ends. Runs on the counter thread, never in a request.
fn flush_command_cells(
    writer: &crate::db::SyncWriter,
    pending: &std::sync::Arc<crate::command_stats::Pending>,
) -> Option<String> {
    let rows = pending.take_closed();
    if rows.is_empty() {
        return None;
    }
    let committed = std::sync::Arc::clone(pending);
    let written = writer.call(move |conn| {
        let usual: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .map_err(crate::db::map_sqlite)?;
        conn.busy_timeout(COMMAND_WRITE_BUSY)
            .map_err(crate::db::map_sqlite)?;
        let outcome = insert_command_rows(conn, &rows).map(|()| committed.write_succeeded());
        let _ = conn.busy_timeout(std::time::Duration::from_millis(usual.max(0) as u64));
        outcome
    });
    match written {
        Ok(ended) => ended.map(|dropped| {
            format!(
                "command counters: metrics.db accepts writes again; {dropped} cells were \
                 dropped during the outage"
            )
        }),
        Err(error) => pending.write_failed().then(|| {
            format!(
                "command counters: metrics.db write failed ({error}); keeping at most {} cells \
                 and retrying once per minute",
                crate::command_stats::MAX_BACKLOG
            )
        }),
    }
}

fn insert_command_rows(
    conn: &mut rusqlite::Connection,
    rows: &[(crate::command_stats::Key, crate::command_stats::Cell)],
) -> Result<(), crate::error::DunkError> {
    let tx = conn.transaction().map_err(crate::db::map_sqlite)?;
    for ((minute, command, role), cell) in rows {
        tx.execute(
            "INSERT INTO command_minutes (minute, command, role, count, errors, total_ms, \
             max_ms, bytes, buckets) VALUES (?,?,?,?,?,?,?,?,?)",
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
    use std::time::{Duration, Instant};

    fn written(store: &MetricsStore) -> i64 {
        store
            .writer
            .call(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_minutes", [], |r| r.get(0))
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap_or(-1)
    }

    fn counted(store: &MetricsStore) -> i64 {
        store
            .command_cells(0.0, 6000.0)
            .unwrap()
            .iter()
            .map(|(_, cell)| cell.count)
            .sum()
    }

    fn rename(store: &MetricsStore, sql: &'static str) {
        store
            .writer
            .call(move |conn| conn.execute_batch(sql).map_err(crate::db::map_sqlite))
            .unwrap();
    }

    /// A write that fails keeps the cells, is asked for again only in the next minute, and
    /// loses nothing once the database answers again.
    #[test]
    fn a_failing_command_write_is_retried_once_per_minute_and_keeps_the_cells() {
        let store = MetricsStore::open(":memory:").unwrap();
        store
            .commands
            .add(600.0, "task_list", "worker", 3, 100, true);
        rename(&store, "ALTER TABLE command_minutes RENAME TO gone");
        assert!(store.commands.add(660.0, "roster", "owner", 1, 10, true));
        let line = store.flush_commands().expect("the outage starts");
        assert!(line.contains("write failed"), "{line}");
        rename(&store, "ALTER TABLE gone RENAME TO command_minutes");
        assert_eq!(2, counted(&store), "the failed cells are kept");
        assert!(
            !store.commands.add(661.0, "roster", "owner", 1, 10, true),
            "no second write is asked for in the minute of the failure"
        );
        assert!(store.commands.add(720.0, "roster", "owner", 1, 10, true));
        let line = store.flush_commands().expect("the outage ends");
        assert!(line.contains("accepts writes again; 0 cells"), "{line}");
        assert_eq!(2, written(&store));
        assert_eq!(4, counted(&store));
    }

    /// A locked database fails the write within the short wait, and an outage gets one line
    /// when it starts and one when it ends, however many minutes it lasts.
    #[test]
    fn a_locked_database_fails_fast_and_an_outage_logs_its_start_and_end_once() {
        let dir = crate::testsupport::dir("metrics-command-outage");
        let store = MetricsStore::open(dir.path().join("metrics.db")).unwrap();
        let other = rusqlite::Connection::open(dir.path().join("metrics.db")).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();

        store
            .commands
            .add(600.0, "task_list", "worker", 3, 100, true);
        store.commands.add(660.0, "roster", "owner", 1, 10, true);
        let started = Instant::now();
        let line = store.flush_commands().expect("the outage starts");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(line.contains("write failed"), "{line}");

        store.commands.add(720.0, "roster", "owner", 1, 10, true);
        assert_eq!(None, store.flush_commands(), "no second line");

        other.execute_batch("COMMIT").unwrap();
        store.commands.add(780.0, "roster", "owner", 1, 10, true);
        let line = store.flush_commands().expect("the outage ends");
        assert!(line.contains("accepts writes again; 0 cells"), "{line}");
        store.commands.add(840.0, "roster", "owner", 1, 10, true);
        assert_eq!(None, store.flush_commands(), "a healthy write logs nothing");
        assert_eq!(4, written(&store));
        assert_eq!(5, counted(&store), "nothing was lost");
    }

    /// The request path never waits for the writer: with the writer busy for most of a second,
    /// counting a command across a minute boundary returns at once, and the cells are written
    /// when the writer is free again.
    #[test]
    fn a_stalled_writer_does_not_delay_a_command() {
        let store = std::sync::Arc::new(MetricsStore::open(":memory:").unwrap());
        let (entered, is_in) = std::sync::mpsc::channel();
        let stalled = {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .writer
                    .call(move |_| {
                        let _ = entered.send(());
                        std::thread::sleep(Duration::from_millis(800));
                        Ok(())
                    })
                    .unwrap();
            })
        };
        is_in.recv().unwrap();

        let started = Instant::now();
        store.record_command(600.0, "task_list", "worker", 3, 100, true);
        store.record_command(660.0, "roster", "owner", 1, 10, true);
        store.record_command(661.0, "roster", "owner", 1, 10, true);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(300), "counting took {took:?}");

        stalled.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while written(&store) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            1,
            written(&store),
            "the complete minute is written afterwards"
        );
        assert_eq!(3, counted(&store));
    }
}
