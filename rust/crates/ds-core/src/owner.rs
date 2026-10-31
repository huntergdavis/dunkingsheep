//! Direct owner delivery: the message is in front of the agent, nothing is typed (T16).
//!
//! The owner waited seven minutes for a message because it was written to an inbox and
//! then typed into a pane behind a typing guard. He said the obvious thing: there was
//! never a need to wait for agent input to clear - a message can be pushed and read.
//!
//! So this is a PUSH channel with no inbox row and no keystrokes:
//!
//! > owner_push(target, body) - stores the message for a pane, returns its id
//! > owner_take(pane) - the harness extension drains what is waiting
//!
//! The harness side polls `owner_take` about once a second and injects what it finds into
//! the conversation. Nothing is typed, so no guard can hold it, and the send path cannot
//! fail silently: an undrained message is visible as a row with `taken_at IS NULL`.
//!
//! Ported from `dunk_owner.py`. The write path is [`crate::db::SyncWriter`] (t978), so the
//! methods keep the Python's blocking shape, and reads go through a `ReadPool`.

use std::path::Path;

use serde::Serialize;

use crate::db::{map_sqlite, ReadPool, SyncWriter};
use crate::error::DunkError;

/// The schema, verbatim from `dunk_owner.SCHEMA`.
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS owner_push (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id TEXT NOT NULL,
    target TEXT,
    subject TEXT,
    body TEXT NOT NULL,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT
);
CREATE INDEX IF NOT EXISTS owner_push_pane ON owner_push(pane_id, taken_at);

-- A DRAIN HEARTBEAT, separate from the messages. A take proves a message was read once;
-- it does not prove a poller is running now (a manual `owner take` would confer 'healthy'
-- for ever) and an idle poller draining EMPTY queues writes no row at all (so a healthy
-- lane would look dead). The extension records every successful poll here, empty or not,
-- and that is what capability reads.
CREATE TABLE IF NOT EXISTS owner_poll (
    pane_id   TEXT NOT NULL,
    at        REAL NOT NULL,
    by        TEXT,
    PRIMARY KEY (pane_id)
);";

/// One pushed message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Push {
    pub id: i64,
    pub pane_id: String,
    pub target: Option<String>,
    pub subject: Option<String>,
    pub body: String,
    pub created_at: f64,
    pub taken_at: Option<f64>,
    pub taken_by: Option<String>,
}

/// A receipt for one push: exactly the two timestamps, and nothing about the content.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Receipt {
    pub id: i64,
    pub pane_id: String,
    pub created_at: f64,
    pub taken_at: Option<f64>,
}

/// What the push path reports back: the id and when it was stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pushed {
    pub id: i64,
    pub pane_id: String,
    pub at: f64,
}

/// The store. One writer actor, one read pool, for the life of the daemon.
pub struct OwnerPush {
    writer: SyncWriter,
    readers: ReadPool,
}

impl std::fmt::Debug for OwnerPush {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnerPush")
            .field("path", &self.writer.path())
            .finish()
    }
}

impl OwnerPush {
    /// Open (or create) the database and install the schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DunkError> {
        let path = path.as_ref();
        // t991 finding 2: the Python's constructor does `os.makedirs(dirname, exist_ok=True)`,
        // so a first run with no config directory yet works. Without this the owner channel
        // comes up dead instead of creating its file (measured: "unable to open database
        // file" and nothing created).
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let writer = SyncWriter::spawn(path)?;
        writer.call(|connection| {
            connection.execute_batch(SCHEMA).map_err(map_sqlite)?;
            Ok(())
        })?;
        let readers = ReadPool::open(path, 2)?;
        Ok(OwnerPush { writer, readers })
    }

    pub fn close(self) {
        self.writer.shutdown();
    }

    /// Store a message for a pane. An empty body is refused: an owner message with no
    /// content is a bug at the caller, and storing it would look delivered for ever.
    pub fn push(
        &self,
        pane_id: &str,
        body: &str,
        target: Option<&str>,
        subject: Option<&str>,
        now: f64,
    ) -> Result<Pushed, DunkError> {
        let body = body.trim().to_string(); // owned: the closure outlives this frame
        if body.is_empty() {
            return Err(DunkError::Refused(
                "an owner message needs a body".to_string(),
            ));
        }
        let pane_id = pane_id.to_string();
        let pane_for_answer = pane_id.clone();
        let target = target.map(str::to_string);
        let subject = subject.map(str::to_string);
        let id = self.writer.call(move |connection| {
            connection
                .execute(
                    "INSERT INTO owner_push (pane_id, target, subject, body, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (
                        pane_id.as_str(),
                        target.as_deref(),
                        subject.as_deref(),
                        body.as_str(),
                        now,
                    ),
                )
                .map_err(map_sqlite)?;
            Ok(connection.last_insert_rowid())
        })?;
        // t991 note: no second round trip. The caller already knows the pane it pushed to;
        // re-reading it cost one writer hop per owner message on the latency-critical path.
        Ok(Pushed {
            id,
            pane_id: pane_for_answer,
            at: now,
        })
    }

    /// Hand over what is waiting for this pane, oldest first, and mark it taken.
    pub fn take(
        &self,
        pane_id: &str,
        by: Option<&str>,
        limit: i64,
        now: f64,
    ) -> Result<Vec<Push>, DunkError> {
        let pane = pane_id.to_string();
        // Python `by or pane_id`: an EMPTY by is the pane id too.
        let taken_by = by
            .filter(|name| !name.is_empty())
            .unwrap_or(pane_id)
            .to_string();
        let rows = self.writer.call(move |connection| {
            // ONE transaction, like the Python's single commit: a failure part-way must not leave a
            // prefix of the batch marked taken that the caller never receives (t1180 review).
            let transaction = connection.transaction().map_err(map_sqlite)?;
            let rows: Vec<Push> = {
                let mut statement = transaction
                    .prepare(
                        "SELECT * FROM owner_push WHERE pane_id = ?1 AND taken_at IS NULL \
                              ORDER BY id LIMIT ?2",
                    )
                    .map_err(map_sqlite)?;
                let found = statement
                    .query_map((pane.as_str(), limit), row_to_push)
                    .map_err(map_sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite)?;
                found
            };
            for row in &rows {
                transaction
                    .execute(
                        "UPDATE owner_push SET taken_at = ?1, taken_by = ?2 WHERE id = ?3",
                        (now, taken_by.as_str(), row.id),
                    )
                    .map_err(map_sqlite)?;
            }
            transaction.commit().map_err(map_sqlite)?;
            Ok(rows
                .into_iter()
                .map(|row| Push {
                    taken_at: Some(now),
                    taken_by: Some(taken_by.clone()),
                    ..row
                })
                .collect::<Vec<_>>())
        })?;
        Ok(rows)
    }

    /// Receipts for specific pushes: the two timestamps, nothing about the content.
    ///
    /// Unknown ids are ABSENT from the result rather than null: "no such push" and "a push
    /// with no timestamps" are different answers. Counts travel WITH the rows (the t319
    /// review: a shared attribute was stale on the empty path and two concurrent calls
    /// could report each other's counts).
    pub fn receipts(&self, push_ids: &[i64]) -> Result<Receipts, DunkError> {
        let requested: Vec<i64> = push_ids.iter().copied().filter(|id| *id > 0).collect();
        let ids: Vec<i64> = requested.iter().copied().take(100).collect();
        let mut meta = Receipts {
            requested: requested.len(),
            returned: 0,
            truncated: requested.len() > ids.len(),
            receipts: Vec::new(),
        };
        if ids.is_empty() {
            return Ok(meta);
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id, pane_id, created_at, taken_at FROM owner_push \
                           WHERE id IN ({placeholders}) ORDER BY id"
        );
        meta.receipts = self.readers.with(move |connection| {
            let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
            let rows: Vec<Receipt> = statement
                .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                    Ok(Receipt {
                        id: row.get(0)?,
                        pane_id: row.get(1)?,
                        created_at: row.get(2)?,
                        taken_at: row.get(3)?,
                    })
                })
                .map_err(map_sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite)?;
            Ok(rows)
        })?;
        meta.returned = meta.receipts.len();
        Ok(meta)
    }

    /// Record a successful poll by the lane's own harness (EMPTY POLLS COUNT).
    pub fn poll(&self, pane_id: &str, by: Option<&str>, now: f64) -> Result<Poll, DunkError> {
        let pane = pane_id.to_string();
        let who = by
            .map(str::to_string)
            .unwrap_or_else(|| "harness".to_string());
        let who_for_write = who.clone();
        let pane_for_write = pane.clone();
        self.writer.call(move |connection| {
            connection
                .execute(
                    "INSERT INTO owner_poll (pane_id, at, by) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(pane_id) DO UPDATE SET at=excluded.at, by=excluded.by",
                    (pane_for_write.as_str(), now, who_for_write.as_str()),
                )
                .map_err(map_sqlite)?;
            Ok(())
        })?;
        Ok(Poll {
            pane_id: pane,
            at: now,
            by: who,
        })
    }

    /// When this pane's harness last polled, or None. THE drain fact.
    pub fn last_poll(&self, pane_id: &str) -> Result<Option<f64>, DunkError> {
        let pane = pane_id.to_string();
        self.readers.with(move |connection| {
            connection
                .query_row(
                    "SELECT at FROM owner_poll WHERE pane_id = ?1",
                    (pane,),
                    |row| row.get::<_, Option<f64>>(0),
                )
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(map_sqlite(other)),
                })
                // Python `row["at"] if row and row["at"] else None`: a 0 is absent, not a poll.
                .map(|value| value.filter(|at| *at != 0.0))
        })
    }

    /// When this pane last drained a push, or None. The OBSERVED drain fact.
    ///
    /// A harness name is not evidence: %2 is pi and still went blind for a day because its
    /// session started before the extension existed and never loaded it (2026-10-24). Only
    /// a take proves a drainer is running.
    pub fn last_take(&self, pane_id: &str) -> Result<Option<f64>, DunkError> {
        let pane = pane_id.to_string();
        self.readers.with(move |connection| {
            let value: Option<Option<f64>> = connection
                .query_row(
                    "SELECT MAX(taken_at) FROM owner_push WHERE pane_id = ?1",
                    (pane,),
                    |row| row.get(0),
                )
                .map_err(map_sqlite)?;
            // a falsy (0) taken_at is absent in the Python too
            Ok(value.flatten().filter(|at| *at != 0.0))
        })
    }

    /// What is waiting, for one pane or the whole fleet.
    pub fn pending(&self, pane_id: Option<&str>) -> Result<Vec<Push>, DunkError> {
        match pane_id {
            Some(pane) => {
                let pane = pane.to_string();
                self.readers.with(move |connection| {
                    let mut statement = connection
                        .prepare(
                            "SELECT * FROM owner_push WHERE pane_id = ?1 \
                                  AND taken_at IS NULL ORDER BY id",
                        )
                        .map_err(map_sqlite)?;
                    let rows = statement
                        .query_map((pane,), row_to_push)
                        .map_err(map_sqlite)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(map_sqlite)?;
                    Ok(rows)
                })
            }
            None => self.readers.with(|connection| {
                let mut statement = connection
                    .prepare("SELECT * FROM owner_push WHERE taken_at IS NULL ORDER BY id")
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map([], row_to_push)
                    .map_err(map_sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite)?;
                Ok(rows)
            }),
        }
    }

    /// Counts and latency percentiles for one window.
    pub fn stats(&self, window_s: f64, now: f64) -> Result<Stats, DunkError> {
        let since = now - window_s;
        self.readers.with(move |connection| {
            let mut statement = connection
                .prepare("SELECT created_at, taken_at FROM owner_push WHERE created_at >= ?1")
                .map_err(map_sqlite)?;
            let rows: Vec<(f64, Option<f64>)> = statement
                .query_map((since,), |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(map_sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite)?;
            let mut latencies: Vec<f64> = rows
                .iter()
                .filter_map(|(created, taken)| {
                    taken
                        .filter(|at| *at != 0.0)
                        .map(|taken| (taken - created).max(0.0))
                })
                .collect();
            latencies.sort_by(|left, right| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            });
            let pending = rows
                .iter()
                .filter(|(_, taken)| taken.map_or(true, |at| at == 0.0))
                .count();
            Ok(Stats {
                pushed: rows.len() as i64,
                taken: latencies.len() as i64,
                pending: pending as i64,
                latency_s: Latency {
                    p50: percentile(&latencies, 0.5),
                    p90: percentile(&latencies, 0.9),
                    max: latencies.last().map(|value| round2(*value)),
                },
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Poll {
    pub pane_id: String,
    pub at: f64,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Receipts {
    pub receipts: Vec<Receipt>,
    pub requested: usize,
    pub returned: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    pub pushed: i64,
    pub taken: i64,
    pub pending: i64,
    pub latency_s: Latency,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Latency {
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
}

/// Python's `round(value, 2)`: the EXACT binary value rounded half-to-even. Formatting to two places
/// is exact in Rust too, so format-then-parse matches; `(v * 100).round() / 100` multiplies in
/// floating point first and rounds half AWAY from zero (round2(0.125) was 0.13, Python 0.12; the
/// t1180 review).
fn round2(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap_or(value)
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    // t991 finding 1: the Python's `_pct` indexes with `int(round(q * (n-1)))`, and
    // Python's round() is HALF-TO-EVEN. `f64::round()` is half-away-from-zero, so the two
    // disagree whenever q*(n-1) lands on .5 - measured n=2 p50: Python 10.0, this 20.0
    // (the median DOUBLED with two samples), n=6 p50 30/40 and p90 50/60, n=10 p50 50/60.
    // The original stats test used exactly two samples and asserted only `max` and the
    // empty-window p50, so it could not see it.
    let index =
        (round_half_even(fraction * (values.len() as f64 - 1.0)) as usize).min(values.len() - 1);
    Some(round2(values[index]))
}

/// `round()` as Python means it: a tie goes to the EVEN neighbour.
///
/// Python's `_pct` indexes with `int(round(q * (n-1)))`, and Python's round() is
/// half-to-even while `f64::round()` is half-away-from-zero - so the two disagree
/// whenever q*(n-1) lands on .5 (t991 finding 1: n=2 p50 was 10.0 in Python and 20.0
/// here, the median doubling). Kept local because this branch predates `metrics.rs`,
/// which carries the same helper; when both land, one of the two should go.
fn round_half_even(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    if (fraction - 0.5).abs() < f64::EPSILON {
        if (floor as i64) % 2 == 0 {
            floor as i64
        } else {
            floor as i64 + 1
        }
    } else {
        value.round() as i64
    }
}

fn row_to_push(row: &rusqlite::Row<'_>) -> rusqlite::Result<Push> {
    Ok(Push {
        id: row.get("id")?,
        pane_id: row.get("pane_id")?,
        target: row.get("target")?,
        subject: row.get("subject")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
        taken_at: row.get("taken_at")?,
        taken_by: row.get("taken_by")?,
    })
}
