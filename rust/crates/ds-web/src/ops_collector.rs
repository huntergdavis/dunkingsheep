//! Real operations-metrics collector: the Rust producer behind `bridges/ops_metrics.py`
//! `collect` and its helpers (Python lines 264-530). `record_accounting` (531-544) is the
//! one writer `collect` calls, so it is ported with it.
//!
//! One run, in Python order:
//! 1. Read the external sources before any write. Panes come from the daemon (`list_panes`),
//!    VP message counts and agent-message events from read-only SQLite files. GitHub PRs, the
//!    Dunking Sheep metrics window and the published accounting pair are read only when
//!    `force_external` is set or the newest `ok` run is at least `EXTERNAL_INTERVAL_S` old;
//!    otherwise the state is `cached`. A failure that Python's `except` tuple catches becomes
//!    `unavailable` with the exception class name in `errors_json`. Any other failure escapes
//!    and nothing is written.
//! 2. One `IMMEDIATE` transaction writes `collection_runs`, `pane_samples`, `vp_message_counts`,
//!    `agent_message_events`, `pr_latest`, `pr_state_events`, `dunk_metrics_snapshots` and
//!    `accounting_snapshots`. Any SQLite error or escaping fault rolls the whole run back.
//! 3. Only after the commit does the caller's `summarize` run (Python `return summary(...)`).
//!
//! Inputs are explicit: the caller's initialised connection, the clock, the two read-only
//! database paths, a daemon transport (`DaemonCalls`) and a GitHub source (`GitHubSource`).
//! Nothing here reads the environment or the home directory. The poll timer, startup, the
//! summary and history readers, and the other `record_*` writers are separate slices.
//!
//! Approximations of Python's dynamic typing, kept deliberately narrow: `datetime.fromisoformat`
//! is approximated by RFC 3339 (`T` or space separator, explicit offset required); BLOB values
//! are rendered with a simplified `bytes` repr; `int()` accepts ASCII digits only, and `float()`
//! rejects underscores. Integers are exact at any size (`PyInt`). Only the SQLite bind is limited
//! to 64 bits, as in Python.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use ds_core::{
    client::{ClientError, FlockClient, Transport},
    usage::py_float_repr,
    wire_json,
};
use rusqlite::{
    params, types::Value as SqlValue, Connection, ErrorCode, OpenFlags, OptionalExtension,
    TransactionBehavior,
};
use serde_json::{json, Map, Number, Value};

/// Project key written to every row (`PROJECT`).
pub const PROJECT: &str = "poolrad";
/// Cadence of the timer that calls `collect` (`POLL_INTERVAL_S`). The timer is not ported here.
pub const POLL_INTERVAL_S: u64 = 60;
/// Minimum age, in seconds, before an external source is read again (`EXTERNAL_INTERVAL_S`).
pub const EXTERNAL_INTERVAL_S: i64 = 300;
/// Repository whose published accounting files are read (`PUBLISHED_REPO`).
pub const PUBLISHED_REPO: &str = "huntergdavis/poolrad-decomp";
/// Dunking Sheep metrics window in seconds (`--window 3600`).
pub const METRICS_WINDOW_S: u64 = 3600;
/// Timeout for each `gh` command (Python `subprocess.run(timeout=25)`).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

const POOLRAD_WORKSPACE: &str = "poolrad-decomp";
const CHECKPOINT_PATH: &str = "docs/re/function_accounting_checkpoint.json";
const CENSUS_PATH: &str = "docs/re/function_ceiling_census.json";
const CENSUS_SCOPE: &str = "original-game-functions-overlay-and-resident";
const EVENT_SOURCE_PREFIX: &str = "dunkingsheep:metrics.db:events:";
const FORBIDDEN_METRIC_KEYS: [&str; 7] = [
    "body", "text", "subject", "filename", "email", "token", "secret",
];

/// A failure that Python's `except` tuple catches. The source is recorded `unavailable`, and
/// `python_type` is the exception class name stored in `errors_json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    pub python_type: &'static str,
    pub detail: String,
}

impl SourceError {
    fn new(python_type: &'static str, detail: impl Into<String>) -> Self {
        Self {
            python_type,
            detail: detail.into(),
        }
    }
}

/// A failure that Python lets escape `collect` (for example `TypeError`, or `KeyError` inside
/// the unguarded write block). Nothing is committed when one is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub python_type: &'static str,
    pub detail: String,
}

/// Why `collect` returned an error.
#[derive(Debug)]
pub enum CollectError<E> {
    /// Python would raise this out of `collect`; the transaction was not committed.
    Fault(Fault),
    /// SQLite failed inside the write transaction, which was rolled back.
    Sqlite(rusqlite::Error),
    /// The caller's summary failed after the run was committed.
    Summary(E),
}

impl<E: std::fmt::Display> std::fmt::Display for CollectError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::Fault(fault) => {
                write!(formatter, "{}: {}", fault.python_type, fault.detail)
            }
            CollectError::Sqlite(error) => write!(formatter, "sqlite: {error}"),
            CollectError::Summary(error) => write!(formatter, "summary: {error}"),
        }
    }
}

/// A stop inside `collect_run`: a fault or a SQLite failure.
#[derive(Debug)]
enum Stop {
    Fault(Fault),
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for Stop {
    fn from(error: rusqlite::Error) -> Self {
        Stop::Sqlite(error)
    }
}

/// Inside the write block, a value error escapes Python's `try` blocks, so it is a fault.
impl From<SourceError> for Stop {
    fn from(error: SourceError) -> Self {
        Stop::Fault(Fault {
            python_type: error.python_type,
            detail: error.detail,
        })
    }
}

impl<E> From<Stop> for CollectError<E> {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Fault(fault) => CollectError::Fault(fault),
            Stop::Sqlite(error) => CollectError::Sqlite(error),
        }
    }
}

fn fault(python_type: &'static str, detail: impl Into<String>) -> Stop {
    Stop::Fault(Fault {
        python_type,
        detail: detail.into(),
    })
}

/// A source read either fails in a way Python catches, or stops the run.
#[derive(Debug)]
enum ReadFailure {
    Caught(SourceError),
    Stop(Stop),
}

impl From<SourceError> for ReadFailure {
    fn from(error: SourceError) -> Self {
        ReadFailure::Caught(error)
    }
}

impl From<Stop> for ReadFailure {
    fn from(stop: Stop) -> Self {
        ReadFailure::Stop(stop)
    }
}

/// SQLite failures that Python catches as `sqlite3.Error` in the source reads.
fn caught_sqlite(error: rusqlite::Error) -> ReadFailure {
    ReadFailure::Caught(SourceError::new(
        sqlite_python_type(&error),
        error.to_string(),
    ))
}

fn sqlite_python_type(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
            ErrorCode::InternalMalfunction | ErrorCode::NotFound => "InternalError",
            ErrorCode::OutOfMemory => "MemoryError",
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => "DatabaseError",
            ErrorCode::TooBig => "DataError",
            ErrorCode::ConstraintViolation | ErrorCode::TypeMismatch => "IntegrityError",
            ErrorCode::ApiMisuse | ErrorCode::ParameterOutOfRange => "ProgrammingError",
            _ => "OperationalError",
        },
        _ => "OperationalError",
    }
}

/// Daemon read commands. Production is `FlockClient` over the daemon socket.
pub trait DaemonCalls {
    /// One read command with its arguments; the reply is the command's `result` value.
    fn call(&mut self, command: &str, args: Map<String, Value>) -> Result<Value, SourceError>;
}

impl<T: Transport> DaemonCalls for FlockClient<T> {
    fn call(&mut self, command: &str, args: Map<String, Value>) -> Result<Value, SourceError> {
        FlockClient::call(self, command, args).map_err(daemon_error)
    }
}

/// Maps a daemon-client failure to the exception class the Python CLI would surface: a refused
/// or unavailable command exits non-zero (`RuntimeError`), an unparseable reply is
/// `JSONDecodeError`, and a socket timeout is `TimeoutExpired`.
fn daemon_error(error: ClientError) -> SourceError {
    match error {
        ClientError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            SourceError::new("TimeoutExpired", io.to_string())
        }
        ClientError::Io(io) => SourceError::new("OSError", io.to_string()),
        ClientError::Protocol(text) => SourceError::new("JSONDecodeError", text),
        ClientError::Unavailable(text) => SourceError::new("RuntimeError", text),
        ClientError::Dunk(error) => SourceError::new("RuntimeError", error.to_string()),
    }
}

/// GitHub reads: the poolrad PR list and the published accounting files.
pub trait GitHubSource {
    /// `gh pr list --state all --limit 100 --json number,state,headRefOid,mergedAt` in the repo.
    fn pr_list(&mut self) -> Result<Value, SourceError>;
    /// `gh api repos/<PUBLISHED_REPO>/git/ref/heads/main`.
    fn published_ref(&mut self) -> Result<Value, SourceError>;
    /// `gh api repos/<PUBLISHED_REPO>/contents/<path>?ref=<commit>`.
    fn published_contents(&mut self, path: &str, commit: &str) -> Result<Value, SourceError>;
}

/// Production GitHub source: the configured `gh` executable, run with Python's arguments.
#[derive(Debug, Clone)]
pub struct GhCli {
    pub program: PathBuf,
    /// Working directory for `gh pr list` (the poolrad checkout).
    pub repo_dir: PathBuf,
    pub timeout: Duration,
}

impl GitHubSource for GhCli {
    fn pr_list(&mut self) -> Result<Value, SourceError> {
        let argv: Vec<String> = [
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "number,state,headRefOid,mergedAt",
        ]
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
        command_json(&self.program, &argv, Some(&self.repo_dir), self.timeout)
    }

    fn published_ref(&mut self) -> Result<Value, SourceError> {
        let argv = vec![
            "api".to_owned(),
            format!("repos/{PUBLISHED_REPO}/git/ref/heads/main"),
        ];
        command_json(&self.program, &argv, None, self.timeout)
    }

    fn published_contents(&mut self, path: &str, commit: &str) -> Result<Value, SourceError> {
        let argv = vec![
            "api".to_owned(),
            format!("repos/{PUBLISHED_REPO}/contents/{path}?ref={commit}"),
        ];
        command_json(&self.program, &argv, None, self.timeout)
    }
}

/// Python `command_json`: run argv, decode stdout as UTF-8, require exit 0, parse JSON. Python
/// decodes the text before it checks the return code, so that order is kept.
fn command_json(
    program: &Path,
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Value, SourceError> {
    let mut command = Command::new(program);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(program, &error))?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| SourceError::new("OSError", error.to_string()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SourceError::new(
                "TimeoutExpired",
                format!("{} timed out", program.display()),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = join_output(stdout);
    let _ = join_output(stderr);
    let text = String::from_utf8(stdout)
        .map_err(|error| SourceError::new("UnicodeDecodeError", error.to_string()))?;
    if !status.success() {
        return Err(SourceError::new(
            "RuntimeError",
            format!("{} exited {status}", program.display()),
        ));
    }
    wire_json::from_str(&text)
        .map_err(|error| SourceError::new("JSONDecodeError", error.to_string()))
}

fn spawn_error(program: &Path, error: &std::io::Error) -> SourceError {
    let python_type = match error.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    };
    SourceError::new(python_type, format!("{}: {error}", program.display()))
}

fn drain<R: std::io::Read + Send + 'static>(reader: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
    reader.map(|mut reader| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            bytes
        })
    })
}

fn join_output(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

/// Explicit source locations. Both are opened read-only; tests point them at temporary files.
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// VP message database; the `messages` table is counted by channel and role.
    pub message_db: PathBuf,
    /// Dunking Sheep metrics database; the `events` table supplies delivery metadata.
    pub dunk_events_db: PathBuf,
}

/// The collector: explicit config, a daemon transport and a GitHub source.
pub struct Collector<D, G> {
    config: CollectorConfig,
    daemon: D,
    github: G,
}

/// Per-source state recorded in `collection_runs`. `skipped` is the Python placeholder.
#[derive(Debug)]
struct States {
    panes: &'static str,
    github: &'static str,
    vp_messages: &'static str,
    agent_messages: &'static str,
    dunk_metrics: &'static str,
    accounting: &'static str,
}

impl Default for States {
    fn default() -> Self {
        Self {
            panes: "skipped",
            github: "skipped",
            vp_messages: "skipped",
            agent_messages: "skipped",
            dunk_metrics: "skipped",
            accounting: "skipped",
        }
    }
}

impl<D: DaemonCalls, G: GitHubSource> Collector<D, G> {
    pub fn new(config: CollectorConfig, daemon: D, github: G) -> Self {
        Self {
            config,
            daemon,
            github,
        }
    }

    /// Python `collect(conn, now=now, force_external=...)`. `summarize` stands in for the
    /// trailing `summary(conn, now=now)` and runs only after the run is committed.
    pub fn collect<T, E>(
        &mut self,
        conn: &mut Connection,
        now: DateTime<Utc>,
        force_external: bool,
        summarize: impl FnOnce(&Connection, DateTime<Utc>) -> Result<T, E>,
    ) -> Result<T, CollectError<E>> {
        self.collect_run(conn, now, force_external)?;
        summarize(&*conn, now).map_err(CollectError::Summary)
    }

    fn collect_run(
        &mut self,
        conn: &mut Connection,
        now: DateTime<Utc>,
        force_external: bool,
    ) -> Result<(), Stop> {
        let observed_at = iso(&now);
        let mut states = States::default();
        let mut errors: BTreeMap<&'static str, &'static str> = BTreeMap::new();

        let panes = settle(
            read_panes(&mut self.daemon),
            &mut states.panes,
            &mut errors,
            "panes",
        )?
        .unwrap_or_default();
        let counts = settle(
            read_vp_counts(&self.config.message_db),
            &mut states.vp_messages,
            &mut errors,
            "vp_messages",
        )?
        .unwrap_or_default();
        let events = settle(
            read_agent_events(&*conn, &self.config.dunk_events_db),
            &mut states.agent_messages,
            &mut errors,
            "agent_messages",
        )?
        .unwrap_or_default();

        let prs = if force_external || source_due(&*conn, "github_state", now)? {
            settle(
                read_prs(&mut self.github),
                &mut states.github,
                &mut errors,
                "github",
            )?
            .unwrap_or_default()
        } else {
            states.github = "cached";
            Vec::new()
        };
        let metrics = if force_external || source_due(&*conn, "dunk_metrics_state", now)? {
            settle(
                read_metrics(&mut self.daemon),
                &mut states.dunk_metrics,
                &mut errors,
                "dunk_metrics",
            )?
        } else {
            states.dunk_metrics = "cached";
            None
        };
        let accounting = if force_external || source_due(&*conn, "accounting_state", now)? {
            settle(
                read_accounting(&mut self.github),
                &mut states.accounting,
                &mut errors,
                "accounting",
            )?
        } else {
            states.accounting = "cached";
            None
        };

        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run_id = insert_run(&transaction, &observed_at, &states, &errors)?;
        write_panes(&transaction, run_id, &panes)?;
        write_vp_counts(&transaction, run_id, &counts)?;
        write_agent_events(&transaction, &events)?;
        write_prs(&transaction, &prs, &observed_at)?;
        if let Some(metrics) = &metrics {
            write_metrics(&transaction, &observed_at, metrics)?;
        }
        if let Some(accounting) = &accounting {
            write_accounting(&transaction, &observed_at, accounting)?;
        }
        transaction.commit()?;
        Ok(())
    }
}

/// Records a source result: `ok` on success, `unavailable` for a caught failure (the class name
/// goes into `errors`), and a stop for anything Python would not catch.
fn settle<T>(
    result: Result<T, ReadFailure>,
    state: &mut &'static str,
    errors: &mut BTreeMap<&'static str, &'static str>,
    source: &'static str,
) -> Result<Option<T>, Stop> {
    match result {
        Ok(value) => {
            *state = "ok";
            Ok(Some(value))
        }
        Err(ReadFailure::Caught(error)) => {
            *state = "unavailable";
            errors.insert(source, error.python_type);
            Ok(None)
        }
        Err(ReadFailure::Stop(stop)) => Err(stop),
    }
}

/// Python `_source_due`: due when no `ok` run exists or the newest one is at least 300 s old.
fn source_due(conn: &Connection, column: &str, now: DateTime<Utc>) -> Result<bool, Stop> {
    let sql = format!(
        "SELECT observed_at FROM collection_runs WHERE {column}='ok' ORDER BY id DESC LIMIT 1"
    );
    let Some(observed) = conn
        .query_row(&sql, [], |row| row.get::<_, String>(0))
        .optional()?
    else {
        return Ok(true);
    };
    let then = parse_time(&observed)?;
    let elapsed_us = (now - then).num_microseconds().unwrap_or(i64::MAX);
    Ok(elapsed_us as f64 / 1_000_000.0 >= EXTERNAL_INTERVAL_S as f64)
}

/// Python `command_json(["dunkingsheep", "--json", "panes"])` and the `result["panes"]` read.
fn read_panes<D: DaemonCalls>(daemon: &mut D) -> Result<Vec<Value>, ReadFailure> {
    let reply = daemon.call("list_panes", Map::new())?;
    let Value::Object(reply) = reply else {
        return Err(ReadFailure::Stop(fault(
            "TypeError",
            "list_panes reply is not subscriptable by 'panes'",
        )));
    };
    let panes = reply
        .get("panes")
        .ok_or_else(|| SourceError::new("KeyError", "'panes'"))?;
    match panes {
        Value::Array(items) => Ok(items.clone()),
        _ => Err(SourceError::new("ValueError", "panes must be a list").into()),
    }
}

/// Python `_vp_counts`: the `messages` table grouped by channel and role.
fn read_vp_counts(path: &Path) -> Result<Vec<(String, String, i64)>, ReadFailure> {
    if !path.exists() {
        return Err(SourceError::new("FileNotFoundError", "VP message database is absent").into());
    }
    let source = open_read_only(path).map_err(caught_sqlite)?;
    let mut statement = source
        .prepare("SELECT channel,role,COUNT(*) FROM messages GROUP BY channel,role")
        .map_err(caught_sqlite)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, SqlValue>(0)?,
                row.get::<_, SqlValue>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(caught_sqlite)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(caught_sqlite)?;
    Ok(rows
        .iter()
        .map(|(channel, role, total)| (py_str_sql(channel), py_str_sql(role), *total))
        .collect())
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// One row of the Dunking Sheep `events` table, as read.
#[derive(Debug)]
struct RawEvent {
    id: SqlValue,
    at: SqlValue,
    kind: SqlValue,
    pane_id: SqlValue,
    source: SqlValue,
    sender: SqlValue,
    mailbox: SqlValue,
    bytes: SqlValue,
    duration_ms: SqlValue,
    attempts: SqlValue,
}

/// One delivery-metadata row for `agent_message_events` (no message text, no account details).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentEvent {
    pub source_event_id: PyInt,
    pub occurred_at: String,
    pub pane_id: String,
    pub kind: String,
    pub delivery_source: String,
    pub sender_alias: Option<String>,
    pub mailbox_alias: Option<String>,
    pub message_bytes: SqlValue,
    pub duration_ms: SqlValue,
    pub attempts: SqlValue,
    pub source_ref: String,
}

/// Python `_new_agent_message_events`: events after the newest recorded id, metadata only.
fn read_agent_events(conn: &Connection, path: &Path) -> Result<Vec<AgentEvent>, ReadFailure> {
    if !path.exists() {
        return Err(SourceError::new(
            "FileNotFoundError",
            "Dunking Sheep event database is absent",
        )
        .into());
    }
    let last_id: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(source_event_id),0) FROM agent_message_events",
            [],
            |row| row.get(0),
        )
        .map_err(caught_sqlite)?;
    let source = open_read_only(path).map_err(caught_sqlite)?;
    let mut statement = source
        .prepare(
            "SELECT id,at,kind,pane_id,source,sender,mailbox,bytes,duration_ms,attempts \
             FROM events WHERE id>? AND kind IN ('inbox.sent','message.sent') \
             AND pane_id IS NOT NULL ORDER BY id",
        )
        .map_err(caught_sqlite)?;
    let raw_rows = statement
        .query_map([last_id], |row| {
            Ok(RawEvent {
                id: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                pane_id: row.get(3)?,
                source: row.get(4)?,
                sender: row.get(5)?,
                mailbox: row.get(6)?,
                bytes: row.get(7)?,
                duration_ms: row.get(8)?,
                attempts: row.get(9)?,
            })
        })
        .map_err(caught_sqlite)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(caught_sqlite)?;
    raw_rows.into_iter().map(convert_event).collect()
}

fn convert_event(raw: RawEvent) -> Result<AgentEvent, ReadFailure> {
    let source_event_id = sql_int(&raw.id)?;
    let at = sql_float(&raw.at)?;
    let occurred_at = iso(&fromtimestamp_utc(at)?);
    let delivery_source = if sql_truthy(&raw.source) {
        py_str_sql(&raw.source)
    } else {
        "unknown".to_owned()
    };
    Ok(AgentEvent {
        source_event_id,
        occurred_at,
        pane_id: py_str_sql(&raw.pane_id),
        kind: py_str_sql(&raw.kind),
        delivery_source,
        sender_alias: safe_alias(&raw.sender),
        mailbox_alias: safe_alias(&raw.mailbox),
        message_bytes: raw.bytes,
        duration_ms: raw.duration_ms,
        attempts: raw.attempts,
        source_ref: format!("{EVENT_SOURCE_PREFIX}{}", py_str_sql(&raw.id)),
    })
}

/// Python `int(value)` for an event id. Malformed text, NaN and infinity are caught source errors,
/// since `ValueError` and `OverflowError` are in the `except` tuple. A well-formed integer above
/// the 64-bit range is kept: Python only fails when SQLite binds it, inside the write transaction
/// (see `PyInt::bind`). A `None` raises `TypeError`, which escapes the `try`, so it is a fault.
///
/// The real Dunking Sheep table declares `id INTEGER PRIMARY KEY AUTOINCREMENT` (dunk_metrics.py),
/// a rowid alias that only holds integers, so the oversized case cannot come from that table.
/// The code keeps Python's behaviour for any table it is pointed at.
fn sql_int(value: &SqlValue) -> Result<PyInt, ReadFailure> {
    match value {
        SqlValue::Integer(number) => Ok(PyInt::from_integer(*number)),
        SqlValue::Real(number) => PyInt::from_float(*number).map_err(ReadFailure::Caught),
        SqlValue::Text(text) => parse_py_int(text).map_err(ReadFailure::Caught),
        // int() takes an ASCII bytes literal. Anything else fails inside parse_py_int as ValueError.
        SqlValue::Blob(bytes) => parse_py_int(std::str::from_utf8(bytes).unwrap_or_default())
            .map_err(ReadFailure::Caught),
        SqlValue::Null => Err(ReadFailure::Stop(fault(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number",
        ))),
    }
}

/// Python `float(value)` for an event time column; `None` and BLOB are `TypeError` faults.
fn sql_float(value: &SqlValue) -> Result<f64, ReadFailure> {
    match value {
        SqlValue::Integer(number) => Ok(*number as f64),
        SqlValue::Real(number) => Ok(*number),
        SqlValue::Text(text) => parse_py_float(text).map_err(ReadFailure::Caught),
        SqlValue::Null | SqlValue::Blob(_) => Err(ReadFailure::Stop(fault(
            "TypeError",
            "float() argument must be a string or a real number",
        ))),
    }
}

/// Python `alias if isinstance(str) and alias and "@" not in alias`, truncated to 120 chars.
fn safe_alias(value: &SqlValue) -> Option<String> {
    match value {
        SqlValue::Text(text) if !text.is_empty() && !text.contains('@') => {
            Some(text.chars().take(120).collect())
        }
        _ => None,
    }
}

/// Python `_fetch` of `gh pr list` and the published accounting pair, with the metrics window.
fn read_prs<G: GitHubSource>(github: &mut G) -> Result<Vec<Value>, ReadFailure> {
    match github.pr_list()? {
        Value::Array(items) => Ok(items),
        _ => Err(SourceError::new("ValueError", "PR list must be a list").into()),
    }
}

fn read_metrics<D: DaemonCalls>(daemon: &mut D) -> Result<Value, ReadFailure> {
    let mut args = Map::new();
    args.insert("window_s".to_owned(), json!(METRICS_WINDOW_S));
    args.insert("since".to_owned(), Value::Null);
    args.insert("until".to_owned(), Value::Null);
    let metrics = daemon.call("metrics", args)?;
    valid_dunk_metrics(&metrics)?;
    Ok(metrics)
}

fn read_accounting<G: GitHubSource>(github: &mut G) -> Result<Accounting, ReadFailure> {
    Ok(published_accounting(github)?)
}

/// One PM-reviewed accounting snapshot: the `published_accounting` return value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accounting {
    pub commit_oid: String,
    pub strict_full_c: PyInt,
    pub verified_game_identities: PyInt,
    pub ceiling: PyInt,
    pub bound_kind: String,
    pub source_signature: String,
    pub evidence_ref: String,
}

/// Python `published_accounting`: the reviewed checkpoint and census at one `main` commit.
pub fn published_accounting<G: GitHubSource>(github: &mut G) -> Result<Accounting, SourceError> {
    let reference = github.published_ref()?;
    let commit = reference
        .as_object()
        .and_then(|outer| outer.get("object"))
        .and_then(Value::as_object)
        .and_then(|target| target.get("sha"))
        .and_then(Value::as_str)
        .filter(|sha| is_commit_oid(sha))
        .ok_or_else(|| SourceError::new("ValueError", "published main commit is invalid"))?
        .to_owned();
    let checkpoint = published_json(github, CHECKPOINT_PATH, &commit)?;
    let census = published_json(github, CENSUS_PATH, &commit)?;
    validate_published(commit, &checkpoint, &census)
}

/// Python `_published_json`: a base64 `contents` payload that decodes to a JSON object.
fn published_json<G: GitHubSource>(
    github: &mut G,
    path: &str,
    commit: &str,
) -> Result<Map<String, Value>, SourceError> {
    let invalid = || SourceError::new("ValueError", "published accounting file is invalid");
    let payload = github.published_contents(path, commit)?;
    let Some(payload) = payload.as_object() else {
        return Err(SourceError::new(
            "ValueError",
            "published accounting file encoding is unsupported",
        ));
    };
    if payload.get("encoding").and_then(Value::as_str) != Some("base64") {
        return Err(SourceError::new(
            "ValueError",
            "published accounting file encoding is unsupported",
        ));
    }
    let bytes = decode_base64_content(payload.get("content")).ok_or_else(invalid)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
    match wire_json::from_str(text).map_err(|_| invalid())? {
        Value::Object(map) => Ok(map),
        _ => Err(SourceError::new(
            "ValueError",
            "published accounting file must be an object",
        )),
    }
}

/// Python `base64.b64decode(content, validate=False)`: non-alphabet characters are discarded
/// (GitHub wraps the payload in newlines), and non-ASCII text or non-string content fails.
fn decode_base64_content(content: Option<&Value>) -> Option<Vec<u8>> {
    let Value::String(text) = content? else {
        return None;
    };
    if !text.is_ascii() {
        return None;
    }
    let alphabet: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(alphabet)
        .ok()
}

/// The validation block of Python `published_accounting`, in its order of checks.
fn validate_published(
    commit: String,
    checkpoint: &Map<String, Value>,
    census: &Map<String, Value>,
) -> Result<Accounting, SourceError> {
    let invalid = || {
        SourceError::new(
            "ValueError",
            "published accounting and census are not jointly reviewed",
        )
    };
    let (Some(strict), Some(identities), Some(ceiling)) = (
        json_int(checkpoint.get("strict_full_c_originals")),
        json_int(checkpoint.get("verified_game_identities")),
        json_int(census.get("count")),
    ) else {
        return Err(invalid());
    };
    let inputs = match checkpoint.get("inputs") {
        Some(Value::Object(map)) if !map.is_empty() => map,
        _ => return Err(invalid()),
    };
    let bound_kind = census.get("bound_kind").and_then(Value::as_str);
    let evidence = census.get("evidence_doc").and_then(Value::as_str);
    let digests_match = census
        .get("source_digests")
        .is_some_and(|digests| py_eq(digests, &Value::Object(inputs.clone())));
    let well_formed = py_eq_int(checkpoint.get("schema_version"), 1)
        && !strict.is_negative()
        && !identities.is_negative()
        && ceiling.is_positive()
        && strict.cmp_exact(&identities).is_le()
        && identities.cmp_exact(&ceiling).is_le()
        && census.get("status").and_then(Value::as_str) == Some("reviewed")
        && census.get("reviewed_by").and_then(Value::as_str) == Some("PM")
        && matches!(bound_kind, Some("exact" | "upper_bound"))
        && census.get("scope").and_then(Value::as_str) == Some(CENSUS_SCOPE)
        && digests_match
        && evidence.is_some_and(|doc| doc.starts_with("docs/re/"));
    if !well_formed {
        return Err(invalid());
    }
    let signature = sha256_hex(py_dumps(&Value::Object(inputs.clone()), ", ", ": ").as_bytes());
    Ok(Accounting {
        commit_oid: commit,
        strict_full_c: strict,
        verified_game_identities: identities,
        ceiling,
        bound_kind: bound_kind.unwrap_or_default().to_owned(),
        source_signature: signature,
        evidence_ref: evidence.unwrap_or_default().to_owned(),
    })
}

/// Python `_valid_dunk_metrics`: schema 1, a timestamped interval, and no content-bearing keys.
pub fn valid_dunk_metrics(value: &Value) -> Result<(), SourceError> {
    let schema = || {
        SourceError::new(
            "ValueError",
            "Dunking Sheep metrics schema_version 1 required",
        )
    };
    let Value::Object(map) = value else {
        return Err(schema());
    };
    if !py_eq_int(map.get("schema_version"), 1) {
        return Err(schema());
    }
    let Some(Value::Object(interval)) = map.get("interval") else {
        return Err(SourceError::new(
            "ValueError",
            "Dunking Sheep metrics interval required",
        ));
    };
    dunk_timestamp(interval.get("start"), "interval.start")?;
    dunk_timestamp(interval.get("end"), "interval.end")?;
    dunk_timestamp(map.get("generated_at"), "generated_at")?;
    if has_content_key(value) {
        return Err(SourceError::new(
            "ValueError",
            "Dunking Sheep metrics contains content-bearing keys",
        ));
    }
    Ok(())
}

fn has_content_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.keys()
                .any(|key| FORBIDDEN_METRIC_KEYS.contains(&key.to_lowercase().as_str()))
                || map.values().any(has_content_key)
        }
        Value::Array(items) => items.iter().any(has_content_key),
        _ => false,
    }
}

/// Python `_dunk_timestamp`: an ISO string, or a finite number of epoch seconds.
pub fn dunk_timestamp(item: Option<&Value>, label: &str) -> Result<String, SourceError> {
    match item {
        Some(Value::String(text)) => Ok(iso(&parse_time(text)?)),
        Some(Value::Number(number)) if number.as_f64().is_some_and(|f| f.is_finite()) => {
            match integer_token(number) {
                Some(text) => {
                    let seconds: i64 = text.parse().map_err(|_| {
                        SourceError::new("ValueError", format!("Dunking Sheep {label} invalid"))
                    })?;
                    DateTime::from_timestamp(seconds, 0)
                        .map(|moment| iso(&moment))
                        .ok_or_else(|| {
                            SourceError::new("ValueError", format!("Dunking Sheep {label} invalid"))
                        })
                }
                None => Ok(iso(&fromtimestamp_utc(
                    number.as_f64().unwrap_or(f64::NAN),
                )?)),
            }
        }
        _ => Err(SourceError::new(
            "ValueError",
            format!("Dunking Sheep {label} required"),
        )),
    }
}

/// Python `datetime.fromisoformat(value.replace("Z", "+00:00"))` with the tz-aware requirement.
pub fn parse_time(value: &str) -> Result<DateTime<Utc>, SourceError> {
    let mut normalized = value.replace('Z', "+00:00");
    if normalized.as_bytes().get(10) == Some(&b' ') {
        normalized.replace_range(10..11, "T");
    }
    DateTime::parse_from_rfc3339(&normalized)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| {
            SourceError::new(
                "ValueError",
                format!("invalid isoformat string {value:?}: {error}"),
            )
        })
}

/// Python `iso`: UTC, whole seconds (truncated), with a `Z` suffix.
pub fn iso(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Python `datetime.fromtimestamp(value, timezone.utc)`: microseconds round half to even, and
/// the carry is applied before the value is truncated to seconds by [`iso`].
pub fn fromtimestamp_utc(value: f64) -> Result<DateTime<Utc>, SourceError> {
    if value.is_nan() {
        return Err(SourceError::new(
            "ValueError",
            "Invalid value NaN (not a number)",
        ));
    }
    if value.is_infinite() {
        return Err(SourceError::new(
            "OverflowError",
            "cannot convert float infinity to integer",
        ));
    }
    let whole = value.trunc();
    let mut seconds = whole as i64;
    let mut micros = ((value - whole) * 1_000_000.0).round_ties_even() as i64;
    if micros >= 1_000_000 {
        seconds += 1;
        micros -= 1_000_000;
    } else if micros < 0 {
        seconds -= 1;
        micros += 1_000_000;
    }
    DateTime::from_timestamp(seconds, (micros * 1_000) as u32)
        .ok_or_else(|| SourceError::new("ValueError", format!("timestamp {value} out of range")))
}

/// Python `INSERT INTO collection_runs`: one row per run, with each source state and the
/// `errors_json` map of caught exception class names.
fn insert_run(
    tx: &Connection,
    observed_at: &str,
    states: &States,
    errors: &BTreeMap<&'static str, &'static str>,
) -> Result<i64, Stop> {
    let error_map: Map<String, Value> = errors
        .iter()
        .map(|(source, kind)| ((*source).to_owned(), Value::String((*kind).to_owned())))
        .collect();
    let errors_json = py_dumps(&Value::Object(error_map), ", ", ": ");
    tx.execute(
        "INSERT INTO collection_runs(observed_at,panes_state,github_state,vp_messages_state,\
         dunk_metrics_state,accounting_state,agent_messages_state,errors_json) \
         VALUES(?,?,?,?,?,?,?,?)",
        params![
            observed_at,
            states.panes,
            states.github,
            states.vp_messages,
            states.dunk_metrics,
            states.accounting,
            states.agent_messages,
            errors_json
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Python `lane_assignments` lookup for the project, as a pane-id to role map.
fn lane_assignments(tx: &Connection) -> Result<BTreeMap<String, String>, Stop> {
    let mut statement = tx.prepare("SELECT pane_id,role FROM lane_assignments WHERE project=?")?;
    let rows = statement.query_map([PROJECT], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(Stop::Sqlite)
}

/// Python `for pane in panes` (poolrad workspace only), in input order.
fn write_panes(tx: &Connection, run_id: i64, panes: &[Value]) -> Result<(), Stop> {
    let assignments = lane_assignments(tx)?;
    for pane in panes {
        let Value::Object(pane) = pane else {
            return Err(fault("AttributeError", "pane entry has no attribute 'get'"));
        };
        if pane.get("workspace_label") != Some(&Value::String(POOLRAD_WORKSPACE.to_owned())) {
            continue;
        }
        let pane_id = pane.get("pane_id").map_or_else(String::new, py_str);
        if pane_id.is_empty() {
            continue;
        }
        let role = assignments
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| "other".to_owned());
        let tab = or_empty(pane.get("tab_label"));
        let agent_name = non_empty(py_strip(&tab));
        let cwd = or_empty(pane.get("cwd"));
        let lane_name = non_empty(path_name(&cwd));
        let agent_kind = if truthy(pane.get("agent_type")) {
            json_to_sql(pane.get("agent_type"))?
        } else {
            json_to_sql(pane.get("agent"))?
        };
        let agent_status = json_to_sql(pane.get("agent_status"))?;
        tx.execute(
            "INSERT INTO pane_samples(run_id,pane_id,project,agent_kind,agent_status,agent_name,\
             lane_name,role) VALUES(?,?,?,?,?,?,?,?)",
            params![
                run_id,
                pane_id,
                PROJECT,
                agent_kind,
                agent_status,
                agent_name,
                lane_name,
                role
            ],
        )?;
    }
    Ok(())
}

fn write_vp_counts(
    tx: &Connection,
    run_id: i64,
    counts: &[(String, String, i64)],
) -> Result<(), Stop> {
    for (channel, role, total) in counts {
        tx.execute(
            "INSERT INTO vp_message_counts(run_id,channel,role,total) VALUES(?,?,?,?)",
            params![run_id, channel, role, total],
        )?;
    }
    Ok(())
}

/// `INSERT OR IGNORE`: an event id that already exists is skipped, so reruns are idempotent.
fn write_agent_events(tx: &Connection, events: &[AgentEvent]) -> Result<(), Stop> {
    for event in events {
        // Python binds source_event_id first, so an oversized id fails here, inside the transaction.
        let source_event_id = event.source_event_id.bind()?;
        tx.execute(
            "INSERT OR IGNORE INTO agent_message_events(source_event_id,occurred_at,pane_id,kind,\
             delivery_source,sender_alias,mailbox_alias,message_bytes,duration_ms,attempts,\
             source_ref) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![
                source_event_id,
                event.occurred_at,
                event.pane_id,
                event.kind,
                event.delivery_source,
                event.sender_alias,
                event.mailbox_alias,
                event.message_bytes,
                event.duration_ms,
                event.attempts,
                event.source_ref
            ],
        )?;
    }
    Ok(())
}

/// Python's PR loop: the latest row is replaced (first_seen kept), and each (number, state, head)
/// is recorded once in `pr_state_events`.
fn write_prs(tx: &Connection, prs: &[Value], observed_at: &str) -> Result<(), Stop> {
    for pr in prs {
        let Value::Object(pr) = pr else {
            return Err(fault("TypeError", "PR entry is not subscriptable by key"));
        };
        let number = py_int(
            pr.get("number")
                .ok_or_else(|| fault("KeyError", "'number'"))?,
        )?;
        let state = py_str(
            pr.get("state")
                .ok_or_else(|| fault("KeyError", "'state'"))?,
        );
        // Python binds the statement's parameters in column order, so the number's bind comes first.
        let number = number.bind()?;
        let head = if truthy(pr.get("headRefOid")) {
            json_to_sql(pr.get("headRefOid"))?
        } else {
            SqlValue::Text(String::new())
        };
        let merged_at = json_to_sql(pr.get("mergedAt"))?;
        tx.execute(
            "INSERT INTO pr_latest(project,number,state,head_oid,merged_at,first_seen_at,\
             last_seen_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(project,number) DO UPDATE SET \
             state=excluded.state,head_oid=excluded.head_oid,merged_at=excluded.merged_at,\
             last_seen_at=excluded.last_seen_at",
            params![
                PROJECT,
                number,
                state,
                head,
                merged_at,
                observed_at,
                observed_at
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO pr_state_events(project,number,state,head_oid,observed_at,\
             merged_at) VALUES(?,?,?,?,?,?)",
            params![PROJECT, number, state, head, observed_at, merged_at],
        )?;
    }
    Ok(())
}

fn write_metrics(tx: &Connection, observed_at: &str, metrics: &Value) -> Result<(), Stop> {
    let Value::Object(map) = metrics else {
        return Err(fault("ValueError", "metrics must be an object"));
    };
    let interval = map
        .get("interval")
        .and_then(Value::as_object)
        .ok_or_else(|| fault("ValueError", "interval required"))?;
    let generated = dunk_timestamp(map.get("generated_at"), "generated_at")?;
    let start = dunk_timestamp(interval.get("start"), "interval.start")?;
    let end = dunk_timestamp(interval.get("end"), "interval.end")?;
    let payload = py_dumps(metrics, ",", ":");
    tx.execute(
        "INSERT OR IGNORE INTO dunk_metrics_snapshots(observed_at,generated_at,interval_start,\
         interval_end,schema_version,payload_json) VALUES(?,?,?,?,?,?)",
        params![observed_at, generated, start, end, 1_i64, payload],
    )?;
    Ok(())
}

/// Python `record_accounting`: one row per (project, observed_at, commit), duplicates ignored.
fn write_accounting(
    tx: &Connection,
    observed_at: &str,
    accounting: &Accounting,
) -> Result<(), Stop> {
    let when = iso(&parse_time(observed_at)?);
    // Python binds the integer columns in order, so a count above the 64-bit range fails here.
    let strict_full_c = accounting.strict_full_c.bind()?;
    let verified_game_identities = accounting.verified_game_identities.bind()?;
    let ceiling = accounting.ceiling.bind()?;
    tx.execute(
        "INSERT OR IGNORE INTO accounting_snapshots(project,observed_at,commit_oid,\
         strict_full_c,verified_game_identities,ceiling,bound_kind,source_signature,evidence_ref) \
         VALUES(?,?,?,?,?,?,?,?,?)",
        params![
            PROJECT,
            when,
            accounting.commit_oid,
            strict_full_c,
            verified_game_identities,
            ceiling,
            accounting.bound_kind,
            accounting.source_signature,
            accounting.evidence_ref
        ],
    )?;
    Ok(())
}

/// Python `int(value)` for a JSON value in the write block. Every failure escapes the unguarded
/// block, so it is a fault. An integer above the 64-bit range is kept and fails at its bind.
fn py_int(value: &Value) -> Result<PyInt, Stop> {
    match value {
        Value::Bool(flag) => Ok(PyInt::from_integer(i64::from(*flag))),
        Value::Number(number) => match integer_token(number) {
            Some(text) => Ok(PyInt::canonical(&text)),
            None => PyInt::from_float(number.as_f64().unwrap_or(f64::NAN)).map_err(Stop::from),
        },
        Value::String(text) => parse_py_int(text).map_err(Stop::from),
        _ => Err(fault(
            "TypeError",
            "int() argument must be a string or a real number",
        )),
    }
}

/// A Python `int`, kept as canonical decimal text: an optional `-`, then digits with no leading
/// zeros, and `0` is never negative. Python ints have no size limit. SQLite binding is the only
/// step limited to 64 bits, so that check lives in `bind`, as it does in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyInt(String);

impl PyInt {
    fn from_integer(value: i64) -> Self {
        PyInt(value.to_string())
    }

    /// Canonical form of an optional sign followed by ASCII digits.
    fn canonical(text: &str) -> Self {
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let digits = digits.trim_start_matches('0');
        if digits.is_empty() {
            PyInt("0".to_owned())
        } else if negative {
            PyInt(format!("-{digits}"))
        } else {
            PyInt(digits.to_owned())
        }
    }

    /// Python `int(value)` for a finite float: truncation toward zero, with exact digits.
    fn from_float(value: f64) -> Result<Self, SourceError> {
        if value.is_nan() {
            return Err(SourceError::new(
                "ValueError",
                "cannot convert float NaN to integer",
            ));
        }
        if value.is_infinite() {
            return Err(SourceError::new(
                "OverflowError",
                "cannot convert float infinity to integer",
            ));
        }
        Ok(PyInt::canonical(&format!("{:.0}", value.trunc())))
    }

    /// The decimal text, as Python's `str()` prints the integer.
    pub fn as_decimal(&self) -> &str {
        &self.0
    }

    fn is_negative(&self) -> bool {
        self.0.starts_with('-')
    }

    fn is_positive(&self) -> bool {
        !self.is_negative() && self.0 != "0"
    }

    /// Exact ordering, as Python compares ints.
    fn cmp_exact(&self, other: &PyInt) -> Ordering {
        match (self.is_negative(), other.is_negative()) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_magnitude(&self.0, &other.0),
            (true, true) => cmp_magnitude(&other.0[1..], &self.0[1..]),
        }
    }

    /// Binding to a SQLite INTEGER. Above the 64-bit range Python raises `OverflowError` here,
    /// inside the write transaction, so the whole run rolls back.
    fn bind(&self) -> Result<i64, Stop> {
        self.0.parse::<i64>().map_err(|_| {
            fault(
                "OverflowError",
                "Python int too large to convert to SQLite INTEGER",
            )
        })
    }
}

fn cmp_magnitude(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Python `int(text)` for base 10: surrounding whitespace, one optional sign, and underscores
/// only between digits. The value is exact at any size.
fn parse_py_int(text: &str) -> Result<PyInt, SourceError> {
    let invalid = || {
        SourceError::new(
            "ValueError",
            format!("invalid literal for int() with base 10: {text:?}"),
        )
    };
    let trimmed = py_strip(text);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => ("-", &trimmed[1..]),
        Some(b'+') => ("", &trimmed[1..]),
        _ => ("", trimmed),
    };
    let well_formed = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'_');
    if !well_formed {
        return Err(invalid());
    }
    let compact: String = digits.chars().filter(|c| *c != '_').collect();
    Ok(PyInt::canonical(&format!("{sign}{compact}")))
}

/// Python `float(text)`: whitespace-trimmed; underscores are rejected (approximation).
fn parse_py_float(text: &str) -> Result<f64, SourceError> {
    let trimmed = py_strip(text);
    if trimmed.contains('_') {
        return Err(SourceError::new(
            "ValueError",
            format!("could not convert string to float: {text:?}"),
        ));
    }
    trimmed.parse::<f64>().map_err(|_| {
        SourceError::new(
            "ValueError",
            format!("could not convert string to float: {text:?}"),
        )
    })
}

/// Python `json_to_sql` binding for a JSON value: `None` and missing keys bind NULL, booleans
/// bind as 0/1, integers as INTEGER, floats as REAL, and containers fail to bind.
fn json_to_sql(value: Option<&Value>) -> Result<SqlValue, Stop> {
    Ok(match value {
        None | Some(Value::Null) => SqlValue::Null,
        Some(Value::Bool(flag)) => SqlValue::Integer(i64::from(*flag)),
        Some(Value::Number(number)) => match integer_token(number) {
            Some(text) => SqlValue::Integer(PyInt::canonical(&text).bind()?),
            None => SqlValue::Real(number.as_f64().unwrap_or(f64::NAN)),
        },
        Some(Value::String(text)) => SqlValue::Text(text.clone()),
        Some(Value::Array(_) | Value::Object(_)) => {
            return Err(fault(
                "ProgrammingError",
                "Error binding parameter: unsupported type",
            ))
        }
    })
}

/// Python truthiness of a SQLite value: `NULL`, `0`, `0.0`, `""` and an empty BLOB are false.
fn sql_truthy(value: &SqlValue) -> bool {
    match value {
        SqlValue::Null => false,
        SqlValue::Integer(number) => *number != 0,
        SqlValue::Real(number) => *number != 0.0,
        SqlValue::Text(text) => !text.is_empty(),
        SqlValue::Blob(bytes) => !bytes.is_empty(),
    }
}

fn py_str_sql(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "None".to_owned(),
        SqlValue::Integer(number) => number.to_string(),
        SqlValue::Real(number) => py_float_repr(*number),
        SqlValue::Text(text) => text.clone(),
        SqlValue::Blob(bytes) => py_bytes_repr(bytes),
    }
}

fn py_bytes_repr(bytes: &[u8]) -> String {
    let mut out = String::from("b'");
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push('\'');
    out
}

/// Python truthiness of an optional JSON value (`None`, `0`, `""`, `[]`, `{}`, `False` are false).
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => !matches!(number.as_f64(), Some(f) if f == 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// Python `str(value or "")`.
fn or_empty(value: Option<&Value>) -> String {
    if truthy(value) {
        value.map_or_else(String::new, py_str)
    } else {
        String::new()
    }
}

fn non_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// Python `str.strip()`: ASCII and Unicode whitespace, plus the U+001C..U+001F separators.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Python `Path(text).name`: the last component, ignoring empty and `.` parts.
fn path_name(text: &str) -> &str {
    text.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .next_back()
        .unwrap_or("")
}

/// Python `str(value)` for a JSON value (strings unchanged, everything else as `repr`).
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(flag) => (if *flag { "True" } else { "False" }).to_owned(),
        Value::Number(number) => match integer_token(number) {
            Some(text) => text,
            None => py_float_repr(number.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(text) => py_str_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!("{}: {}", py_str_repr(key), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The text of a JSON number when it is an integer literal (Python `int`), with `-0` as `0`.
fn integer_token(number: &Number) -> Option<String> {
    let token = number.to_string();
    let digits = token.strip_prefix('-').unwrap_or(&token);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if token == "-0" {
        Some("0".to_owned())
    } else {
        Some(token)
    }
}

/// Python `type(value) is int`: a JSON integer literal. Booleans and floats are not ints here.
fn json_int(value: Option<&Value>) -> Option<PyInt> {
    match value {
        Some(Value::Number(number)) => integer_token(number).map(|text| PyInt::canonical(&text)),
        _ => None,
    }
}

/// Python `value == n` for `n` an int: `True == 1` and `1.0 == 1`.
fn py_eq_int(value: Option<&Value>, expected: i64) -> bool {
    value
        .and_then(num_of)
        .is_some_and(|number| num_eq(&number, &Num::Int(PyInt::from_integer(expected))))
}

/// Python `==` for JSON-derived values. `True == 1`, `1 == 1.0`, and NaN is never equal.
fn py_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| py_eq(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| py_eq(value, other)))
        }
        _ => match (num_of(left), num_of(right)) {
            (Some(a), Some(b)) => num_eq(&a, &b),
            _ => false,
        },
    }
}

/// A JSON bool or number as Python sees it: an exact int, or a float.
enum Num {
    Int(PyInt),
    Float(f64),
}

fn num_of(value: &Value) -> Option<Num> {
    match value {
        Value::Bool(flag) => Some(Num::Int(PyInt::from_integer(i64::from(*flag)))),
        Value::Number(number) => match integer_token(number) {
            Some(text) => Some(Num::Int(PyInt::canonical(&text))),
            None => number.as_f64().map(Num::Float),
        },
        _ => None,
    }
}

/// Python `==` between two numbers. Int against int is exact, and so is int against float:
/// `10**30 != 1e30`, because the float is exactly 1000000000000000019884624838656.
fn num_eq(left: &Num, right: &Num) -> bool {
    match (left, right) {
        (Num::Int(a), Num::Int(b)) => a == b,
        (Num::Float(a), Num::Float(b)) => a == b,
        (Num::Int(int), Num::Float(float)) | (Num::Float(float), Num::Int(int)) => {
            float.is_finite()
                && float.fract() == 0.0
                && PyInt::from_float(*float).is_ok_and(|exact| exact == *int)
        }
    }
}

fn is_commit_oid(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Python `json.dumps(value, sort_keys=True, separators=(item_sep, key_sep))` with the default
/// `ensure_ascii=True` string escaping and Python's float and integer spelling.
pub fn py_dumps(value: &Value, item_sep: &str, key_sep: &str) -> String {
    let mut out = String::new();
    dump_into(&mut out, value, item_sep, key_sep);
    out
}

fn dump_into(out: &mut String, value: &Value, item_sep: &str, key_sep: &str) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&json_number(number)),
        Value::String(text) => push_json_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                dump_into(out, item, item_sep, key_sep);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                push_json_string(out, key);
                out.push_str(key_sep);
                dump_into(out, item, item_sep, key_sep);
            }
            out.push('}');
        }
    }
}

fn json_number(number: &Number) -> String {
    if let Some(text) = integer_token(number) {
        return text;
    }
    let value = number.as_f64().unwrap_or(f64::NAN);
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "Infinity".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else {
        py_float_repr(value)
    }
}

fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(character),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}
