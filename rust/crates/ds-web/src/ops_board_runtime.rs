//! Real operations board and metrics cache: the Rust backend behind `web/app.py`'s
//! `/api/ops-board` reads and refresh and `/api/ops-metrics` history, refresh and board
//! snapshot. It ports `bridges/ops_board.py` (`build`, `compact`, the readers, `BoardCache`)
//! and `bridges/ops_budget.py` (`project`, `build`, `UsageCache`).
//!
//! Every number on the board is read from a record. A source that cannot be read becomes
//! `{"state": "unavailable", "error": <exception class name>}` and keeps no fake zero; the
//! other panels still render. A refresh that fails keeps the last good snapshot with a
//! `refresh_error`, and a cold failure is an error. No model is consulted anywhere.
//!
//! Inputs are explicit, as in `ops_collector`. Nothing here reads the environment or the home
//! directory. The caller supplies one shared daemon client (`SharedDaemon`, the same client
//! `routes::ops_board::SocketOwnerDecisions` uses), an opener for the operations database
//! (`OpenDb`), a clock, a provider usage reader (`UsageReader`), and the paths in
//! `BoardRuntimeConfig`. The timer, the usage workers and the route mounting are started by the
//! composition step, which is outside this slice.
//!
//! Gaps and approximations, kept narrow:
//! - `ops_history::connect` is private, so the operations database is opened through the
//!   injected `OpenDb`. The production composition must supply the schema-initialising opener.
//! - Daemon reads use the board's exception names (`DunkError`, `DaemonUnavailable`,
//!   `TimeoutError`, ...), mapped the same way as `routes::ops_board`, not the collector's
//!   CLI-subprocess names. The daemon client is shared and not reconnected per refresh.
//! - Provider usage is read in process through `ds_core::usage::run`, not by the
//!   `dunkingsheep --json usage` subprocess. Its command timeouts come from the provider spec,
//!   not from the Python 20-second subprocess limit.
//! - Timestamps follow RFC 3339 (`ops_collector::parse_time`), plus naive `YYYY-MM-DD[ T]HH:MM:SS`
//!   and date-only text read as UTC. CPython's other `fromisoformat` forms are rejected here.
//! - Python sorting and dict-key rules are approximated for odd shapes. Dict keys (assignees,
//!   priorities) compare by their JSON text. Mixed-type name ties raise `TypeError`. A string
//!   or mapping where a list is expected is iterated the way `list()` iterates it.
//! - `float()` and `int()` on strings accept what Rust parsing accepts, and underscores are
//!   removed without checking their placement.
//! - The 429 check uses ASCII case folding. Whitespace is Rust's `char::is_whitespace`.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use ds_core::{
    client::{ClientError, FlockClient, UnixTransport},
    usage::{load_config, py_float_repr, run as run_usage},
};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::{
    ops_collector::{
        fromtimestamp_utc, parse_time, py_str, CollectError, Collector, CollectorConfig,
        DaemonCalls, GhCli, SourceError,
    },
    ops_history,
    ops_summary::{self, SummaryError},
    routes::{
        ops_board::{OpsBoardBackend, OpsBoardError, OpsBoardFuture, SocketOwnerDecisions},
        ops_metrics::{OpsMetricsBackend, OpsMetricsError, OpsMetricsFuture},
    },
};

/// Python `TASK_LIST_LIMIT`: the page size and the completeness bound for the task reads.
pub const TASK_LIST_LIMIT: u64 = 1000;
/// Python `ACTIVE_STATUSES`, in the oracle's order.
const ACTIVE_STATUSES: [&str; 3] = ["open", "in_progress", "blocked"];
/// Python `ops_budget.SOURCE`.
const SOURCE: &str = "dunkingsheep --json usage";
/// Python `ops_budget.PROVIDERS`, in the oracle's order.
pub const PROVIDERS: [&str; 4] = ["claude", "codex", "pi", "opencode-go"];
/// Python `ops_metrics.PROJECT`, the project whose subscription prices the budget.
const PROJECT: &str = "poolrad";
/// Python `ops_budget.project` dollars text, verbatim.
const DOLLARS_REASON: &str = "Dollars remaining and per-agent dollar balances are unknown. Subscriptions are fixed prices; allowance percentages are shared provider limits.";
const DAY_S: f64 = 86_400.0;
const HOUR_S: f64 = 3_600.0;
/// Python exception class name recorded for a reader that fails.
type PyError = &'static str;
type Obj = Map<String, Value>;
/// Python `ops_metrics.connect` analogue. The caller owns the schema-initialised connection.
pub type OpenDb = fn(&Path) -> rusqlite::Result<Connection>;
/// Python `time.time()` as a function, for the usage cache.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

static NULL: Value = Value::Null;

/// The wall clock, injectable so tests control every age.
pub trait BoardClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The process clock.
pub struct SystemClock;

impl BoardClock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// One daemon read. A failure carries the exception class name the board records.
pub trait BoardCalls: Send + Sync {
    fn request(&self, command: &str, args: Obj) -> Result<Value, SourceError>;
}

/// The daemon client shared by the board and the owner decisions.
#[derive(Clone)]
pub struct SharedDaemon {
    client: Arc<Mutex<FlockClient<UnixTransport>>>,
}

impl SharedDaemon {
    pub fn new(client: FlockClient<UnixTransport>) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
    }
}

impl BoardCalls for SharedDaemon {
    fn request(&self, command: &str, args: Obj) -> Result<Value, SourceError> {
        let mut client = lock(&self.client);
        client.call(command, args).map_err(board_daemon_error)
    }
}

/// The collector reads the same client with its own CLI-faithful error mapping.
impl DaemonCalls for SharedDaemon {
    fn call(&mut self, command: &str, args: Obj) -> Result<Value, SourceError> {
        let mut client = lock(&self.client);
        <FlockClient<UnixTransport> as DaemonCalls>::call(&mut client, command, args)
    }
}

/// Maps a daemon-client failure to the exception class the Python board records. The same
/// mapping `routes::ops_board` uses for the owner decisions.
fn board_daemon_error(error: ClientError) -> SourceError {
    let python_type = match &error {
        ClientError::Dunk(_) => "DunkError",
        ClientError::Unavailable(_) => "DaemonUnavailable",
        ClientError::Protocol(_) => "ValueError",
        ClientError::Io(io) => match io.kind() {
            std::io::ErrorKind::NotFound => "FileNotFoundError",
            std::io::ErrorKind::PermissionDenied => "PermissionError",
            std::io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
            std::io::ErrorKind::ConnectionReset => "ConnectionResetError",
            std::io::ErrorKind::BrokenPipe => "BrokenPipeError",
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "TimeoutError",
            _ => "OSError",
        },
    };
    SourceError {
        python_type,
        detail: error.to_string(),
    }
}

/// One provider's reading: the CLI row for `provider`, or the text of the failure.
pub trait UsageReader: Send + Sync {
    fn read(&self, provider: &str) -> Result<Value, String>;
}

/// Production reader: the `ds_core` usage runner over the configured `usage.json`.
#[derive(Debug, Clone)]
pub struct CoreUsageReader {
    pub config_path: PathBuf,
    pub max_age_s: i64,
}

impl UsageReader for CoreUsageReader {
    fn read(&self, provider: &str) -> Result<Value, String> {
        let path = self.config_path.to_string_lossy();
        let config = load_config(&path).map_err(|error| error.to_string())?;
        let rows = run_usage(&config, Some(provider), Some(self.max_age_s as f64))
            .map_err(|error| error.to_string())?;
        let [row] = <[Value; 1]>::try_from(rows)
            .map_err(|_| "usage command returned an invalid provider list".to_owned())?;
        let Value::Object(map) = &row else {
            return Err("usage command returned an invalid provider list".to_owned());
        };
        if field(map, "provider").as_str() != Some(provider) {
            return Err("usage command returned the wrong provider".to_owned());
        }
        Ok(row)
    }
}

/// Where usage collection runs. `Threads` is production (one worker per due provider, joined
/// by `stop`). `Inline` runs each due collection before `snapshot` returns, which makes the
/// cadence and backoff tests deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Threads,
    Inline,
}

/// The subscription price for one provider (`subscription_costs`, latest row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub provider: String,
    pub monthly_usd_cents: i64,
    pub observed_at: String,
}

/// Explicit inputs for the board. The composition fills these from the environment.
#[derive(Debug, Clone)]
pub struct BoardRuntimeConfig {
    /// Board refresh cadence in seconds (Python `REFRESH_INTERVAL_S`). The snapshot is stale
    /// after three cadences, and the timer waits this long between refreshes.
    pub interval_s: u64,
    /// The operations database path handed to the injected opener and the history reader.
    pub ops_db: PathBuf,
    /// Host-watch state file read by `ops_summary::summary` for the host sample.
    pub host_watch_state: PathBuf,
    /// OpenCode usage database read by `ops_summary::summary`.
    pub opencode_db: PathBuf,
    /// Collector source locations (message and dunk-metrics databases).
    pub collector: CollectorConfig,
    /// The `gh` executable and checkout used by the metrics refresh.
    pub github: GhCli,
    /// Provider cadence in seconds (`REFRESH_S`).
    pub usage_refresh_s: i64,
    /// Provider backoff sequence in seconds (`BACKOFF_S`).
    pub usage_backoff_s: Vec<i64>,
    /// Maximum reading age before a provider is `stale` (`MAX_AGE_S`).
    pub usage_max_age_s: i64,
    /// Timer period. Equal to `interval_s` in production; tests use a shorter period.
    pub timer_period: Duration,
}

#[derive(Default)]
struct BoardState {
    snapshot: Option<Value>,
    error: Option<String>,
}

struct Timer {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<()>,
    handle: JoinHandle<()>,
}

struct Inner {
    config: BoardRuntimeConfig,
    daemon: SharedDaemon,
    open_db: OpenDb,
    clock: Arc<dyn BoardClock>,
    usage: UsageCache,
    state: Mutex<BoardState>,
    timer: Mutex<Option<Timer>>,
}

/// The production board: the cached snapshot, its refresh timer, and the metrics readers.
/// Cloning shares the same cache, timer and daemon client.
#[derive(Clone)]
pub struct BoardRuntime {
    inner: Arc<Inner>,
}

impl BoardRuntime {
    pub fn new(
        config: BoardRuntimeConfig,
        daemon: SharedDaemon,
        open_db: OpenDb,
        clock: Arc<dyn BoardClock>,
        reader: Arc<dyn UsageReader>,
        dispatch: Dispatch,
    ) -> Self {
        let usage_clock: Clock = {
            let clock = Arc::clone(&clock);
            Arc::new(move || seconds(clock.now()))
        };
        let usage = UsageCache::new(
            config.usage_refresh_s,
            config.usage_backoff_s.clone(),
            reader,
            usage_clock,
            dispatch,
        );
        Self {
            inner: Arc::new(Inner {
                config,
                daemon,
                open_db,
                clock,
                usage,
                state: Mutex::new(BoardState::default()),
                timer: Mutex::new(None),
            }),
        }
    }

    /// Python `_build`: open the database, read every source, and close it.
    fn build_now(&self) -> Result<Value, PyError> {
        let inner = &self.inner;
        let conn = (inner.open_db)(&inner.config.ops_db).map_err(|e| sqlite_error_name(&e))?;
        build(
            &inner.daemon,
            &conn,
            inner.clock.now(),
            &inner.config,
            &inner.usage,
        )
    }

    /// Python `BoardCache.refresh`: a failure keeps the last good snapshot with `refresh_error`
    /// and raises only when there is no snapshot to keep.
    pub fn refresh(&self) -> Result<Value, String> {
        match self.build_now() {
            Ok(board) => {
                let mut state = lock(&self.inner.state);
                state.snapshot = Some(board.clone());
                state.error = None;
                Ok(board)
            }
            Err(name) => {
                let mut state = lock(&self.inner.state);
                state.error = Some(name.to_owned());
                match &state.snapshot {
                    Some(stale) => {
                        let mut stale = stale.clone();
                        set_key(&mut stale, "refresh_error", json!(name));
                        Ok(stale)
                    }
                    None => Err(name.to_owned()),
                }
            }
        }
    }

    /// Python `BoardCache.snapshot`: serve the cache, refreshing it when it is older than
    /// `max_age_s` (default: three cadences), and mark it with `refresh_error` after a failure.
    pub fn snapshot(&self, max_age_s: Option<i64>) -> Result<Value, String> {
        let (current, error) = {
            let state = lock(&self.inner.state);
            (state.snapshot.clone(), state.error.clone())
        };
        let Some(mut current) = current else {
            return self.refresh();
        };
        let age_limit = max_age_s
            .unwrap_or_else(|| i64::try_from(self.inner.config.interval_s).unwrap_or(i64::MAX) * 3);
        let age = age_seconds(
            self.inner.clock.now(),
            value_field(&current, "generated_at"),
        );
        if age.is_some_and(|age| age > age_limit) {
            if let Ok(fresh) = self.refresh() {
                return Ok(fresh);
            }
        }
        if let Some(snapshot) = lock(&self.inner.state).snapshot.clone() {
            current = snapshot;
        }
        if let Some(error) = error {
            set_key(&mut current, "refresh_error", json!(error));
        }
        Ok(current)
    }

    /// Python `BoardCache.start`: start the refresh timer once. Starting a running timer is a
    /// no-op.
    pub fn start_timer(&self) -> std::io::Result<()> {
        let mut timer = lock(&self.inner.timer);
        if timer
            .as_ref()
            .is_some_and(|timer| !timer.handle.is_finished())
        {
            return Ok(());
        }
        let (stop, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done) = mpsc::channel::<()>();
        let runtime = self.clone();
        let period = self.inner.config.timer_period;
        let handle = thread::Builder::new()
            .name("vp-ops-board".to_owned())
            .spawn(move || {
                // Python `while not self._stop.wait(interval): refresh()`: a timeout is a tick,
                // and a stop message or a closed channel ends the loop.
                while let Err(mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(period) {
                    let _ = runtime.refresh();
                }
                let _ = done_tx.send(());
            })?;
        *timer = Some(Timer { stop, done, handle });
        Ok(())
    }

    /// Python `BoardCache.stop`: stop the timer and wait up to five seconds for it to finish.
    /// A timer still running after that is abandoned, as in Python.
    pub fn stop_timer(&self) {
        let Some(timer) = lock(&self.inner.timer).take() else {
            return;
        };
        drop(timer.stop);
        match timer.done.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = timer.handle.join();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    /// Python startup: warm the provider cache, then start the board timer.
    pub fn start(&self) -> std::io::Result<()> {
        let _ = self.inner.usage.snapshot();
        self.start_timer()
    }

    /// Python shutdown: stop the board timer, then the usage workers.
    pub fn stop(&self) {
        self.stop_timer();
        self.inner.usage.stop();
    }

    /// Python `compact`'s peer: the metrics history for the window, from `ops_history`.
    fn history_now(&self, hours: u16) -> Result<Value, OpsMetricsError> {
        let inner = &self.inner;
        ops_history::history(&inner.config.ops_db, inner.clock.now(), hours)
            .map_err(|error| OpsMetricsError::database(sqlite_error_name(&error)))
    }

    /// Python `collect(conn, force_external=True)` followed by `summary(conn)`.
    fn refresh_metrics(&self) -> Result<Value, OpsMetricsError> {
        let inner = &self.inner;
        let mut conn = (inner.open_db)(&inner.config.ops_db)
            .map_err(|error| OpsMetricsError::database(sqlite_error_name(&error)))?;
        let now = inner.clock.now();
        let host = inner.config.host_watch_state.clone();
        let opencode = inner.config.opencode_db.clone();
        let mut collector = Collector::new(
            inner.config.collector.clone(),
            inner.daemon.clone(),
            inner.config.github.clone(),
        );
        collector
            .collect(&mut conn, now, true, |conn, now| {
                ops_summary::summary(conn, now, &host, &opencode)
            })
            .map_err(|error| match error {
                CollectError::Fault(fault) => OpsMetricsError::other(fault.python_type),
                CollectError::Sqlite(error) => OpsMetricsError::database(sqlite_error_name(&error)),
                CollectError::Summary(error) => match error {
                    SummaryError::Sql(error) => {
                        OpsMetricsError::database(sqlite_error_name(&error))
                    }
                    other => OpsMetricsError::other(summary_error_name(&other)),
                },
            })
    }
}

/// The production owner-decision backend: the existing `SocketOwnerDecisions` over the shared
/// daemon client, with the board cache as its refresh source.
pub fn owner_board_backend(runtime: &BoardRuntime) -> SocketOwnerDecisions {
    SocketOwnerDecisions {
        board: Arc::new(runtime.clone()),
        client: Arc::clone(&runtime.inner.daemon.client),
    }
}

impl OpsBoardBackend for BoardRuntime {
    fn refresh<'a>(&'a self) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        let runtime = self.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || runtime.refresh()).await {
                Ok(result) => result.map_err(OpsBoardError::Failure),
                Err(_) => Err(OpsBoardError::Failure("RuntimeError".to_owned())),
            }
        })
    }

    /// The owner's "not now" decision uses the same shared-client binding the route uses, so the
    /// daemon call is not duplicated here.
    fn defer_question<'a>(
        &'a self,
        task_id: &'a str,
    ) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
        let decisions = owner_board_backend(self);
        Box::pin(async move { decisions.defer_question(task_id).await })
    }
}

impl OpsMetricsBackend for BoardRuntime {
    fn history<'a>(&'a self, hours: u16) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        let runtime = self.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || runtime.history_now(hours)).await {
                Ok(result) => result,
                Err(_) => Err(OpsMetricsError::other("RuntimeError")),
            }
        })
    }

    fn refresh<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        let runtime = self.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || runtime.refresh_metrics()).await {
                Ok(result) => result,
                Err(_) => Err(OpsMetricsError::other("RuntimeError")),
            }
        })
    }

    fn board_snapshot<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
        let runtime = self.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || runtime.snapshot(None)).await {
                Ok(result) => result.map_err(OpsMetricsError::other),
                Err(_) => Err(OpsMetricsError::other("RuntimeError")),
            }
        })
    }
}

// -- the board -----------------------------------------------------------------------------

/// Python `build(client, conn, now=...)`: nine readers, `compact`, then the budget.
pub fn build(
    daemon: &dyn BoardCalls,
    conn: &Connection,
    now: DateTime<Utc>,
    config: &BoardRuntimeConfig,
    usage: &UsageCache,
) -> Result<Value, PyError> {
    let mut readings = Obj::new();
    let mut record = |name: &str, reading: Result<Value, PyError>| {
        readings.insert(name.to_owned(), reading.unwrap_or_else(unavailable));
    };
    record("tasks", read_tasks(daemon, now));
    record("panes", read_panes(daemon));
    record("dunks", read_dunks(daemon, now));
    record("health", read_health(daemon));
    record(
        "host",
        read_host(conn, now, &config.host_watch_state, &config.opencode_db),
    );
    record("asks", read_asks(daemon, now));
    record("reports", read_reports(daemon, now));
    record("poolrad", read_poolrad(daemon));
    record("reviews", read_reviews(daemon, now));
    let mut board = compact(&readings, &iso_seconds(now), config.interval_s)?;
    let budget = budget(conn, usage, now, config.usage_max_age_s)?;
    board.insert("budget".to_owned(), budget);
    Ok(Value::Object(board))
}

fn unavailable(error: &str) -> Value {
    json!({"state": "unavailable", "error": error})
}

fn args(value: Value) -> Obj {
    match value {
        Value::Object(map) => map,
        _ => Obj::new(),
    }
}

fn list_items(listing: &Value, key: &str) -> Value {
    match listing {
        Value::Object(map) => field(map, key).clone(),
        other => other.clone(),
    }
}

fn read_tasks(daemon: &dyn BoardCalls, now: DateTime<Utc>) -> Result<Value, PyError> {
    let listing = match daemon.request(
        "task_list",
        args(json!({"include_done": false, "limit": TASK_LIST_LIMIT})),
    ) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    // t996: throughput is read on its own, and its failure must not take the backlog tile.
    let throughput = match daemon.request("task_metrics", args(json!({"window_s": 86_400}))) {
        Ok(metrics) => throughput_of(&metrics, now).unwrap_or_else(throughput_error),
        Err(error) => throughput_error(error.python_type),
    };
    let Value::Array(tasks) = list_items(&listing, "tasks") else {
        return Ok(unavailable("invalid task list"));
    };

    let mut counts = Tally::default();
    let mut rows: Vec<TaskRow> = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let t = obj(task)?;
        let status = py_str(&get_or(t, "status", json!("unknown")));
        counts.add(&json!(status.clone()));
        let assignees_raw = field(t, "assignees");
        let assignees = iter_values(assignees_raw)?;
        let needs_owner = truthy(field(t, "needs_owner")) || contains_str(assignees_raw, "owner")?;
        let blocked_by = iter_values(field(t, "blocked_by"))?;
        let created_at = field(t, "created_at");
        let age_s = age_seconds(now, created_at);
        let fields = ordered([
            ("id", field(t, "id").clone()),
            ("title", field(t, "title").clone()),
            ("status", json!(status.clone())),
            ("priority", field(t, "priority").clone()),
            ("scope", field(t, "scope").clone()),
            ("assignees", json!(assignees)),
            ("created_at", created_at.clone()),
            ("age_s", json!(age_s)),
            (
                "updated_age_s",
                json!(age_seconds(now, field(t, "updated_at"))),
            ),
            (
                "lease_expires_in_s",
                json!(seconds_until(now, field(t, "lease_expires_at"))),
            ),
            ("blocked_by", json!(blocked_by)),
            ("eligible", json!(truthy(field(t, "eligible")))),
            ("claimable", json!(truthy(field(t, "claimable")))),
            ("attempts", field(t, "attempts").clone()),
            ("needs_owner", json!(needs_owner)),
            (
                "needs_owner_question",
                json!(owner_question(field(t, "block_reason"))),
            ),
        ]);
        rows.push(TaskRow {
            fields,
            id: field(t, "id").clone(),
            status,
            priority: field(t, "priority").clone(),
            assignees,
            age_s,
        });
    }

    // DESK_DESIGN 4.1: a blocked task's dependents are the tasks that name it in depends_on.
    let mut dependents: HashMap<String, Vec<Value>> = HashMap::new();
    for task in &tasks {
        let t = obj(task)?;
        for dep in iter_values(field(t, "depends_on"))? {
            dependents
                .entry(py_str(&dep))
                .or_default()
                .push(field(t, "id").clone());
        }
    }
    for row in &mut rows {
        let blocks = match &row.id {
            Value::String(id) => dependents.get(id).cloned().unwrap_or_default(),
            Value::Array(_) | Value::Object(_) => return Err("TypeError"),
            _ => Vec::new(),
        };
        row.fields.insert("blocks".to_owned(), json!(blocks));
    }

    // Python sort key: (priority rank, -(age or 0), str(id)).
    let mut keyed: Vec<((i64, i64, String), TaskRow)> = Vec::with_capacity(rows.len());
    for row in rows {
        let key = (
            priority_rank(&row.priority)?,
            -row.age_s.unwrap_or(0),
            py_str(&row.id),
        );
        keyed.push((key, row));
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let rows: Vec<TaskRow> = keyed.into_iter().map(|(_, row)| row).collect();

    let mut load = Tally::default();
    for row in &rows {
        for assignee in &row.assignees {
            if matches!(assignee, Value::Array(_) | Value::Object(_)) {
                return Err("TypeError");
            }
            load.add(assignee);
        }
    }
    let by_assignee = by_assignee(&load)?;

    let complete = (rows.len() as u64) < TASK_LIST_LIMIT;
    let mut by_priority: Vec<(&str, Tally)> = ACTIVE_STATUSES
        .iter()
        .map(|status| (*status, Tally::default()))
        .collect();
    let mut open = 0_i64;
    let mut active_ages: Vec<i64> = Vec::new();
    for row in &rows {
        let Some(index) = ACTIVE_STATUSES.iter().position(|s| *s == row.status) else {
            continue;
        };
        let key = or_else(&row.priority, json!("unknown"));
        if matches!(key, Value::Array(_) | Value::Object(_)) {
            return Err("TypeError");
        }
        by_priority[index].1.add(&key);
        open += 1;
        if let Some(age) = row.age_s {
            active_ages.push(age);
        }
    }
    let depth: i64 = ACTIVE_STATUSES.iter().map(|s| counts.count_of(s)).sum();
    let backlog = json!({
        "by_priority": by_priority
            .iter()
            .map(|(status, tally)| ((*status).to_owned(), json!(tally.as_map())))
            .collect::<Obj>(),
        "open": open,
        "oldest_age_s": active_ages.iter().copied().max(),
        "ages_unknown": open - active_ages.len() as i64,
        "complete": complete,
    });

    Ok(json!({
        "state": "ok",
        "counts": counts.as_map(),
        "depth": depth,
        "items": rows.into_iter().map(|row| Value::Object(row.fields)).collect::<Vec<_>>(),
        "by_assignee": by_assignee,
        "backlog": backlog,
        "throughput": throughput,
    }))
}

/// One task row before output: the fields in oracle order, plus what sorting and the
/// dependents need.
struct TaskRow {
    fields: Obj,
    id: Value,
    status: String,
    priority: Value,
    assignees: Vec<Value>,
    age_s: Option<i64>,
}

fn priority_rank(priority: &Value) -> Result<i64, PyError> {
    match priority {
        Value::String(text) => Ok(match text.as_str() {
            "urgent" => 0,
            "high" => 1,
            "medium" => 2,
            "low" => 3,
            _ => 9,
        }),
        Value::Array(_) | Value::Object(_) => Err("TypeError"),
        _ => Ok(9),
    }
}

/// Python `sorted(load.items(), key=lambda row: (-row[1], row[0]))`.
fn by_assignee(load: &Tally) -> Result<Value, PyError> {
    let mut entries: Vec<(String, Value, i64)> = load.rows.clone();
    for (index, first) in entries.iter().enumerate() {
        for second in &entries[index + 1..] {
            if first.2 == second.2 && !names_comparable(&first.1, &second.1) {
                return Err("TypeError");
            }
        }
    }
    entries.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| compare_names(&a.1, &b.1)));
    Ok(Value::Array(
        entries
            .into_iter()
            .map(|(_, name, count)| json!({"assignee": name, "count": count}))
            .collect(),
    ))
}

fn names_comparable(a: &Value, b: &Value) -> bool {
    (a.is_string() && b.is_string()) || (number_of(a).is_some() && number_of(b).is_some())
}

fn compare_names(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => number_of(a)
            .partial_cmp(&number_of(b))
            .unwrap_or(std::cmp::Ordering::Equal),
    }
}

fn owner_question(block_reason: &Value) -> Option<String> {
    let text = py_strip(&py_str(&or_else(block_reason, json!("")))).to_owned();
    let rest = text.strip_prefix("needs-owner:")?;
    let question = py_strip(rest);
    (!question.is_empty()).then(|| question.to_owned())
}

fn throughput_error(name: &str) -> Value {
    json!({"completed_current_bucket": null, "completed_last_24h": null, "error": name})
}

/// Python `_throughput`: the daemon's hourly buckets, never re-derived from the task list.
fn throughput_of(metrics: &Value, now: DateTime<Utc>) -> Result<Value, PyError> {
    let metrics = obj_or_empty(metrics)?;
    let throughput = obj_or_empty(field(&metrics, "throughput"))?;
    let Value::Array(buckets) = or_else(field(&throughput, "buckets"), json!([])) else {
        return Ok(no_buckets());
    };
    if buckets.is_empty() {
        return Ok(no_buckets());
    }
    let now_s = seconds(now);
    let rows: Vec<&Obj> = buckets
        .iter()
        .filter_map(|row| match row {
            Value::Object(map) if !field(map, "bucket").is_null() => Some(map),
            _ => None,
        })
        .collect();
    let mut starts = Vec::with_capacity(rows.len());
    for row in &rows {
        starts.push(float_of(field(row, "bucket"))?);
    }
    let day: Vec<usize> = (0..rows.len())
        .filter(|&index| now_s - starts[index] <= DAY_S)
        .collect();
    // Python `max(rows, key=...)` returns the first largest bucket.
    let mut newest: Option<usize> = None;
    for (index, start) in starts.iter().enumerate() {
        match newest {
            None => newest = Some(index),
            Some(best) if *start > starts[best] => newest = Some(index),
            Some(_) => {}
        }
    }
    let current = newest.filter(|&index| now_s - starts[index] < HOUR_S);
    let completed_current_bucket = match current {
        Some(index) => completed_of(rows[index])?,
        None => 0,
    };
    let completed_last_24h: i64 = day
        .iter()
        .map(|&index| completed_of(rows[index]))
        .sum::<Result<i64, PyError>>()?;
    Ok(json!({
        "completed_current_bucket": completed_current_bucket,
        "bucket_started_at": newest.map(|index| float_value(starts[index])),
        "completed_last_24h": completed_last_24h,
        "completed_in_window": field(&throughput, "completed"),
        "note": "current (partial) hourly bucket; last_24h sums buckets started in 86400 s",
    }))
}

fn no_buckets() -> Value {
    json!({
        "completed_current_bucket": null,
        "completed_last_24h": null,
        "note": "no buckets in the metrics payload",
    })
}

fn completed_of(row: &Obj) -> Result<i64, PyError> {
    int_of(&or_else(field(row, "completed"), json!(0)))
}

fn read_asks(daemon: &dyn BoardCalls, now: DateTime<Utc>) -> Result<Value, PyError> {
    let listing = match daemon.request(
        "task_list",
        args(json!({"include_done": true, "limit": TASK_LIST_LIMIT})),
    ) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let Value::Array(tasks) = list_items(&listing, "tasks") else {
        return Ok(unavailable("invalid task list"));
    };
    let cutoff = seconds(now) - 24.0 * 3600.0;
    let mut asks: Vec<(Option<i64>, Value)> = Vec::new();
    for task in &tasks {
        let t = obj(task)?;
        let created_by = py_str(&or_else(field(t, "created_by"), json!("")));
        let created_at = field(t, "created_at");
        if !is_owner_mailbox(&created_by) {
            continue;
        }
        let Some(created) = number_of(created_at) else {
            continue;
        };
        if created < cutoff {
            continue;
        }
        let age_s = age_seconds(now, created_at);
        asks.push((
            age_s,
            json!({
                "id": field(t, "id"),
                "title": field(t, "title"),
                "status": py_str(&get_or(t, "status", json!("unknown"))),
                "age_s": age_s,
            }),
        ));
    }
    // Python sorts by age, with an unknown age last.
    asks.sort_by_key(|(age, _)| age.unwrap_or(1_000_000_000));
    let asks: Vec<Value> = asks.into_iter().map(|(_, row)| row).collect();
    let count = asks.len();
    Ok(json!({"asks": asks, "count": count, "window_hours": 24}))
}

fn is_owner_mailbox(created_by: &str) -> bool {
    created_by == "owner" || created_by.starts_with("owner@")
}

fn read_dunks(daemon: &dyn BoardCalls, now: DateTime<Utc>) -> Result<Value, PyError> {
    let listing = match daemon.request("list_dunks", Obj::new()) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let listing = obj_or_empty(&listing)?;
    let Value::Array(rows) = field(&listing, "dunks") else {
        return Ok(unavailable("invalid dunk list"));
    };
    let mut items = Vec::with_capacity(rows.len());
    let mut running = 0_i64;
    let mut failed = 0_i64;
    for dunk in rows {
        let d = obj(dunk)?;
        let is_running = truthy(field(d, "running"));
        let last_error = field(d, "last_error").clone();
        if is_running {
            running += 1;
        }
        if truthy(&last_error) {
            failed += 1;
        }
        let target = [
            "target_pane_id",
            "target_agent",
            "target_tab",
            "target_workspace",
        ]
        .iter()
        .map(|key| field(d, key))
        .find(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!("unknown"));
        items.push(json!({
            "id": field(d, "id"),
            "name": or_else(field(d, "name"), field(d, "id").clone()),
            "target": target,
            "target_label": field(d, "target_label"),
            "interval_minutes": field(d, "interval_minutes"),
            "running": is_running,
            "status": field(d, "status"),
            "next_send_in_s": field(d, "next_send_in_s"),
            "send_count": field(d, "send_count"),
            "skip_count": field(d, "skip_count"),
            "hold_count": field(d, "hold_count"),
            "only_when": field(d, "only_when"),
            "last_error": last_error,
            // The daemon clears last_error on a successful send, so the age of the last
            // successful send says whether the schedule still delivers.
            "last_sent_at": field(d, "last_sent_at"),
            "last_sent_age_s": age_seconds(now, field(d, "last_sent_at")),
            "last_skipped_age_s": age_seconds(now, field(d, "last_skipped_at")),
        }));
    }
    Ok(json!({
        "state": "ok",
        "total": items.len(),
        "running": running,
        "failed": failed,
        "items": items,
    }))
}

fn read_panes(daemon: &dyn BoardCalls) -> Result<Value, PyError> {
    let listing = match daemon.request("list_panes", Obj::new()) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let listing = obj_or_empty(&listing)?;
    let Value::Array(rows) = field(&listing, "panes") else {
        return Ok(unavailable("invalid pane list"));
    };
    let mut items = Vec::with_capacity(rows.len());
    for pane in rows {
        let p = obj(pane)?;
        items.push(json!({
            "pane_id": field(p, "pane_id"),
            "agent": or_else(field(p, "agent"), json!("")),
            "agent_status": field(p, "agent_status"),
            "agent_status_source": field(p, "agent_status_source"),
            "agent_status_reason": field(p, "agent_status_reason"),
            "agent_status_inferred": field(p, "agent_status_inferred"),
            "workspace_label": field(p, "workspace_label"),
            "tab_label": field(p, "tab_label"),
            "is_self": truthy(field(p, "is_self")),
        }));
    }
    let status_is =
        |row: &Value, wanted: &str| field_of(row, "agent_status").as_str() == Some(wanted);
    let working = items
        .iter()
        .filter(|row| status_is(*row, "working"))
        .count();
    let idle = items.iter().filter(|row| status_is(*row, "idle")).count();
    let gap = iter_values(field(&listing, "registry_gap"))?;
    let mut dead = Vec::new();
    for row in &gap {
        let r = obj(row)?;
        if field(r, "alive") == &Value::Bool(false) {
            let pane_id = or_else(field(r, "pane_id"), field(r, "pane").clone());
            let mut copy = r.clone();
            set_key_map(&mut copy, "pane_id", pane_id);
            dead.push(Value::Object(copy));
        }
    }
    // A lane is a pane with a declared turn or an agent. A bare shell tab is counted on its own.
    let is_lane = |row: &Value| {
        truthy(field_of(row, "agent"))
            || matches!(
                field_of(row, "agent_status").as_str(),
                Some("working" | "idle")
            )
    };
    let bare = items.iter().filter(|row| !is_lane(*row)).count();
    let unknown_turns = items
        .iter()
        .filter(|row| is_lane(*row) && !status_is(*row, "working") && !status_is(*row, "idle"))
        .count();
    let registry_alive = gap
        .iter()
        .filter(|row| field_of(*row, "alive") == &Value::Bool(true))
        .count();
    Ok(json!({
        "state": "ok",
        "total": items.len(),
        "working": working,
        "idle": idle,
        "bare_panes": bare,
        "dead": dead.len(),
        "dead_items": dead,
        "unknown": unknown_turns + registry_alive,
        "items": items,
    }))
}

fn read_health(daemon: &dyn BoardCalls) -> Result<Value, PyError> {
    let health = match daemon.request("health", Obj::new()) {
        Ok(health) => health,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let Value::Object(health) = &health else {
        return Ok(unavailable("invalid health report"));
    };
    let checks = obj_or_empty(field(health, "checks"))?;
    let memory = obj_or_empty(field(&checks, "memory"))?;
    let stale = obj_or_empty(field(&checks, "stale_leases"))?;
    Ok(json!({
        "state": "ok",
        "ok": truthy(field(health, "ok")),
        "problems": iter_values(field(health, "problems"))?,
        "repairs": iter_values(field(health, "repairs"))?,
        "duration_ms": field(health, "duration_ms"),
        "memory_ok": field(&memory, "ok"),
        "memory_sources": field(&memory, "sources"),
        "memory_chunks": field(&memory, "chunks"),
        "queue_depth": field(&memory, "queue_depth"),
        "stale_leases": field(&stale, "expired_leases"),
        "wal_bytes": field(&checks, "wal_bytes"),
        "disk_free_bytes": field(&checks, "disk_free_bytes"),
    }))
}

/// Python `_host_board`: the host sample comes from `ops_summary::summary`.
fn read_host(
    conn: &Connection,
    now: DateTime<Utc>,
    host_watch: &Path,
    opencode: &Path,
) -> Result<Value, PyError> {
    let summary = ops_summary::summary(conn, now, host_watch, opencode)
        .map_err(|error| summary_error_name(&error))?;
    let host = value_field(&summary, "host_health").clone();
    if !truthy(&host) {
        return Ok(unavailable("no host sample"));
    }
    let host = obj(&host)?;
    let state = if truthy(field(host, "stale")) {
        "stale"
    } else {
        "ok"
    };
    let mut out = Obj::new();
    out.insert("state".to_owned(), json!(state));
    for (key, value) in host {
        out.insert(key.clone(), value.clone());
    }
    Ok(Value::Object(out))
}

fn read_reports(daemon: &dyn BoardCalls, now: DateTime<Utc>) -> Result<Value, PyError> {
    let listing = match daemon.request("report_list", Obj::new()) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let Value::Object(listing) = &listing else {
        return Ok(unavailable("invalid report list"));
    };
    let Value::Array(rows) = field(listing, "reports") else {
        return Ok(unavailable("invalid report list"));
    };
    let mut items = Vec::new();
    for (index, row) in rows.iter().take(5).enumerate() {
        let r = obj(row)?;
        let at = field(r, "at");
        items.push(json!({
            "name": field(r, "name"),
            "size": field(r, "size"),
            "at": at,
            "age_s": age_seconds(now, at),
            "open": index == 0,
        }));
    }
    let count = match listing.get("count") {
        Some(count) => count.clone(),
        None => json!(rows.len()),
    };
    Ok(json!({"state": "ok", "count": count, "items": items}))
}

fn read_poolrad(daemon: &dyn BoardCalls) -> Result<Value, PyError> {
    let progress = match daemon.request("poolrad_progress", Obj::new()) {
        Ok(progress) => progress,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let Value::Object(progress) = &progress else {
        return Ok(unavailable("invalid poolrad progress"));
    };
    Ok(json!({
        "state": "ok",
        "credited": field(progress, "credited"),
        "identities": field(progress, "identities"),
        "ceiling": field(progress, "ceiling"),
        "ceiling_note": or_else(
            field(progress, "ceiling_note"),
            json!("CEILING: an upper bound, not a verified denominator"),
        ),
        "credit_pending": field(progress, "credit_pending"),
        "main": field(progress, "main"),
        "queue_exported_at": field(progress, "queue_exported_at"),
    }))
}

fn read_reviews(daemon: &dyn BoardCalls, now: DateTime<Utc>) -> Result<Value, PyError> {
    let listing = match daemon.request("review_list", Obj::new()) {
        Ok(listing) => listing,
        Err(error) => return Ok(unavailable(error.python_type)),
    };
    let Value::Array(rows) = list_items(&listing, "reviews") else {
        return Ok(unavailable("invalid review list"));
    };
    let mut ages: Vec<i64> = Vec::new();
    let mut unknown = 0_i64;
    for row in &rows {
        let r = obj(row)?;
        let age = match field(r, "age_s") {
            Value::Null => age_seconds(now, field(r, "created_at")),
            other => Some(int_of(other)?),
        };
        match age {
            None => unknown += 1,
            Some(age) => ages.push(age),
        }
    }
    Ok(json!({
        "state": "ok",
        "review": {
            "awaiting": rows.len(),
            "oldest_age_s": ages.iter().copied().max(),
            "ages_unknown": unknown,
        },
    }))
}

/// Python `compact`: three owner-facing sections, with each source's state and read time.
fn compact(readings: &Obj, generated_at: &str, interval_s: u64) -> Result<Obj, PyError> {
    let source = |name: &str| -> Obj {
        let mut out = Obj::new();
        out.insert("source".to_owned(), json!(name));
        out.insert("generated_at".to_owned(), json!(generated_at));
        let reading = readings
            .get(name)
            .cloned()
            .unwrap_or_else(|| unavailable("missing source"));
        if let Value::Object(map) = reading {
            for (key, value) in map {
                out.insert(key, value);
            }
        }
        out
    };
    let tasks = source("tasks");
    let panes = source("panes");
    let dunks = source("dunks");
    let health = source("health");
    let host = source("host");
    let reviews = readings
        .get("reviews")
        .cloned()
        .unwrap_or_else(|| unavailable("missing source"));
    let reviews_obj = match &reviews {
        Value::Object(map) => map.clone(),
        _ => Obj::new(),
    };
    let owner: Vec<Value> = obj_items(&tasks)
        .into_iter()
        .filter(|row| truthy(field_of(row, "needs_owner")))
        .collect();

    let mut alarms: Vec<Value> = Vec::new();
    for reading in [&tasks, &panes, &dunks, &health, &host] {
        let state = field(reading, "state");
        if state.as_str() != Some("ok") {
            let text = if truthy(field(reading, "error")) {
                field(reading, "error").clone()
            } else {
                let name = py_str(field(reading, "source"));
                let state_text = state.as_str().ok_or("TypeError")?;
                json!(format!("{name} is {state_text}"))
            };
            alarms.push(json!({
                "source": field(reading, "source"),
                "state": state,
                "generated_at": generated_at,
                "text": text,
            }));
        }
    }
    for problem in field(&health, "problems")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        alarms.push(json!({
            "source": "health",
            "state": "problem",
            "generated_at": generated_at,
            "text": problem,
        }));
    }
    for row in obj_items(&dunks) {
        if truthy(field_of(&row, "running")) && truthy(field_of(&row, "last_error")) {
            let last_error = field_of(&row, "last_error")
                .as_str()
                .ok_or("TypeError")?
                .to_owned();
            alarms.push(json!({
                "source": "dunks",
                "state": "problem",
                "generated_at": generated_at,
                "text": format!("{}: {last_error}", py_str(field_of(&row, "id"))),
            }));
        }
    }
    if let Value::Object(levels) = or_else(field(&host, "levels"), json!({})) {
        for (resource, level) in &levels {
            if let Some(level) = level
                .as_str()
                .filter(|l| *l == "warning" || *l == "critical")
            {
                alarms.push(json!({
                    "source": "host",
                    "state": level,
                    "generated_at": generated_at,
                    "text": format!("{resource}: {level}"),
                }));
            }
        }
    } else {
        return Err("AttributeError");
    }

    // t996: the header strip. Each tile carries its source and read time.
    let withheld = obj_or_empty(field(&tasks, "backlog"))?;
    let throughput = obj_or_empty(field(&tasks, "throughput"))?;
    let tasks_state = field(&tasks, "state").clone();
    let mut throughput_brief = Obj::new();
    throughput_brief.insert("source".to_owned(), json!("tasks"));
    throughput_brief.insert("generated_at".to_owned(), json!(generated_at));
    throughput_brief.insert("state".to_owned(), tasks_state.clone());
    for (key, value) in &throughput {
        if key != "error" {
            throughput_brief.insert(key.clone(), value.clone());
        }
    }
    throughput_brief.insert("error".to_owned(), field(&throughput, "error").clone());
    let review = match reviews_obj.get("review") {
        Some(review) => obj_or_empty(review)?,
        None => Obj::new(),
    };
    // Python `(reviews or {}).get("state", "unavailable")`: a present key wins, even when null.
    let review_state = reviews_obj
        .get("state")
        .cloned()
        .unwrap_or_else(|| json!("unavailable"));
    let brief = json!({
        "throughput": Value::Object(throughput_brief),
        "backlog": count(
            &tasks,
            &[
                ("open", field(&withheld, "open").clone()),
                ("by_priority", or_else(field(&withheld, "by_priority"), json!({}))),
                ("oldest_age_s", field(&withheld, "oldest_age_s").clone()),
                ("ages_unknown", field(&withheld, "ages_unknown").clone()),
                ("complete", field(&withheld, "complete").clone()),
            ],
        ),
        "review": {
            "source": "review_list",
            "generated_at": generated_at,
            "state": review_state,
            "awaiting": field(&review, "awaiting"),
            "oldest_age_s": field(&review, "oldest_age_s"),
            "ages_unknown": field(&review, "ages_unknown"),
        },
        "lanes": count(
            &panes,
            &[
                ("working", field(&panes, "working").clone()),
                ("idle", field(&panes, "idle").clone()),
                ("dead", field(&panes, "dead").clone()),
                ("unknown", field(&panes, "unknown").clone()),
                ("bare_panes", field(&panes, "bare_panes").clone()),
            ],
        ),
    });

    let asks = readings
        .get("asks")
        .cloned()
        .unwrap_or_else(|| unavailable("missing source"));
    let mut out = Obj::new();
    out.insert("generated_at".to_owned(), json!(generated_at));
    out.insert("interval_s".to_owned(), json!(interval_s));
    out.insert("no_model".to_owned(), json!(true));
    out.insert("brief".to_owned(), brief);
    out.insert(
        "attention".to_owned(),
        json!({
            "generated_at": generated_at,
            "alarms": alarms,
            "owner": {
                "source": "tasks",
                "state": tasks_state,
                "generated_at": generated_at,
                "items": owner,
            },
        }),
    );
    out.insert(
        "reference".to_owned(),
        json!({
            "tasks": Value::Object(tasks),
            "lanes": Value::Object(panes),
            "schedules": Value::Object(dunks),
            "asks": asks,
        }),
    );
    out.insert("reports".to_owned(), Value::Object(source("reports")));
    out.insert(
        "poolrad_progress".to_owned(),
        Value::Object(source("poolrad")),
    );
    Ok(out)
}

/// Python `count(reading, **values)`: the values are shown only when the source is `ok`.
fn count(reading: &Obj, values: &[(&str, Value)]) -> Value {
    let state = field(reading, "state").clone();
    let mut out = Obj::new();
    out.insert("source".to_owned(), field(reading, "source").clone());
    out.insert("state".to_owned(), state.clone());
    out.insert(
        "generated_at".to_owned(),
        field(reading, "generated_at").clone(),
    );
    for (key, value) in values {
        let shown = if state.as_str() == Some("ok") {
            value.clone()
        } else {
            Value::Null
        };
        out.insert((*key).to_owned(), shown);
    }
    Value::Object(out)
}

fn obj_items(reading: &Obj) -> Vec<Value> {
    match field(reading, "items") {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

// -- provider budgets ----------------------------------------------------------------------

/// Python `ops_budget.build`: subscriptions from the operations database, provider readings
/// from the cache, then `project`.
fn budget(
    conn: &Connection,
    usage: &UsageCache,
    now: DateTime<Utc>,
    max_age_s: i64,
) -> Result<Value, PyError> {
    let (subscriptions, subscription_error) = match subscriptions(conn) {
        Ok(rows) => (rows, None),
        Err(_) => (Vec::new(), Some("Subscription source unavailable")),
    };
    let (readings, error) = match usage.snapshot() {
        Ok(rows) => (rows, None),
        Err(message) => (
            Vec::new(),
            Some(message.chars().take(240).collect::<String>()),
        ),
    };
    let mut budget = project(&readings, &subscriptions, now, error.as_deref(), max_age_s)?;
    set_key(&mut budget, "subscription_error", json!(subscription_error));
    Ok(budget)
}

fn subscriptions(conn: &Connection) -> rusqlite::Result<Vec<Subscription>> {
    let mut statement = conn.prepare(
        "SELECT provider,monthly_usd_cents,observed_at FROM subscription_costs s \
         WHERE project=? AND id=(SELECT MAX(id) FROM subscription_costs \
         WHERE project=s.project AND provider=s.provider) ORDER BY provider",
    )?;
    let rows = statement.query_map([PROJECT], |row| {
        Ok(Subscription {
            provider: row.get(0)?,
            monthly_usd_cents: row.get(1)?,
            observed_at: row.get(2)?,
        })
    })?;
    rows.collect()
}

/// Python `ops_budget.project`: the provider windows, states and prices for the board.
pub fn project(
    readings: &[Value],
    subscriptions: &[Subscription],
    now: DateTime<Utc>,
    error: Option<&str>,
    max_age_s: i64,
) -> Result<Value, PyError> {
    let generated = iso_micro(now);
    let mut prices: HashMap<&str, &Subscription> = HashMap::new();
    for row in subscriptions {
        prices.insert(row.provider.as_str(), row);
    }
    let mut providers: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for reading in readings {
        let reading = obj(reading)?;
        let name = match field(reading, "provider") {
            Value::String(name) if !name.is_empty() => name.clone(),
            _ => continue,
        };
        seen.insert(name.clone());
        let last_good = field(reading, "last_good");
        let observation: &Obj = match last_good {
            Value::Object(good) => good,
            _ => reading,
        };
        let measured = moment_of(field(observation, "measured_at"));
        let age = measured.and_then(|moment| age_between(now, moment));
        let status = or_else(field(reading, "status"), json!("unknown"));
        let meaning = field(observation, "meaning").clone();
        let snapshot: Obj = match field(observation, "last_snapshot") {
            Value::Object(snapshot) => snapshot.clone(),
            _ => Obj::new(),
        };
        let mut raw_windows = field(observation, "windows").clone();
        if !truthy(&raw_windows) && !snapshot.is_empty() && meaning.as_str() == Some("percent_used")
        {
            raw_windows = json!([snapshot_window(&snapshot)]);
        }
        let local_estimate = meaning.as_str() == Some("local_session_estimate");
        let has_value = truthy(&raw_windows)
            || (local_estimate && number_of(field(observation, "session_cost_usd")).is_some());
        // Python `state in {"ok", "unknown"}` raises for an unhashable state.
        if matches!(status, Value::Array(_) | Value::Object(_)) {
            return Err("TypeError");
        }
        let mut state = status;
        if matches!(state.as_str(), Some("ok" | "unknown")) && has_value {
            state = json!(if age.is_none_or(|age| age > max_age_s) {
                "stale"
            } else {
                "ok"
            });
        }
        let reason = reason_text(reading, &name, &state);
        let failed = if state.as_str() == Some("error") {
            moment_of(field(reading, "measured_at")).map(iso_micro)
        } else {
            None
        };
        let mut windows: Vec<Value> = Vec::new();
        if meaning.as_str() == Some("percent_used") {
            if let Value::Array(items) = &raw_windows {
                for window in items {
                    let Value::Object(window) = window else {
                        continue;
                    };
                    let reset = moment_of(field(window, "resets_at"));
                    let mut window_state = if reset.is_some_and(|at| at <= now) {
                        json!("expired")
                    } else {
                        state.clone()
                    };
                    let used = number_of(field(window, "percent_used"));
                    let left = number_of(field(window, "percent_remaining"));
                    if used.is_none() && left.is_none() {
                        window_state = json!("unknown");
                    }
                    windows.push(json!({
                        "name": py_str(&or_else(field(window, "name"), json!("unnamed"))),
                        "state": window_state,
                        "percent_used": num_value(field(window, "percent_used")),
                        "percent_remaining": num_value(field(window, "percent_remaining")),
                        "resets_at": reset.map(iso_micro),
                    }));
                }
            }
        }
        let price_key = if name == "opencode-go" {
            "opencode"
        } else {
            name.as_str()
        };
        let price = prices
            .get(price_key)
            .map(|subscription| price_value(subscription));
        let extra = match field(observation, "extra_usage") {
            Value::Object(source) => Some(json!({
                "used_credits": num_value(field(source, "used_credits")),
                "monthly_limit": num_value(field(source, "monthly_limit")),
                "utilization": num_value(field(source, "utilization")),
                "spend_limit_reached": match field(source, "spend_limit_reached") {
                    Value::Bool(flag) => json!(flag),
                    _ => Value::Null,
                },
            })),
            _ => None,
        };
        let local = local_estimate.then(|| {
            json!({
                "session_cost_usd": num_value(field(observation, "session_cost_usd")),
                "last_context_tokens": num_value(field(observation, "last_context_tokens")),
                "session_messages_with_usage": num_value(field(observation, "session_messages_with_usage")),
            })
        });
        providers.push(json!({
            "provider": name,
            "source": SOURCE,
            "state": state,
            "meaning": meaning,
            "generated_at": generated,
            "measured_at": measured.map(iso_micro),
            "age_s": age,
            "reason": reason,
            "last_good": last_good.is_object(),
            "next_attempt_at": field(reading, "next_attempt_at"),
            "failed_at": failed,
            "windows": windows,
            "subscription": price,
            "extra_usage": extra,
            "local_estimate": local,
        }));
    }
    for subscription in subscriptions {
        let canonical = if subscription.provider == "opencode" {
            "opencode-go"
        } else {
            subscription.provider.as_str()
        };
        if seen.contains(canonical) {
            continue;
        }
        providers.push(json!({
            "provider": canonical,
            "source": SOURCE,
            "state": "unavailable",
            "meaning": "percent_used",
            "generated_at": generated,
            "measured_at": null,
            "age_s": null,
            "reason": error.filter(|text| !text.is_empty()).unwrap_or("no provider reading"),
            "windows": [],
            "extra_usage": null,
            "local_estimate": null,
            "subscription": price_value(subscription),
        }));
    }
    let state = if error.is_some_and(|text| !text.is_empty()) {
        "unavailable"
    } else {
        "ok"
    };
    Ok(json!({
        "source": SOURCE,
        "state": state,
        "generated_at": generated,
        "error": error,
        "providers": providers,
        "dollars_remaining": null,
        "dollars_reason": DOLLARS_REASON,
    }))
}

fn price_value(subscription: &Subscription) -> Value {
    json!({
        "monthly_usd": float_value(subscription.monthly_usd_cents as f64 / 100.0),
        "state": "ok",
        "source": "owner-reported subscription",
        "observed_at": subscription.observed_at,
    })
}

/// Python `_reason`: the 429 text, the reason verbatim (240 characters), or a fallback.
fn reason_text(reading: &Obj, name: &str, state: &Value) -> String {
    let reason = py_strip(&py_str(&or_else(
        field(reading, "reason"),
        or_else(field(reading, "error"), json!("")),
    )))
    .to_owned();
    if rate_limited(&reason) {
        return format!("{name} endpoint rate-limited (HTTP 429)");
    }
    if !reason.is_empty() {
        return reason.chars().take(240).collect();
    }
    if state.as_str() == Some("ok") {
        String::new()
    } else {
        format!("{name} usage {}; no further reason supplied", py_str(state))
    }
}

/// Python `re.search(r"HTTP\s*429\b|rate_limit_error", text, re.I)`.
fn rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("rate_limit_error") {
        return true;
    }
    let mut from = 0;
    while let Some(found) = lower[from..].find("http") {
        let after = from + found + 4;
        let rest = lower[after..].trim_start_matches(char::is_whitespace);
        if let Some(tail) = rest.strip_prefix("429") {
            if !tail
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return true;
            }
        }
        from = after;
    }
    false
}

/// Python `ops_budget._collect`'s `snapshot` window: the newest snapshot as a window row.
fn snapshot_window(snapshot: &Obj) -> Value {
    let minutes = field(snapshot, "window_minutes");
    let given = field(snapshot, "name");
    let name = if truthy(given) {
        given.clone()
    } else {
        json!(default_window_name(minutes))
    };
    let remaining = match number_of(field(snapshot, "percent_used")) {
        Some(used) if (0.0..=100.0).contains(&used) => {
            subtract_from_hundred(field(snapshot, "percent_used"))
        }
        _ => Value::Null,
    };
    let mut window = snapshot.clone();
    set_key_map(&mut window, "name", name);
    set_key_map(&mut window, "percent_remaining", remaining);
    Value::Object(window)
}

/// Python `100 - used`: an int stays an int, and a float stays a float.
fn subtract_from_hundred(raw: &Value) -> Value {
    match raw.as_i64() {
        Some(int) => json!(100 - int),
        None => float_value(100.0 - number_of(raw).unwrap_or(0.0)),
    }
}

fn default_window_name(minutes: &Value) -> String {
    if number_of(minutes) == Some(10080.0) {
        "weekly".to_owned()
    } else if truthy(minutes) {
        format!("{}min", py_str(minutes))
    } else {
        "last window".to_owned()
    }
}

/// Python `UsageCache`: a per-provider cadence, a failure backoff, and the last good reading.
pub struct UsageCache {
    inner: Arc<UsageInner>,
    dispatch: Dispatch,
}

struct UsageInner {
    refresh_s: i64,
    backoff_s: Vec<i64>,
    reader: Arc<dyn UsageReader>,
    clock: Clock,
    state: Mutex<UsageState>,
}

#[derive(Default)]
struct UsageState {
    rows: HashMap<String, Value>,
    good: HashMap<String, Value>,
    due: HashMap<String, f64>,
    failures: HashMap<String, usize>,
    futures: HashMap<String, JoinHandle<()>>,
}

impl UsageCache {
    pub fn new(
        refresh_s: i64,
        backoff_s: Vec<i64>,
        reader: Arc<dyn UsageReader>,
        clock: Clock,
        dispatch: Dispatch,
    ) -> Self {
        Self {
            inner: Arc::new(UsageInner {
                refresh_s,
                backoff_s,
                reader,
                clock,
                state: Mutex::new(UsageState::default()),
            }),
            dispatch,
        }
    }

    /// Python `snapshot`: submit each due provider, then return every row in oracle order.
    /// A provider with no reading yet is `not_read_yet`, never a zero.
    pub fn snapshot(&self) -> Result<Vec<Value>, String> {
        let due = {
            let mut state = lock(&self.inner.state);
            let now = (self.inner.clock)();
            let mut due = Vec::new();
            for provider in PROVIDERS {
                let idle = state
                    .futures
                    .get(provider)
                    .is_none_or(JoinHandle::is_finished);
                if idle && now >= state.due.get(provider).copied().unwrap_or(0.0) {
                    due.push(provider);
                }
            }
            if self.dispatch == Dispatch::Threads {
                for provider in &due {
                    let inner = Arc::clone(&self.inner);
                    let name = (*provider).to_owned();
                    let handle = thread::Builder::new()
                        .name(format!("vp-usage-{name}"))
                        .spawn(move || inner.collect(&name))
                        .map_err(|error| error.to_string())?;
                    state.futures.insert((*provider).to_owned(), handle);
                }
                Vec::new()
            } else {
                due
            }
        };
        for provider in due {
            self.inner.collect(provider);
        }
        let state = lock(&self.inner.state);
        Ok(PROVIDERS
            .iter()
            .map(|provider| match state.rows.get(*provider) {
                Some(Value::Object(row)) if !row.is_empty() => Value::Object(row.clone()),
                _ => json!({
                    "provider": provider,
                    "status": "not_read_yet",
                    "meaning": "unknown",
                    "measured_at": null,
                    "reason": "Waiting for the first provider reading",
                }),
            })
            .collect())
    }

    /// Python `stop`: wait for the in-flight provider reads to finish.
    pub fn stop(&self) {
        let handles: Vec<JoinHandle<()>> = lock(&self.inner.state)
            .futures
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl UsageInner {
    fn backoff_for(&self, failures: usize) -> i64 {
        if self.backoff_s.is_empty() {
            return self.refresh_s;
        }
        let index = failures.saturating_sub(1).min(self.backoff_s.len() - 1);
        self.backoff_s[index]
    }

    /// Python `_collect`: one provider read, then the cadence and backoff update.
    fn collect(&self, provider: &str) {
        let row = match self.reader.read(provider) {
            Ok(row) => row,
            Err(message) => json!({
                "provider": provider,
                "status": "error",
                "meaning": "unknown",
                "measured_at": (self.clock)(),
                "error": message.chars().take(240).collect::<String>(),
            }),
        };
        let Value::Object(mut row) = row else {
            return;
        };
        let mut state = lock(&self.state);
        let now = (self.clock)();
        let snapshot: Obj = match field(&row, "last_snapshot") {
            Value::Object(snapshot) => snapshot.clone(),
            _ => Obj::new(),
        };
        let has_snapshot = number_of(field(&snapshot, "percent_used")).is_some();
        let has_local = field(&row, "meaning").as_str() == Some("local_session_estimate")
            && number_of(field(&row, "session_cost_usd")).is_some();
        let status = field(&row, "status").as_str();
        let success =
            status == Some("ok") || (status == Some("unknown") && (has_snapshot || has_local));
        let due = if success {
            state
                .good
                .insert(provider.to_owned(), Value::Object(row.clone()));
            state.failures.insert(provider.to_owned(), 0);
            now + self.refresh_s as f64
        } else {
            let failures = state.failures.get(provider).copied().unwrap_or(0) + 1;
            state.failures.insert(provider.to_owned(), failures);
            let wait = self.backoff_for(failures).min(self.refresh_s);
            if let Some(good) = state.good.get(provider).cloned() {
                row.insert("last_good".to_owned(), good);
            }
            now + wait as f64
        };
        state.due.insert(provider.to_owned(), due);
        row.insert("next_attempt_at".to_owned(), float_value(due));
        state.rows.insert(provider.to_owned(), Value::Object(row));
    }
}

// -- shared helpers ------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn ordered<const N: usize>(pairs: [(&str, Value); N]) -> Obj {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn set_key(value: &mut Value, key: &str, new: Value) {
    if let Value::Object(map) = value {
        set_key_map(map, key, new);
    }
}

fn set_key_map(map: &mut Obj, key: &str, new: Value) {
    map.insert(key.to_owned(), new);
}

fn value_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value {
        Value::Object(map) => field(map, key),
        _ => &NULL,
    }
}

fn field_of<'a>(value: &'a Value, key: &str) -> &'a Value {
    value_field(value, key)
}

/// `dict.get(key)` where a missing key and an explicit null read the same.
fn field<'a>(map: &'a Obj, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

fn get_or(map: &Obj, key: &str, default: Value) -> Value {
    map.get(key).cloned().unwrap_or(default)
}

/// Python `dict(x)`'s attribute access: a non-mapping raises `AttributeError`.
fn obj(value: &Value) -> Result<&Obj, PyError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("AttributeError"),
    }
}

/// Python `(x or {})` followed by `.get`: a falsy value is an empty mapping.
fn obj_or_empty(value: &Value) -> Result<Obj, PyError> {
    if !truthy(value) {
        return Ok(Obj::new());
    }
    obj(value).cloned()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_none_or(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python `x or default`.
fn or_else(value: &Value, default: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        default
    }
}

/// Python `list(x or [])` and the loops over such a value. Strings iterate characters and
/// mappings iterate keys. Any other truthy value is not iterable.
fn iter_values(value: &Value) -> Result<Vec<Value>, PyError> {
    if !truthy(value) {
        return Ok(Vec::new());
    }
    match value {
        Value::Array(items) => Ok(items.clone()),
        Value::String(text) => Ok(text.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(map) => Ok(map.keys().map(|key| json!(key)).collect()),
        _ => Err("TypeError"),
    }
}

/// Python `"owner" in (x or [])`.
fn contains_str(container: &Value, needle: &str) -> Result<bool, PyError> {
    if !truthy(container) {
        return Ok(false);
    }
    match container {
        Value::Array(items) => Ok(items.iter().any(|item| item.as_str() == Some(needle))),
        Value::String(text) => Ok(text.contains(needle)),
        Value::Object(map) => Ok(map.contains_key(needle)),
        _ => Err("TypeError"),
    }
}

/// A JSON number (not a boolean) as a float, when it is finite.
fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|float| float.is_finite()),
        _ => None,
    }
}

/// The original number for output (Python keeps an int as an int and a float as a float).
fn num_value(value: &Value) -> Value {
    if number_of(value).is_some() {
        value.clone()
    } else {
        Value::Null
    }
}

/// Python `int(x)` for the shapes the board reads.
fn int_of(value: &Value) -> Result<i64, PyError> {
    match value {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Number(number) => match number.as_i64() {
            Some(int) => Ok(int),
            None => number
                .as_f64()
                .filter(|float| float.abs() < 9.0e18)
                .map(|float| float.trunc() as i64)
                .ok_or("OverflowError"),
        },
        Value::String(text) => text
            .trim()
            .replace('_', "")
            .parse::<i64>()
            .map_err(|_| "ValueError"),
        _ => Err("TypeError"),
    }
}

/// Python `float(x)` for the shapes the board reads.
fn float_of(value: &Value) -> Result<f64, PyError> {
    match value {
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64().ok_or("OverflowError"),
        Value::String(text) => text
            .trim()
            .replace('_', "")
            .parse::<f64>()
            .map_err(|_| "ValueError"),
        _ => Err("TypeError"),
    }
}

/// Python `str.strip()`: Unicode whitespace plus the information separators U+001C to U+001F.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// A Python float, spelled as Python spells it, as a JSON number.
fn float_value(value: f64) -> Value {
    serde_json::from_str(&py_float_repr(value)).unwrap_or(Value::Null)
}

/// Seconds since the Unix epoch, to microseconds, as Python's `time.time()`.
fn seconds(now: DateTime<Utc>) -> f64 {
    now.timestamp_micros() as f64 / 1_000_000.0
}

/// Python `_iso`: whole seconds with `Z` for UTC.
fn iso_seconds(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Python `datetime.isoformat()` with `+00:00` as `Z`: microseconds appear only when non-zero.
fn iso_micro(value: DateTime<Utc>) -> String {
    if value.timestamp_subsec_micros() == 0 {
        value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        value.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    }
}

/// Python `_time` and `_age_seconds`' parse: a number is a Unix time, text is ISO-8601.
fn moment_of(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(_) => fromtimestamp_utc(number_of(value)?).ok(),
        Value::String(text) => parse_moment(text),
        _ => None,
    }
}

fn parse_moment(text: &str) -> Option<DateTime<Utc>> {
    let normalized = text.replace('Z', "+00:00");
    if let Ok(moment) = parse_time(&normalized) {
        return Some(moment);
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, format) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(&normalized, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc())
}

/// Python `max(0, int((now - moment).total_seconds()))`.
fn age_between(now: DateTime<Utc>, moment: DateTime<Utc>) -> Option<i64> {
    let micros = (now - moment).num_microseconds()?;
    Some((micros / 1_000_000).max(0))
}

/// Python `_age_seconds`: an age in whole seconds, or unknown.
fn age_seconds(now: DateTime<Utc>, value: &Value) -> Option<i64> {
    if value.is_null() {
        return None;
    }
    age_between(now, moment_of(value)?)
}

/// Python `_seconds_until`: whole seconds until a future time, or unknown.
fn seconds_until(now: DateTime<Utc>, value: &Value) -> Option<i64> {
    if value.is_null() {
        return None;
    }
    let micros = (moment_of(value)? - now).num_microseconds()?;
    (micros > 0).then(|| micros / 1_000_000)
}

/// Python `json.dumps` spelling of a dict key: strings as-is, scalars as their JSON text.
fn json_key(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Null => "null".to_owned(),
        other => other.to_string(),
    }
}

/// An insertion-ordered counter, as a Python dict of counts.
#[derive(Default)]
struct Tally {
    rows: Vec<(String, Value, i64)>,
}

impl Tally {
    fn add(&mut self, value: &Value) {
        let key = json_key(value);
        match self.rows.iter_mut().find(|row| row.0 == key) {
            Some(row) => row.2 += 1,
            None => self.rows.push((key, value.clone(), 1)),
        }
    }

    fn count_of(&self, key: &str) -> i64 {
        self.rows
            .iter()
            .find(|row| row.0 == key)
            .map_or(0, |row| row.2)
    }

    fn as_map(&self) -> Obj {
        self.rows
            .iter()
            .map(|(key, _, count)| (key.clone(), json!(count)))
            .collect()
    }
}

fn sqlite_error_name(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            "IntegrityError"
        }
        _ => "OperationalError",
    }
}

fn summary_error_name(error: &SummaryError) -> &'static str {
    match error {
        SummaryError::Sql(error) => sqlite_error_name(error),
        SummaryError::Timestamp(_) => "ValueError",
        SummaryError::Json(_) => "JSONDecodeError",
        SummaryError::InvalidJsonEncoding => "UnicodeDecodeError",
        SummaryError::InvalidPayloadType => "TypeError",
    }
}
