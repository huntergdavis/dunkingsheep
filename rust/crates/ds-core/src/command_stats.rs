//! Per-command request counters, aggregated per minute (t4706).
//!
//! The daemon logged only slow requests, so nobody could say how often a command ran or how
//! long it usually took. Every dispatched command now adds to one in-memory cell per
//! `(minute, command, caller role)`; a cell is written to `metrics.db` once its minute has
//! passed. That is one row per minute and key, never one row per call.
//!
//! Durations are kept as a fixed histogram, so the reported percentiles are the upper bound of
//! the bucket that holds the rank (capped at the largest duration seen), not exact values.
//!
//! NOT here: web routes (their own slice), and the cells of the minute in which the daemon
//! stops, which are lost with the process.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Map, Value};

/// Upper bounds of the duration buckets, in milliseconds. One more bucket holds the rest.
pub const BOUNDS_MS: [i64; 13] = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000];
const BUCKETS: usize = BOUNDS_MS.len() + 1;

/// The label for a command name that is not in the registry: caller-supplied names must not
/// become unbounded keys.
pub const UNKNOWN_COMMAND: &str = "(unknown)";
/// The role label when the caller's role could not be resolved.
pub const UNRESOLVED_ROLE: &str = "unresolved";

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS command_minutes (
    minute   INTEGER NOT NULL,
    command  TEXT NOT NULL,
    role     TEXT NOT NULL,
    count    INTEGER NOT NULL,
    errors   INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    max_ms   INTEGER NOT NULL,
    bytes    INTEGER NOT NULL,
    buckets  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS command_minutes_by_minute ON command_minutes (minute);";

/// `(minute since the epoch, command, caller role)`.
pub type Key = (i64, String, String);

/// What one key saw in one minute.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    pub count: i64,
    /// Requests answered with an error; refusals count.
    pub errors: i64,
    pub total_ms: i64,
    pub max_ms: i64,
    pub bytes: i64,
    pub buckets: [i64; BUCKETS],
}

impl Cell {
    fn add(&mut self, duration_ms: i64, bytes: i64, ok: bool) {
        let duration_ms = duration_ms.max(0);
        self.count += 1;
        self.errors += i64::from(!ok);
        self.total_ms += duration_ms;
        self.max_ms = self.max_ms.max(duration_ms);
        self.bytes += bytes.max(0);
        let bucket = BOUNDS_MS
            .iter()
            .position(|bound| duration_ms <= *bound)
            .unwrap_or(BUCKETS - 1);
        self.buckets[bucket] += 1;
    }

    fn merge(&mut self, other: &Cell) {
        self.count += other.count;
        self.errors += other.errors;
        self.total_ms += other.total_ms;
        self.max_ms = self.max_ms.max(other.max_ms);
        self.bytes += other.bytes;
        for (mine, theirs) in self.buckets.iter_mut().zip(other.buckets) {
            *mine += theirs;
        }
    }

    /// The stored form of the histogram: a JSON array of counts.
    pub fn buckets_text(&self) -> String {
        json!(self.buckets).to_string()
    }

    /// Read the stored histogram back. A row this build cannot read keeps its totals and loses
    /// only its percentiles.
    pub fn buckets_from_text(text: &str) -> [i64; BUCKETS] {
        let mut buckets = [0; BUCKETS];
        if let Ok(values) = serde_json::from_str::<Vec<i64>>(text) {
            for (slot, value) in buckets.iter_mut().zip(values) {
                *slot = value;
            }
        }
        buckets
    }

    /// The upper bound of the bucket holding the `fraction` rank, capped at `max_ms`.
    fn percentile_ms(&self, fraction: f64) -> Option<i64> {
        let total: i64 = self.buckets.iter().sum();
        if total <= 0 {
            return None;
        }
        let rank = ((fraction * total as f64).ceil() as i64).clamp(1, total);
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= rank {
                let bound = BOUNDS_MS.get(index).copied().unwrap_or(self.max_ms);
                return Some(bound.min(self.max_ms));
            }
        }
        Some(self.max_ms)
    }
}

/// The most cells kept after failed writes. Past it the oldest are dropped, so an outage of the
/// metrics database cannot grow the daemon's memory or the size of the next write without limit.
pub const MAX_BACKLOG: usize = 4096;

#[derive(Debug, Default)]
struct PendingState {
    cells: BTreeMap<Key, Cell>,
    /// The minute in which complete cells were last handed out for writing.
    handed_out: Option<i64>,
    /// Cells dropped because the backlog was full.
    dropped: i64,
}

/// The cells not yet written: the current minute, and any earlier one until it is written.
#[derive(Debug, Default)]
pub struct Pending(Mutex<PendingState>);

impl Pending {
    fn state(&self) -> std::sync::MutexGuard<'_, PendingState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Count one request, and hand back the cells of every minute before `at`'s: they are
    /// complete, and the caller writes them. Cells are handed out at most once per minute, so a
    /// write that keeps failing is retried once a minute and not on every request.
    pub fn add(
        &self,
        at: f64,
        command: &str,
        role: &str,
        duration_ms: i64,
        bytes: i64,
        ok: bool,
    ) -> Vec<(Key, Cell)> {
        let minute = minute_of(at);
        let mut state = self.state();
        state
            .cells
            .entry((minute, command.to_string(), role.to_string()))
            .or_default()
            .add(duration_ms, bytes, ok);
        if state.handed_out == Some(minute) {
            return Vec::new();
        }
        let open = state
            .cells
            .split_off(&(minute, String::new(), String::new()));
        let closed: Vec<(Key, Cell)> = std::mem::replace(&mut state.cells, open)
            .into_iter()
            .collect();
        if !closed.is_empty() {
            state.handed_out = Some(minute);
        }
        closed
    }

    /// Put cells back after a failed write; the next minute tries again. Only `MAX_BACKLOG`
    /// cells are kept: the oldest go first and are counted in `dropped`.
    pub fn restore(&self, closed: Vec<(Key, Cell)>) {
        let mut state = self.state();
        for (key, cell) in closed {
            state.cells.entry(key).or_default().merge(&cell);
        }
        while state.cells.len() > MAX_BACKLOG {
            state.cells.pop_first();
            state.dropped += 1;
        }
    }

    /// Cells lost to a full backlog since the daemon started.
    pub fn dropped(&self) -> i64 {
        self.state().dropped
    }

    pub fn snapshot(&self) -> Vec<(Key, Cell)> {
        self.state()
            .cells
            .iter()
            .map(|(key, cell)| (key.clone(), cell.clone()))
            .collect()
    }
}

pub fn minute_of(at: f64) -> i64 {
    if at.is_finite() {
        (at / 60.0).floor() as i64
    } else {
        0
    }
}

/// The `commands` section of the metrics report: every minute that overlaps `[start, end)`,
/// summed per command across roles.
pub fn summarize(rows: impl IntoIterator<Item = (Key, Cell)>, start: f64, end: f64) -> Value {
    let mut commands: BTreeMap<String, (Cell, BTreeMap<String, i64>)> = BTreeMap::new();
    for ((minute, command, role), cell) in rows {
        let begins = minute as f64 * 60.0;
        if begins + 60.0 <= start || begins >= end {
            continue;
        }
        let entry = commands.entry(command).or_default();
        entry.0.merge(&cell);
        *entry.1.entry(role).or_default() += cell.count;
    }
    let by_command: Map<String, Value> = commands
        .into_iter()
        .map(|(command, (cell, roles))| {
            let count = cell.count.max(1);
            (
                command,
                json!({
                    "count": cell.count,
                    "errors": cell.errors,
                    "p50_ms": cell.percentile_ms(0.50),
                    "p95_ms": cell.percentile_ms(0.95),
                    "max_ms": cell.max_ms,
                    "mean_ms": cell.total_ms / count,
                    "bytes": cell.bytes,
                    "mean_bytes": cell.bytes / count,
                    "by_role": roles,
                }),
            )
        })
        .collect();
    json!({
        "note": "per-minute aggregates of every dispatched command; p50_ms and p95_ms are the \
                 upper bound of a histogram bucket (capped at max_ms), errors include refusals, \
                 and bytes is the compact JSON size of the result",
        "bucket_bounds_ms": BOUNDS_MS,
        "by_command": by_command,
    })
}

/// The compact JSON size of a result, without building the text.
pub fn json_size(value: &Value) -> i64 {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    match serde_json::to_writer(&mut count, value) {
        Ok(()) => count.0 as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(minute: i64, command: &str, role: &str) -> Key {
        (minute, command.to_string(), role.to_string())
    }

    #[test]
    fn a_request_stays_pending_until_its_minute_has_passed() {
        let pending = Pending::default();
        assert!(pending
            .add(600.0, "task_list", "worker", 3, 100, true)
            .is_empty());
        assert!(pending
            .add(659.9, "task_list", "worker", 7, 50, false)
            .is_empty());
        let closed = pending.add(660.0, "roster", "owner", 1, 10, true);
        assert_eq!(1, closed.len());
        let (closed_key, cell) = &closed[0];
        assert_eq!(&key(10, "task_list", "worker"), closed_key);
        assert_eq!(
            (2, 1, 10, 7, 150),
            (
                cell.count,
                cell.errors,
                cell.total_ms,
                cell.max_ms,
                cell.bytes
            )
        );
        let open = pending.snapshot();
        assert_eq!(
            vec![key(11, "roster", "owner")],
            open.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
    }

    #[test]
    fn restored_cells_are_handed_out_again() {
        let pending = Pending::default();
        pending.add(600.0, "task_list", "worker", 3, 100, true);
        let closed = pending.add(660.0, "roster", "owner", 1, 10, true);
        pending.restore(closed);
        // Not again within the minute of the failed write: one attempt per minute.
        assert!(pending
            .add(661.0, "roster", "owner", 1, 10, true)
            .is_empty());
        let again = pending.add(720.0, "roster", "owner", 1, 10, true);
        assert_eq!(
            vec![key(10, "task_list", "worker"), key(11, "roster", "owner")],
            again.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_full_backlog_drops_its_oldest_cells_and_counts_them() {
        let pending = Pending::default();
        let closed = (0..MAX_BACKLOG as i64 + 3)
            .map(|minute| (key(minute, "task_list", "worker"), Cell::default()))
            .collect();
        pending.restore(closed);
        let kept = pending.snapshot();
        assert_eq!(MAX_BACKLOG, kept.len());
        assert_eq!(3, kept[0].0 .0, "the oldest minutes went first");
        assert_eq!(3, pending.dropped());
    }

    #[test]
    fn summary_sums_roles_and_reports_bucket_percentiles() {
        let mut fast = Cell::default();
        for _ in 0..18 {
            fast.add(4, 1000, true);
        }
        let mut slow = Cell::default();
        slow.add(120, 4000, true);
        slow.add(7300, 0, false);
        let report = summarize(
            [
                (key(10, "task_list", "worker"), fast),
                (key(11, "task_list", "owner"), slow),
                (key(11, "roster", "owner"), Cell::default()),
                (key(99, "task_list", "worker"), {
                    let mut late = Cell::default();
                    late.add(1, 1, true);
                    late
                }),
            ],
            600.0,
            720.0,
        );
        let task_list = &report["by_command"]["task_list"];
        assert_eq!(20, task_list["count"]);
        assert_eq!(1, task_list["errors"]);
        assert_eq!(5, task_list["p50_ms"]);
        assert_eq!(200, task_list["p95_ms"]);
        assert_eq!(7300, task_list["max_ms"]);
        assert_eq!(22000, task_list["bytes"]);
        assert_eq!(1100, task_list["mean_bytes"]);
        assert_eq!(json!({"owner": 2, "worker": 18}), task_list["by_role"]);
        assert_eq!(Value::Null, report["by_command"]["roster"]["p50_ms"]);
    }

    #[test]
    fn a_percentile_never_exceeds_the_largest_duration_seen() {
        let mut cell = Cell::default();
        cell.add(3, 0, true);
        assert_eq!(Some(3), cell.percentile_ms(0.95));
        let mut over = Cell::default();
        over.add(45_000, 0, true);
        assert_eq!(Some(45_000), over.percentile_ms(0.5));
    }

    #[test]
    fn the_stored_histogram_reads_back() {
        let mut cell = Cell::default();
        cell.add(4, 0, true);
        cell.add(20_000, 0, true);
        assert_eq!(cell.buckets, Cell::buckets_from_text(&cell.buckets_text()));
        assert_eq!([0; BUCKETS], Cell::buckets_from_text("not json"));
    }

    #[test]
    fn json_size_is_the_compact_length() {
        let value = json!({"a": [1, 2, 3], "b": "text"});
        assert_eq!(value.to_string().len() as i64, json_size(&value));
    }
}
