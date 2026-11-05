//! Integration tests for `src/ops_collector.rs`, the Rust port of `bridges/ops_metrics.py`
//! `collect`. The module is compiled into this test crate with `#[path]`, so `lib.rs` needs no
//! change. Every database is a temporary file, the daemon is a local Unix socket or an in-memory
//! fake, and `gh` is an owned shell script. Nothing touches a live service or the network, and
//! Python is never run.

#[allow(dead_code)]
#[path = "../src/ops_collector.rs"]
mod ops_collector;

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use base64::Engine as _;
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use ds_core::client::{FlockClient, UnixTransport};
use ops_collector::{
    dunk_timestamp, fromtimestamp_utc, iso, parse_time, published_accounting, py_dumps,
    valid_dunk_metrics, CollectError, Collector, CollectorConfig, DaemonCalls, GhCli, GitHubSource,
    SourceError, EXTERNAL_INTERVAL_S, POLL_INTERVAL_S, PROJECT, PUBLISHED_REPO,
};
use rusqlite::{params, types::FromSql, Connection};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

const OPS_SCHEMA: &str = include_str!("../src/ops_schema.sql");
const PYTHON_SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../bridges/ops_metrics.py"
);
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const CHECKPOINT_PATH: &str = "docs/re/function_accounting_checkpoint.json";
const CENSUS_PATH: &str = "docs/re/function_ceiling_census.json";
/// `sha256` of `{"a": "x", "b": 2}`, the Python `json.dumps(sort_keys=True)` spelling of the
/// reviewed inputs. Computed with `sha256sum`, not with this crate.
const INPUTS_SIGNATURE: &str = "16688fc9adafaed795638c7e391262e36a34263ad1e2be62fdd980c7157dec61";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 5, 19, 28, 18).unwrap()
}

fn valid_metrics() -> Value {
    json!({
        "schema_version": 1,
        "generated_at": "2026-11-05T19:00:00Z",
        "interval": {"start": "2026-11-05T18:00:00Z", "end": "2026-11-05T19:00:00Z"},
        "panes": 3
    })
}

fn checkpoint_json() -> Value {
    json!({
        "schema_version": 1,
        "strict_full_c_originals": 10,
        "verified_game_identities": 12,
        "inputs": {"b": 2, "a": "x"}
    })
}

fn census_json() -> Value {
    json!({
        "count": 20,
        "status": "reviewed",
        "reviewed_by": "PM",
        "bound_kind": "upper_bound",
        "scope": "original-game-functions-overlay-and-resident",
        "source_digests": {"a": "x", "b": 2},
        "evidence_doc": "docs/re/evidence.md"
    })
}

/// A `gh api contents` reply: base64 wrapped at 60 columns with newlines, as GitHub sends it.
fn published_file(document: &Value) -> Value {
    let encoded = base64::engine::general_purpose::STANDARD.encode(document.to_string());
    let wrapped: String = encoded
        .as_bytes()
        .chunks(60)
        .map(|chunk| format!("{}\n", std::str::from_utf8(chunk).unwrap()))
        .collect();
    json!({"encoding": "base64", "content": wrapped})
}

fn ops_db(dir: &Path) -> Connection {
    let conn = Connection::open(dir.join("operations.sqlite3")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    conn.execute_batch(OPS_SCHEMA).unwrap();
    conn
}

/// The VP message database: a `messages` table with one row per `(channel, role)` pair.
fn message_db(dir: &Path, rows: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("messages.sqlite3");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, channel TEXT, role TEXT, body TEXT);",
    )
    .unwrap();
    for (channel, role) in rows {
        conn.execute(
            "INSERT INTO messages(channel,role,body) VALUES(?,?,?)",
            params![channel, role, "body"],
        )
        .unwrap();
    }
    path
}

/// The Dunking Sheep metrics database: an untyped `events` table, filled with literal SQL so the
/// tests can place NULLs, text and integers exactly where they matter.
fn events_db(dir: &Path, statements: &[String]) -> PathBuf {
    let path = dir.join("metrics.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE events (id, at, kind, pane_id, source, sender, mailbox, bytes, duration_ms, attempts);",
    )
    .unwrap();
    for statement in statements {
        conn.execute_batch(statement).unwrap();
    }
    path
}

fn epoch(hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(2026, 11, 5, hour, minute, 0)
        .unwrap()
        .timestamp()
}

fn config(messages: &Path, events: &Path) -> CollectorConfig {
    CollectorConfig {
        message_db: messages.to_path_buf(),
        dunk_events_db: events.to_path_buf(),
    }
}

fn scalar<T: FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn count(conn: &Connection, table: &str) -> i64 {
    scalar(conn, &format!("SELECT COUNT(*) FROM {table}"))
}

/// Every table a run can write, for the "nothing was committed" assertions.
fn assert_no_rows_written(conn: &Connection) {
    for table in [
        "collection_runs",
        "pane_samples",
        "vp_message_counts",
        "agent_message_events",
        "pr_latest",
        "pr_state_events",
        "dunk_metrics_snapshots",
        "accounting_snapshots",
    ] {
        assert_eq!(
            count(conn, table),
            0,
            "{table} must be empty after a rollback"
        );
    }
}

fn run_count(conn: &Connection, _now: DateTime<Utc>) -> Result<i64, String> {
    conn.query_row("SELECT COUNT(*) FROM collection_runs", [], |row| row.get(0))
        .map_err(|error| error.to_string())
}

/// Shared record of which fake calls ran, so tests can count reads after `collect`.
#[derive(Clone, Default)]
struct Calls(Rc<RefCell<Vec<String>>>);

impl Calls {
    fn record(&self, name: &str) {
        self.0.borrow_mut().push(name.to_owned());
    }

    fn count(&self, name: &str) -> usize {
        self.0
            .borrow()
            .iter()
            .filter(|call| call.as_str() == name)
            .count()
    }
}

struct FakeDaemon {
    panes: Result<Value, SourceError>,
    metrics: Result<Value, SourceError>,
    calls: Calls,
}

impl FakeDaemon {
    fn healthy(calls: &Calls) -> Self {
        Self {
            panes: Ok(json!({"panes": []})),
            metrics: Ok(valid_metrics()),
            calls: calls.clone(),
        }
    }
}

impl DaemonCalls for FakeDaemon {
    fn call(&mut self, command: &str, args: Map<String, Value>) -> Result<Value, SourceError> {
        self.calls.record(command);
        match command {
            "list_panes" => self.panes.clone(),
            "metrics" => {
                assert_eq!(args.get("window_s"), Some(&json!(3600)));
                assert_eq!(args.get("since"), Some(&Value::Null));
                assert_eq!(args.get("until"), Some(&Value::Null));
                self.metrics.clone()
            }
            other => panic!("unexpected daemon verb {other}"),
        }
    }
}

struct FakeGitHub {
    prs: Result<Value, SourceError>,
    reference: Result<Value, SourceError>,
    contents: HashMap<String, Result<Value, SourceError>>,
    calls: Calls,
}

impl FakeGitHub {
    fn healthy(calls: &Calls) -> Self {
        Self::with_documents(calls, &checkpoint_json(), &census_json())
    }

    fn with_documents(calls: &Calls, checkpoint: &Value, census: &Value) -> Self {
        let mut contents = HashMap::new();
        contents.insert(CHECKPOINT_PATH.to_owned(), Ok(published_file(checkpoint)));
        contents.insert(CENSUS_PATH.to_owned(), Ok(published_file(census)));
        Self {
            prs: Ok(json!([])),
            reference: Ok(json!({"object": {"sha": SHA}})),
            contents,
            calls: calls.clone(),
        }
    }
}

impl GitHubSource for FakeGitHub {
    fn pr_list(&mut self) -> Result<Value, SourceError> {
        self.calls.record("pr_list");
        self.prs.clone()
    }

    fn published_ref(&mut self) -> Result<Value, SourceError> {
        self.calls.record("published_ref");
        self.reference.clone()
    }

    fn published_contents(&mut self, path: &str, commit: &str) -> Result<Value, SourceError> {
        self.calls.record("published_contents");
        assert_eq!(
            commit, SHA,
            "accounting reads must pin the published commit"
        );
        self.contents.get(path).cloned().unwrap_or_else(|| {
            Err(SourceError {
                python_type: "KeyError",
                detail: path.to_owned(),
            })
        })
    }
}

fn collector_with(
    messages: &Path,
    events: &Path,
    daemon: FakeDaemon,
    github: FakeGitHub,
) -> Collector<FakeDaemon, FakeGitHub> {
    Collector::new(config(messages, events), daemon, github)
}

fn read_row<T: FromSql>(conn: &Connection, sql: &str) -> T {
    scalar(conn, sql)
}

#[test]
fn success_run_writes_every_table_with_python_values() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    conn.execute(
        "INSERT INTO lane_assignments(project,pane_id,role,focus,updated_at) VALUES('poolrad','%1','coder','f','2026-11-05T00:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO lane_assignments(project,pane_id,role,focus,updated_at) VALUES('poolrad','%2','pm','f','2026-11-05T00:00:00Z')",
        [],
    )
    .unwrap();
    let messages = message_db(
        dir.path(),
        &[
            ("inbox", "coder"),
            ("inbox", "coder"),
            ("inbox", "reviewer"),
            ("dm", "pm"),
        ],
    );
    let long_sender = "x".repeat(150);
    let t0 = epoch(18, 0);
    let events = events_db(
        dir.path(),
        &[
            format!("INSERT INTO events VALUES (1, {t0}, 'inbox.sent', '%7', 'cli', 'pm@host', 'coder', 120, 5, 1)"),
            format!("INSERT INTO events VALUES (2, {t0}, 'message.sent', '%8', NULL, '{long_sender}', NULL, NULL, NULL, NULL)"),
            format!("INSERT INTO events VALUES (3, {t0}, 'pane.focus', '%9', 'cli', 'a', 'b', 1, 1, 1)"),
            format!("INSERT INTO events VALUES (4, {t0}, 'inbox.sent', NULL, 'cli', 'a', 'b', 1, 1, 1)"),
        ],
    );
    let calls = Calls::default();
    let daemon = FakeDaemon {
        panes: Ok(json!({"panes": [
            {"workspace_label": "poolrad-decomp", "pane_id": "%1", "tab_label": "  Lead  ",
             "cwd": "/home/u/lanes/coder-a/", "agent_type": "codex", "agent_status": "working"},
            {"workspace_label": "poolrad-decomp", "pane_id": "%2", "agent": "claude", "cwd": ""},
            {"workspace_label": "elsewhere", "pane_id": "%3"},
            {"workspace_label": "poolrad-decomp"}
        ]})),
        ..FakeDaemon::healthy(&calls)
    };
    let github = FakeGitHub {
        prs: Ok(json!([
            {"number": 7, "state": "OPEN", "headRefOid": "aaa111", "mergedAt": null},
            {"number": 8, "state": "MERGED", "headRefOid": "bbb222", "mergedAt": "2026-11-05T18:00:00Z"}
        ])),
        ..FakeGitHub::healthy(&calls)
    };
    let mut collector = collector_with(&messages, &events, daemon, github);

    let runs = collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();

    assert_eq!(runs, 1, "the summary callback sees the committed run");
    let observed = "2026-11-05T19:28:18Z";
    let run = scalar::<i64>(&conn, "SELECT id FROM collection_runs");
    assert_eq!(
        read_row::<String>(&conn, "SELECT observed_at FROM collection_runs"),
        observed
    );
    for column in [
        "panes_state",
        "github_state",
        "vp_messages_state",
        "dunk_metrics_state",
        "accounting_state",
        "agent_messages_state",
    ] {
        let state: String = read_row(&conn, &format!("SELECT {column} FROM collection_runs"));
        assert_eq!(state, "ok", "{column}");
    }
    assert_eq!(
        read_row::<String>(&conn, "SELECT errors_json FROM collection_runs"),
        "{}"
    );

    let panes: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    )> = {
        let mut statement = conn
            .prepare("SELECT pane_id,agent_kind,agent_status,agent_name,lane_name,role FROM pane_samples WHERE run_id=? ORDER BY pane_id")
            .unwrap();
        statement
            .query_map([run], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        panes,
        vec![
            (
                "%1".to_owned(),
                Some("codex".to_owned()),
                Some("working".to_owned()),
                Some("Lead".to_owned()),
                Some("coder-a".to_owned()),
                "coder".to_owned()
            ),
            (
                "%2".to_owned(),
                Some("claude".to_owned()),
                None,
                None,
                None,
                "pm".to_owned()
            ),
        ],
        "only poolrad-decomp panes with an id are sampled; roles come from lane_assignments"
    );

    let counts: Vec<(String, String, i64)> = {
        let mut statement = conn
            .prepare("SELECT channel,role,total FROM vp_message_counts WHERE run_id=? ORDER BY channel,role")
            .unwrap();
        statement
            .query_map([run], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        counts,
        vec![
            ("dm".to_owned(), "pm".to_owned(), 1),
            ("inbox".to_owned(), "coder".to_owned(), 2),
            ("inbox".to_owned(), "reviewer".to_owned(), 1),
        ]
    );

    let mut statement = conn
        .prepare("SELECT source_event_id,occurred_at,pane_id,kind,delivery_source,sender_alias,mailbox_alias,message_bytes,duration_ms,attempts,source_ref FROM agent_message_events ORDER BY source_event_id")
        .unwrap();
    let events_written: Vec<_> = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        events_written.len(),
        2,
        "only inbox.sent/message.sent rows with a pane"
    );
    assert_eq!(
        events_written[0],
        (
            1,
            "2026-11-05T18:00:00Z".to_owned(),
            "%7".to_owned(),
            "inbox.sent".to_owned(),
            "cli".to_owned(),
            None,
            Some("coder".to_owned()),
            Some(120),
            Some(5),
            Some(1),
            "dunkingsheep:metrics.db:events:1".to_owned(),
        ),
        "an alias containing '@' is dropped"
    );
    assert_eq!(
        events_written[1].4, "unknown",
        "a NULL delivery source becomes 'unknown'"
    );
    assert_eq!(
        events_written[1].5,
        Some("x".repeat(120)),
        "aliases are cut to 120 characters"
    );
    assert_eq!(events_written[1].6, None);
    assert_eq!(events_written[1].7, None, "NULL byte counts stay NULL");
    assert_eq!(events_written[1].10, "dunkingsheep:metrics.db:events:2");

    let mut statement = conn
        .prepare("SELECT number,state,head_oid,merged_at,first_seen_at,last_seen_at FROM pr_latest ORDER BY number")
        .unwrap();
    let latest: Vec<_> = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        latest,
        vec![
            (
                7,
                "OPEN".to_owned(),
                "aaa111".to_owned(),
                None,
                observed.to_owned(),
                observed.to_owned()
            ),
            (
                8,
                "MERGED".to_owned(),
                "bbb222".to_owned(),
                Some("2026-11-05T18:00:00Z".to_owned()),
                observed.to_owned(),
                observed.to_owned()
            ),
        ]
    );
    assert_eq!(count(&conn, "pr_state_events"), 2);

    assert_eq!(
        read_row::<String>(&conn, "SELECT payload_json FROM dunk_metrics_snapshots"),
        r#"{"generated_at":"2026-11-05T19:00:00Z","interval":{"end":"2026-11-05T19:00:00Z","start":"2026-11-05T18:00:00Z"},"panes":3,"schema_version":1}"#,
        "payload is compact, key-sorted JSON"
    );
    assert_eq!(
        read_row::<i64>(&conn, "SELECT schema_version FROM dunk_metrics_snapshots"),
        1
    );

    assert_eq!(
        read_row::<String>(&conn, "SELECT commit_oid FROM accounting_snapshots"),
        SHA
    );
    assert_eq!(
        read_row::<i64>(&conn, "SELECT strict_full_c FROM accounting_snapshots"),
        10
    );
    assert_eq!(
        read_row::<i64>(
            &conn,
            "SELECT verified_game_identities FROM accounting_snapshots"
        ),
        12
    );
    assert_eq!(
        read_row::<i64>(&conn, "SELECT ceiling FROM accounting_snapshots"),
        20
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT bound_kind FROM accounting_snapshots"),
        "upper_bound"
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT source_signature FROM accounting_snapshots"),
        INPUTS_SIGNATURE,
        "signature is sha256 of the sorted, default-separator JSON of the inputs"
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT evidence_ref FROM accounting_snapshots"),
        "docs/re/evidence.md"
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT project FROM accounting_snapshots"),
        PROJECT
    );

    assert_eq!(calls.count("list_panes"), 1);
    assert_eq!(calls.count("metrics"), 1);
    assert_eq!(calls.count("pr_list"), 1);
    assert_eq!(calls.count("published_ref"), 1);
    assert_eq!(calls.count("published_contents"), 2);
}

#[test]
fn external_sources_follow_the_five_minute_cadence_and_force_overrides() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let daemon_calls = Calls::default();
    let github_calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&daemon_calls),
        FakeGitHub::healthy(&github_calls),
    );
    let start = now();

    collector
        .collect(&mut conn, start, false, run_count)
        .unwrap();
    assert_eq!(github_calls.count("pr_list"), 1);
    assert_eq!(daemon_calls.count("metrics"), 1);
    assert_eq!(github_calls.count("published_ref"), 1);

    collector
        .collect(
            &mut conn,
            start + TimeDelta::seconds(EXTERNAL_INTERVAL_S - 1),
            false,
            run_count,
        )
        .unwrap();
    assert_eq!(
        daemon_calls.count("list_panes"),
        2,
        "panes are read on every run"
    );
    assert_eq!(github_calls.count("pr_list"), 1, "299 s old is not due");
    assert_eq!(daemon_calls.count("metrics"), 1);
    assert_eq!(github_calls.count("published_ref"), 1);
    let cached: (String, String, String) = conn
        .query_row(
            "SELECT github_state,dunk_metrics_state,accounting_state FROM collection_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        cached,
        (
            "cached".to_owned(),
            "cached".to_owned(),
            "cached".to_owned()
        )
    );

    collector
        .collect(
            &mut conn,
            start + TimeDelta::seconds(EXTERNAL_INTERVAL_S),
            false,
            run_count,
        )
        .unwrap();
    assert_eq!(github_calls.count("pr_list"), 2, "exactly 300 s old is due");
    assert_eq!(daemon_calls.count("metrics"), 2);

    collector
        .collect(
            &mut conn,
            start + TimeDelta::seconds(EXTERNAL_INTERVAL_S + 1),
            true,
            run_count,
        )
        .unwrap();
    assert_eq!(
        github_calls.count("pr_list"),
        3,
        "force_external reads even when fresh"
    );
    assert_eq!(daemon_calls.count("metrics"), 3);
    assert_eq!(count(&conn, "collection_runs"), 4);
}

#[test]
fn caught_failures_record_python_exception_names_and_stay_due_for_the_next_run() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = dir.path().join("absent-messages.sqlite3");
    let events = dir.path().join("absent-metrics.db");
    let daemon_calls = Calls::default();
    let github_calls = Calls::default();
    let daemon = FakeDaemon {
        panes: Err(SourceError {
            python_type: "RuntimeError",
            detail: "dunkingsheep exited 1".to_owned(),
        }),
        metrics: Err(SourceError {
            python_type: "JSONDecodeError",
            detail: "Expecting value".to_owned(),
        }),
        ..FakeDaemon::healthy(&daemon_calls)
    };
    let github = FakeGitHub {
        prs: Ok(json!({"not": "a list"})),
        reference: Ok(json!({"object": {"sha": "NOT-A-SHA"}})),
        ..FakeGitHub::healthy(&github_calls)
    };
    let mut collector = collector_with(&messages, &events, daemon, github);

    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();

    let (states, errors): (String, String) = conn
        .query_row(
            "SELECT panes_state||','||vp_messages_state||','||agent_messages_state||','||github_state||','||dunk_metrics_state||','||accounting_state, errors_json FROM collection_runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        states,
        "unavailable,unavailable,unavailable,unavailable,unavailable,unavailable"
    );
    assert_eq!(
        errors,
        r#"{"accounting": "ValueError", "agent_messages": "FileNotFoundError", "dunk_metrics": "JSONDecodeError", "github": "ValueError", "panes": "RuntimeError", "vp_messages": "FileNotFoundError"}"#,
        "errors_json uses Python's default separators and sorted keys"
    );
    assert_eq!(
        count(&conn, "pane_samples"),
        0,
        "an unavailable pane list samples nothing"
    );

    collector
        .collect(&mut conn, now() + TimeDelta::seconds(10), false, run_count)
        .unwrap();
    assert_eq!(
        github_calls.count("pr_list"),
        2,
        "no ok row means the source is due again"
    );
    assert_eq!(daemon_calls.count("metrics"), 2);
    assert_eq!(github_calls.count("published_ref"), 2);
}

type DaemonMutation = fn(&mut FakeDaemon, &mut FakeGitHub);

#[test]
fn escaping_faults_roll_back_the_whole_run() {
    let cases: [(&str, DaemonMutation, &str); 3] = [
        (
            "PR entry without a number",
            |_, github| github.prs = Ok(json!([{"state": "OPEN"}])),
            "KeyError",
        ),
        (
            "pane entry that is not a mapping",
            |daemon, _| daemon.panes = Ok(json!({"panes": ["%1"]})),
            "AttributeError",
        ),
        (
            "daemon reply that is not a mapping",
            |daemon, _| daemon.panes = Ok(json!([])),
            "TypeError",
        ),
    ];
    for (name, mutate, expected) in cases {
        let dir = TempDir::new().unwrap();
        let mut conn = ops_db(dir.path());
        let messages = message_db(dir.path(), &[("inbox", "coder")]);
        let events = events_db(dir.path(), &[]);
        let calls = Calls::default();
        let mut daemon = FakeDaemon::healthy(&calls);
        let mut github = FakeGitHub::healthy(&calls);
        mutate(&mut daemon, &mut github);
        let mut collector = collector_with(&messages, &events, daemon, github);

        match collector.collect(&mut conn, now(), true, run_count) {
            Err(CollectError::Fault(fault)) => assert_eq!(fault.python_type, expected, "{name}"),
            other => panic!("{name}: expected a {expected} fault, got {other:?}"),
        }
        assert_no_rows_written(&conn);
    }
}

#[test]
fn duplicate_pane_ids_violate_the_key_and_roll_back() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let daemon = FakeDaemon {
        panes: Ok(json!({"panes": [
            {"workspace_label": "poolrad-decomp", "pane_id": "%1"},
            {"workspace_label": "poolrad-decomp", "pane_id": "%1"}
        ]})),
        ..FakeDaemon::healthy(&calls)
    };
    let mut collector = collector_with(&messages, &events, daemon, FakeGitHub::healthy(&calls));

    let error = collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap_err();

    assert!(matches!(error, CollectError::Sqlite(_)), "got {error:?}");
    assert_no_rows_written(&conn);
}

#[test]
fn pr_updates_are_last_write_wins_and_state_events_are_idempotent() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let github = FakeGitHub {
        prs: Ok(json!([
            {"number": 7, "state": "OPEN", "headRefOid": "a1", "mergedAt": null},
            {"number": 7, "state": "MERGED", "headRefOid": "b2", "mergedAt": "2026-11-05T18:00:00Z"},
            {"number": 9, "state": "OPEN", "headRefOid": ""}
        ])),
        ..FakeGitHub::healthy(&calls)
    };
    let mut collector = collector_with(&messages, &events, FakeDaemon::healthy(&calls), github);

    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();
    let first: (String, String, String) = conn
        .query_row(
            "SELECT state,head_oid,first_seen_at FROM pr_latest WHERE number=7",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        first,
        (
            "MERGED".to_owned(),
            "b2".to_owned(),
            "2026-11-05T19:28:18Z".to_owned()
        )
    );
    assert_eq!(
        count(&conn, "pr_state_events"),
        3,
        "two states for #7 and one for #9"
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT head_oid FROM pr_state_events WHERE number=9"),
        "",
        "a falsy head is stored as the empty string"
    );

    collector
        .collect(&mut conn, now() + TimeDelta::seconds(10), true, run_count)
        .unwrap();
    let second: (String, String, String, String) = conn
        .query_row(
            "SELECT state,head_oid,first_seen_at,last_seen_at FROM pr_latest WHERE number=7",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        second,
        (
            "MERGED".to_owned(),
            "b2".to_owned(),
            "2026-11-05T19:28:18Z".to_owned(),
            "2026-11-05T19:28:28Z".to_owned()
        ),
        "first_seen is kept, last_seen moves"
    );
    assert_eq!(
        count(&conn, "pr_state_events"),
        3,
        "a repeated (number, state, head) is ignored"
    );
}

#[test]
fn agent_events_are_incremental_and_time_errors_split_by_python_rule() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let t0 = epoch(18, 0);
    let events = events_db(
        dir.path(),
        &[
            format!(
                "INSERT INTO events VALUES (1, {t0}, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"
            ),
            format!(
                "INSERT INTO events VALUES (2, {t0}, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"
            ),
        ],
    );
    let calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();
    assert_eq!(count(&conn, "agent_message_events"), 2);

    let writer = Connection::open(&events).unwrap();
    writer
        .execute_batch(&format!(
            "INSERT INTO events VALUES (3, {t0}, 'message.sent', '%8', 'cli', 'a', 'b', 1, 1, 1);"
        ))
        .unwrap();
    collector
        .collect(&mut conn, now() + TimeDelta::seconds(1), false, run_count)
        .unwrap();
    assert_eq!(
        count(&conn, "agent_message_events"),
        3,
        "only event 3 is new"
    );
    collector
        .collect(&mut conn, now() + TimeDelta::seconds(2), false, run_count)
        .unwrap();
    assert_eq!(
        count(&conn, "agent_message_events"),
        3,
        "a rerun adds nothing"
    );

    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(
        dir.path(),
        &["INSERT INTO events VALUES (1, 'not-a-time', 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)".to_owned()],
    );
    let calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();
    let (state, errors): (String, String) = conn
        .query_row(
            "SELECT agent_messages_state, errors_json FROM collection_runs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (state.as_str(), errors.as_str()),
        ("unavailable", r#"{"agent_messages": "ValueError"}"#)
    );
    assert_eq!(count(&conn, "agent_message_events"), 0);

    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(
        dir.path(),
        &[
            "INSERT INTO events VALUES (1, NULL, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"
                .to_owned(),
        ],
    );
    let calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    match collector.collect(&mut conn, now(), false, run_count) {
        Err(CollectError::Fault(fault)) => assert_eq!(fault.python_type, "TypeError"),
        other => panic!("a NULL event time escapes as TypeError, got {other:?}"),
    }
    assert_no_rows_written(&conn);
}

#[test]
fn metrics_validation_matches_python_rules() {
    assert!(valid_dunk_metrics(&valid_metrics()).is_ok());
    assert!(valid_dunk_metrics(&json!({"schema_version": true, "generated_at": 1700000000, "interval": {"start": "2026-11-05T18:00:00Z", "end": "2026-11-05T19:00:00Z"}})).is_ok(), "True == 1 in Python");
    assert!(valid_dunk_metrics(&json!({"schema_version": 2, "generated_at": "2026-11-05T19:00:00Z", "interval": {"start": "2026-11-05T18:00:00Z", "end": "2026-11-05T19:00:00Z"}})).is_err());
    assert!(
        valid_dunk_metrics(&json!({"schema_version": 1, "generated_at": "2026-11-05T19:00:00Z"}))
            .is_err(),
        "interval is required"
    );
    assert!(
        valid_dunk_metrics(&json!({
            "schema_version": 1,
            "generated_at": "2026-11-05T19:00:00Z",
            "interval": {"start": "2026-11-05T18:00:00Z", "end": "2026-11-05T19:00:00Z"},
            "rows": [{"Body": "private text"}]
        }))
        .is_err(),
        "content-bearing keys are rejected at any depth, case-insensitively"
    );
    assert!(
        dunk_timestamp(Some(&json!(true)), "x").is_err(),
        "bools are not timestamps"
    );
    assert!(
        dunk_timestamp(Some(&json!(f64::NAN)), "x").is_err(),
        "non-finite numbers are not timestamps"
    );
    assert_eq!(
        dunk_timestamp(Some(&json!(1700000000)), "x").unwrap(),
        "2023-11-14T22:13:20Z"
    );
}

#[test]
fn metrics_payload_keeps_python_true_and_numeric_timestamps() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let daemon = FakeDaemon {
        metrics: Ok(json!({
            "schema_version": true,
            "generated_at": 1700000000,
            "interval": {"start": "2026-11-05T18:00:00Z", "end": "2026-11-05T19:00:00Z"},
            "panes": 3
        })),
        ..FakeDaemon::healthy(&calls)
    };
    let mut collector = collector_with(&messages, &events, daemon, FakeGitHub::healthy(&calls));

    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();

    let (generated, payload, version): (String, String, i64) = conn
        .query_row(
            "SELECT generated_at, payload_json, schema_version FROM dunk_metrics_snapshots",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(generated, "2023-11-14T22:13:20Z");
    assert_eq!(
        payload,
        r#"{"generated_at":1700000000,"interval":{"end":"2026-11-05T19:00:00Z","start":"2026-11-05T18:00:00Z"},"panes":3,"schema_version":true}"#
    );
    assert_eq!(version, 1, "the column is always the Python literal 1");
}

#[test]
fn published_accounting_rejects_unreviewed_or_mismatched_inputs() {
    let calls = Calls::default();
    let accepted = published_accounting(&mut FakeGitHub::healthy(&calls)).unwrap();
    assert_eq!(accepted.commit_oid, SHA);
    assert_eq!(accepted.source_signature, INPUTS_SIGNATURE);
    assert_eq!(accepted.evidence_ref, "docs/re/evidence.md");

    let mut bool_schema = checkpoint_json();
    bool_schema["schema_version"] = json!(true);
    assert!(
        published_accounting(&mut FakeGitHub::with_documents(
            &calls,
            &bool_schema,
            &census_json()
        ))
        .is_ok(),
        "schema_version true equals 1 in Python"
    );

    type Mutation = fn(&mut Value, &mut Value);
    let rejected: [(&str, Mutation); 12] = [
        ("census still a draft", |_, census| {
            census["status"] = json!("draft")
        }),
        ("reviewer is not PM", |_, census| {
            census["reviewed_by"] = json!("agent")
        }),
        ("unknown bound kind", |_, census| {
            census["bound_kind"] = json!("lower")
        }),
        ("another census scope", |_, census| {
            census["scope"] = json!("other")
        }),
        ("digests differ from inputs", |_, census| {
            census["source_digests"] = json!({"a": "y", "b": 2})
        }),
        ("boolean strict count", |checkpoint, _| {
            checkpoint["strict_full_c_originals"] = json!(true)
        }),
        ("float strict count", |checkpoint, _| {
            checkpoint["strict_full_c_originals"] = json!(10.0)
        }),
        ("zero ceiling", |_, census| census["count"] = json!(0)),
        ("strict above identities", |checkpoint, _| {
            checkpoint["strict_full_c_originals"] = json!(30)
        }),
        ("empty inputs", |checkpoint, census| {
            checkpoint["inputs"] = json!({});
            census["source_digests"] = json!({});
        }),
        ("evidence outside docs/re", |_, census| {
            census["evidence_doc"] = json!("elsewhere/evidence.md")
        }),
        ("checkpoint schema 2", |checkpoint, _| {
            checkpoint["schema_version"] = json!(2)
        }),
    ];
    for (name, mutate) in rejected {
        let (mut checkpoint, mut census) = (checkpoint_json(), census_json());
        mutate(&mut checkpoint, &mut census);
        let result = published_accounting(&mut FakeGitHub::with_documents(
            &calls,
            &checkpoint,
            &census,
        ));
        match result {
            Err(error) => assert_eq!(error.python_type, "ValueError", "{name}"),
            Ok(_) => panic!("{name} must be rejected"),
        }
    }

    let mut bad_sha = FakeGitHub::healthy(&calls);
    bad_sha.reference = Ok(json!({"object": {"sha": SHA.to_uppercase()}}));
    assert_eq!(
        published_accounting(&mut bad_sha).unwrap_err().python_type,
        "ValueError"
    );

    let mut bad_encoding = FakeGitHub::healthy(&calls);
    bad_encoding.contents.insert(
        CHECKPOINT_PATH.to_owned(),
        Ok(json!({"encoding": "utf-8", "content": "e30="})),
    );
    assert_eq!(
        published_accounting(&mut bad_encoding)
            .unwrap_err()
            .python_type,
        "ValueError"
    );

    let mut bad_content = FakeGitHub::healthy(&calls);
    bad_content.contents.insert(
        CENSUS_PATH.to_owned(),
        Ok(json!({"encoding": "base64", "content": "!!!"})),
    );
    assert_eq!(
        published_accounting(&mut bad_content)
            .unwrap_err()
            .python_type,
        "ValueError"
    );
}

/// Writes an owned `gh` script. The script logs each argv with its working directory, then
/// answers from the files the test wrote. Paths are absolute and contain no quotes.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn gh_subprocess_reads_use_python_argv_cwd_and_exception_names() {
    let dir = TempDir::new().unwrap();
    let repo = dir.path().join("poolrad");
    fs::create_dir(&repo).unwrap();
    let log = dir.path().join("gh.log");
    let prs = dir.path().join("prs.json");
    let reference = dir.path().join("ref.json");
    let checkpoint = dir.path().join("checkpoint.json");
    let census = dir.path().join("census.json");
    fs::write(
        &prs,
        r#"[{"number": 7, "state": "OPEN", "headRefOid": "abc", "mergedAt": null}]"#,
    )
    .unwrap();
    fs::write(&reference, json!({"object": {"sha": SHA}}).to_string()).unwrap();
    fs::write(&checkpoint, published_file(&checkpoint_json()).to_string()).unwrap();
    fs::write(&census, published_file(&census_json()).to_string()).unwrap();
    let program = write_script(
        dir.path(),
        "gh",
        &format!(
            r#"printf '%s @ %s\n' "$*" "$(pwd)" >> '{log}'
case "$*" in
  'pr list --state all --limit 100 --json number,state,headRefOid,mergedAt') cat '{prs}' ;;
  'api repos/{PUBLISHED_REPO}/git/ref/heads/main') cat '{reference}' ;;
  'api repos/{PUBLISHED_REPO}/contents/{CHECKPOINT_PATH}?ref={SHA}') cat '{checkpoint}' ;;
  'api repos/{PUBLISHED_REPO}/contents/{CENSUS_PATH}?ref={SHA}') cat '{census}' ;;
  *) echo "unexpected: $*" >&2; exit 9 ;;
esac"#,
            log = log.display(),
            prs = prs.display(),
            reference = reference.display(),
            checkpoint = checkpoint.display(),
            census = census.display(),
        ),
    );
    let mut gh = GhCli {
        program: program.clone(),
        repo_dir: repo.clone(),
        timeout: Duration::from_secs(5),
    };

    assert_eq!(gh.pr_list().unwrap().as_array().unwrap().len(), 1);
    let accepted = published_accounting(&mut gh).unwrap();
    assert_eq!(accepted.source_signature, INPUTS_SIGNATURE);
    let logged = fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains(&format!("@ {}", repo.display())),
        "gh pr list runs in the poolrad checkout, log was: {logged}"
    );
    assert!(logged.contains("api repos/huntergdavis/poolrad-decomp/git/ref/heads/main @"));

    let failing = |name: &str, body: &str, timeout: Duration| -> Result<Value, SourceError> {
        let script = write_script(dir.path(), name, body);
        GhCli {
            program: script,
            repo_dir: repo.clone(),
            timeout,
        }
        .pr_list()
    };
    let generous = Duration::from_secs(5);
    assert_eq!(
        failing("exits", "exit 3", generous)
            .unwrap_err()
            .python_type,
        "RuntimeError"
    );
    assert_eq!(
        failing("garbage", "echo '{'", generous)
            .unwrap_err()
            .python_type,
        "JSONDecodeError"
    );
    assert_eq!(
        failing("binary", r"printf '\377'", generous)
            .unwrap_err()
            .python_type,
        "UnicodeDecodeError"
    );
    let started = Instant::now();
    assert_eq!(
        failing("slow", "exec sleep 5", Duration::from_millis(200))
            .unwrap_err()
            .python_type,
        "TimeoutExpired"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "a timed-out command is killed"
    );
    let missing = GhCli {
        program: dir.path().join("no-such-gh"),
        repo_dir: repo,
        timeout: generous,
    };
    assert_eq!(
        missing.clone().pr_list().unwrap_err().python_type,
        "FileNotFoundError"
    );
}

#[test]
fn daemon_socket_round_trips_and_maps_each_failure_like_the_cli() {
    enum Reply {
        Answer(Value),
        Refused,
        Garbage,
        Silence,
    }
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let replies = vec![
        Reply::Answer(json!({"panes": []})),
        Reply::Refused,
        Reply::Garbage,
        Reply::Silence,
        Reply::Answer(json!({"panes": []})),
        Reply::Answer(valid_metrics()),
    ];
    let server = std::thread::spawn(move || {
        for reply in replies {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].clone();
            let mut writer = stream;
            match reply {
                Reply::Answer(result) => writeln!(
                    writer,
                    "{}",
                    json!({"id": id, "ok": true, "result": result})
                )
                .unwrap(),
                Reply::Refused => writeln!(
                    writer,
                    "{}",
                    json!({"id": id, "ok": false, "error": "refused"})
                )
                .unwrap(),
                Reply::Garbage => writeln!(writer, "not json").unwrap(),
                Reply::Silence => {}
            }
        }
    });

    let mut client = FlockClient::new(
        UnixTransport {
            path: socket.clone(),
        },
        None,
        "ops-collector-test".to_owned(),
        Duration::from_secs(5),
        None,
    );
    let direct: Vec<Result<Value, SourceError>> = (0..4)
        .map(|_| DaemonCalls::call(&mut client, "list_panes", Map::new()))
        .collect();
    assert_eq!(direct[0].as_ref().unwrap(), &json!({"panes": []}));
    assert_eq!(
        direct[1].as_ref().unwrap_err().python_type,
        "RuntimeError",
        "ok:false is a refusal"
    );
    assert_eq!(
        direct[2].as_ref().unwrap_err().python_type,
        "JSONDecodeError",
        "a non-JSON reply"
    );
    assert_eq!(
        direct[3].as_ref().unwrap_err().python_type,
        "RuntimeError",
        "no reply is unavailable"
    );

    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let mut collector = Collector::new(
        config(&messages, &events),
        client,
        FakeGitHub::healthy(&calls),
    );
    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();
    let errors: String = read_row(&conn, "SELECT errors_json FROM collection_runs");
    assert_eq!(
        errors, "{}",
        "the socket serves list_panes and metrics for a real run"
    );
    server.join().unwrap();
}

#[test]
fn rollback_then_restart_keeps_committed_history() {
    let dir = TempDir::new().unwrap();
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let mut conn = ops_db(dir.path());
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();

    let faulty = FakeGitHub {
        prs: Ok(json!([{"state": "OPEN"}])),
        ..FakeGitHub::healthy(&calls)
    };
    let mut faulting = collector_with(&messages, &events, FakeDaemon::healthy(&calls), faulty);
    assert!(matches!(
        faulting.collect(&mut conn, now() + TimeDelta::seconds(60), true, run_count),
        Err(CollectError::Fault(_))
    ));
    assert_eq!(
        count(&conn, "collection_runs"),
        1,
        "the failed run left nothing behind"
    );
    assert_eq!(count(&conn, "pane_samples"), 0);

    drop(conn);
    drop(faulting);
    let mut reopened = ops_db(dir.path());
    let mut restarted = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    let runs = restarted
        .collect(
            &mut reopened,
            now() + TimeDelta::seconds(120),
            false,
            run_count,
        )
        .unwrap();
    assert_eq!(runs, 2);
    let latest: (i64, String) = reopened
        .query_row(
            "SELECT id, observed_at FROM collection_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(latest, (2, "2026-11-05T19:30:18Z".to_owned()));
}

#[test]
fn summary_runs_after_commit_and_its_error_keeps_the_run() {
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );

    let seen = collector
        .collect(&mut conn, now(), false, |conn, _| -> Result<i64, String> {
            conn.query_row("SELECT COUNT(*) FROM collection_runs", [], |row| row.get(0))
                .map_err(|error| error.to_string())
        })
        .unwrap();
    assert_eq!(seen, 1, "the summary reads the committed row");

    let failed = collector.collect(
        &mut conn,
        now() + TimeDelta::seconds(5),
        false,
        |_, _| -> Result<i64, String> { Err("summary exploded".to_owned()) },
    );
    match failed {
        Err(CollectError::Summary(message)) => assert_eq!(message, "summary exploded"),
        other => panic!("expected a summary error, got {other:?}"),
    }
    assert_eq!(
        count(&conn, "collection_runs"),
        2,
        "the run stays committed"
    );
}

/// Text-level isolation of one top-level Python definition (no Python is run).
fn top_level_def(text: &str, name: &str) -> String {
    let header = format!("def {name}(");
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with(&header) {
            inside = true;
        } else if inside && (line.starts_with("def ") || line.starts_with("class ")) {
            break;
        }
        if inside {
            body.push_str(line);
            body.push('\n');
        }
    }
    assert!(inside, "def {name} not found in the Python source");
    body
}

fn python_constant(text: &str, name: &str) -> String {
    let prefix = format!("{name} = ");
    text.lines()
        .find_map(|line| {
            line.strip_prefix(&prefix)
                .map(|value| value.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("{name} missing from the Python source"))
}

fn create_table_names(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            line.split("CREATE TABLE IF NOT EXISTS ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(|name| name.trim_end_matches('(').to_owned())
        })
        .collect()
}

#[test]
fn python_source_still_has_the_contract_this_port_encodes() {
    let python = fs::read_to_string(PYTHON_SOURCE)
        .expect("bridges/ops_metrics.py is part of the repository checkout");

    assert_eq!(python_constant(&python, "PROJECT"), "\"poolrad\"");
    assert_eq!(
        python_constant(&python, "POLL_INTERVAL_S"),
        POLL_INTERVAL_S.to_string()
    );
    assert_eq!(
        python_constant(&python, "EXTERNAL_INTERVAL_S"),
        EXTERNAL_INTERVAL_S.to_string()
    );
    assert_eq!(
        python_constant(&python, "PUBLISHED_REPO"),
        format!("\"{PUBLISHED_REPO}\"")
    );
    assert_eq!(PROJECT, "poolrad");

    let collect = top_level_def(&python, "collect");
    for needle in [
        "\"panes\": \"skipped\"",
        "\"github\": \"skipped\"",
        "\"vp_messages\": \"skipped\"",
        "\"agent_messages\": \"skipped\"",
        "\"dunk_metrics\": \"skipped\"",
        "\"accounting\": \"skipped\"",
        "\"cached\"",
        "workspace_label\") != \"poolrad-decomp\"",
        "ON CONFLICT(project,number) DO UPDATE SET",
        "INSERT OR IGNORE INTO agent_message_events",
        "INSERT OR IGNORE INTO dunk_metrics_snapshots",
        "source_event_id",
        "json.dumps(errors, sort_keys=True)",
    ] {
        assert!(
            collect.contains(needle),
            "Python collect no longer contains {needle:?}"
        );
    }
    let record_accounting = top_level_def(&python, "record_accounting");
    assert!(record_accounting.contains("INSERT OR IGNORE INTO accounting_snapshots"));

    assert_eq!(
        create_table_names(&python),
        create_table_names(OPS_SCHEMA),
        "the Python SCHEMA and ops_schema.sql declare the same tables"
    );
}

/// Parses a JSON fixture with `serde_json`, so integers above 64 bits keep their exact digits.
fn doc(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON fixture")
}

/// Python's `int()` accepts any size, so an oversized value fails only when SQLite binds it, inside
/// the write transaction. Malformed text is a caught source error, and the run still commits.
#[test]
fn oversized_integers_fault_at_the_sqlite_bind_not_at_the_source_read() {
    let t0 = epoch(18, 0);
    let run_with_event = |id_sql: &str| {
        let dir = TempDir::new().unwrap();
        let mut conn = ops_db(dir.path());
        let messages = message_db(dir.path(), &[]);
        let events = events_db(
            dir.path(),
            &[format!(
                "INSERT INTO events VALUES ({id_sql}, {t0}, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"
            )],
        );
        let calls = Calls::default();
        let mut collector = collector_with(
            &messages,
            &events,
            FakeDaemon::healthy(&calls),
            FakeGitHub::healthy(&calls),
        );
        let outcome = collector.collect(&mut conn, now(), false, run_count);
        (dir, conn, outcome)
    };

    for id_sql in ["'99999999999999999999'", "1e30"] {
        let (_dir, conn, outcome) = run_with_event(id_sql);
        match outcome {
            Err(CollectError::Fault(fault)) => {
                assert_eq!(fault.python_type, "OverflowError", "{id_sql}")
            }
            other => panic!("{id_sql}: expected an OverflowError fault, got {other:?}"),
        }
        assert_no_rows_written(&conn);
    }

    let (_dir, conn, outcome) = run_with_event("'12abc'");
    assert_eq!(
        outcome.unwrap(),
        1,
        "a malformed id is caught, so the run commits"
    );
    assert_eq!(
        read_row::<String>(&conn, "SELECT errors_json FROM collection_runs"),
        r#"{"agent_messages": "ValueError"}"#
    );
    assert_eq!(count(&conn, "agent_message_events"), 0);

    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(
        dir.path(),
        &[
            format!("INSERT INTO events VALUES (' 42 ', {t0}, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"),
            format!("INSERT INTO events VALUES (7.9, {t0}, 'inbox.sent', '%7', 'cli', 'a', 'b', 1, 1, 1)"),
        ],
    );
    let calls = Calls::default();
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::healthy(&calls),
    );
    collector
        .collect(&mut conn, now(), false, run_count)
        .unwrap();
    collector
        .collect(&mut conn, now() + TimeDelta::seconds(1), false, run_count)
        .unwrap();
    assert_eq!(
        count(&conn, "agent_message_events"),
        2,
        "a TEXT id sorts above the newest integer, so reruns re-read it and OR IGNORE keeps one row"
    );
    let mut statement = conn
        .prepare(
            "SELECT source_event_id, source_ref FROM agent_message_events ORDER BY source_event_id",
        )
        .unwrap();
    let written: Vec<(i64, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        written,
        vec![
            (7, "dunkingsheep:metrics.db:events:7.9".to_owned()),
            (42, "dunkingsheep:metrics.db:events: 42 ".to_owned()),
        ],
        "int() strips whitespace, and a REAL id keeps Python's float spelling in source_ref"
    );

    // A PR number above the 64-bit range binds before the list-valued head is converted, so the
    // fault is OverflowError, not ProgrammingError.
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let calls = Calls::default();
    let github = FakeGitHub {
        prs: Ok(doc(
            r#"[{"number": 99999999999999999999, "state": "OPEN", "headRefOid": [1]}]"#,
        )),
        ..FakeGitHub::healthy(&calls)
    };
    let mut collector = collector_with(&messages, &events, FakeDaemon::healthy(&calls), github);
    match collector.collect(&mut conn, now(), false, run_count) {
        Err(CollectError::Fault(fault)) => assert_eq!(fault.python_type, "OverflowError"),
        other => panic!("expected OverflowError for the PR number, got {other:?}"),
    }
    assert_no_rows_written(&conn);

    // Accounting counts above the 64-bit range pass Python's validation, then fail at the bind.
    let checkpoint = doc(
        r#"{"schema_version": 1, "strict_full_c_originals": 99999999999999999999, "verified_game_identities": 99999999999999999999, "inputs": {"b": 2, "a": "x"}}"#,
    );
    let census = doc(
        r#"{"count": 99999999999999999999, "status": "reviewed", "reviewed_by": "PM", "bound_kind": "upper_bound", "scope": "original-game-functions-overlay-and-resident", "source_digests": {"a": "x", "b": 2}, "evidence_doc": "docs/re/evidence.md"}"#,
    );
    let accepted = published_accounting(&mut FakeGitHub::with_documents(
        &calls,
        &checkpoint,
        &census,
    ))
    .unwrap();
    assert_eq!(accepted.strict_full_c.as_decimal(), "99999999999999999999");
    let dir = TempDir::new().unwrap();
    let mut conn = ops_db(dir.path());
    let messages = message_db(dir.path(), &[]);
    let events = events_db(dir.path(), &[]);
    let mut collector = collector_with(
        &messages,
        &events,
        FakeDaemon::healthy(&calls),
        FakeGitHub::with_documents(&calls, &checkpoint, &census),
    );
    match collector.collect(&mut conn, now(), false, run_count) {
        Err(CollectError::Fault(fault)) => assert_eq!(fault.python_type, "OverflowError"),
        other => panic!("expected OverflowError for the accounting counts, got {other:?}"),
    }
    assert_no_rows_written(&conn);
}

/// Python compares an int with a float exactly, so `10**30` (an int) and `1e30` (a float equal to
/// 1000000000000000019884624838656) are different values, and only an identical int matches.
#[test]
fn json_numbers_compare_exactly_like_python() {
    let calls = Calls::default();
    let checkpoint = doc(
        r#"{"schema_version": 1, "strict_full_c_originals": 1, "verified_game_identities": 2, "inputs": {"a": 1000000000000000000000000000000}}"#,
    );
    let census = |digests: &str| {
        doc(&format!(
            r#"{{"count": 3, "status": "reviewed", "reviewed_by": "PM", "bound_kind": "exact", "scope": "original-game-functions-overlay-and-resident", "source_digests": {digests}, "evidence_doc": "docs/re/x.md"}}"#
        ))
    };
    for digests in [
        r#"{"a": 1e30}"#,
        r#"{"a": 1000000000000000000000000000000.0}"#,
    ] {
        let result = published_accounting(&mut FakeGitHub::with_documents(
            &calls,
            &checkpoint,
            &census(digests),
        ));
        assert_eq!(result.unwrap_err().python_type, "ValueError", "{digests}");
    }
    let same = published_accounting(&mut FakeGitHub::with_documents(
        &calls,
        &checkpoint,
        &census(r#"{"a": 1000000000000000000000000000000}"#),
    ));
    assert!(same.is_ok(), "the identical integer compares equal");
}

#[test]
fn timestamps_and_json_spelling_match_cpython() {
    assert_eq!(
        iso(&fromtimestamp_utc(1.9999999).unwrap()),
        "1970-01-01T00:00:02Z",
        "microseconds round up into the next second before truncation"
    );
    assert_eq!(
        iso(&fromtimestamp_utc(1700000000.5).unwrap()),
        "2023-11-14T22:13:20Z"
    );
    assert_eq!(
        dunk_timestamp(Some(&json!("2026-11-05T12:00:00+02:00")), "x").unwrap(),
        "2026-11-05T10:00:00Z"
    );
    assert_eq!(
        iso(&parse_time("2026-11-05 10:00:00Z").unwrap()),
        "2026-11-05T10:00:00Z",
        "a space separator is accepted"
    );
    assert!(
        parse_time("2026-11-05T10:00:00").is_err(),
        "a naive timestamp is rejected"
    );

    assert_eq!(
        py_dumps(&json!({"b": 1, "a": "é"}), ", ", ": "),
        r#"{"a": "é", "b": 1}"#
    );
    assert_eq!(
        py_dumps(
            &json!({"n": 1e16, "f": 0.1, "w": 1.0, "s": "\u{7f}"}),
            ",",
            ":"
        ),
        r#"{"f":0.1,"n":1e+16,"s":"\u007f","w":1.0}"#,
        "floats use repr, and DEL is escaped"
    );
    assert_eq!(
        py_dumps(&json!("😀"), ",", ":"),
        r#""😀""#,
        "astral characters are surrogate pairs"
    );
    assert_eq!(
        py_dumps(&json!({"a": [1, null, false]}), ",", ":"),
        r#"{"a":[1,null,false]}"#
    );
}
