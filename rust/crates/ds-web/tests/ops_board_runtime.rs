//! Integration tests for `src/ops_board_runtime.rs`, the Rust port of `bridges/ops_board.py`
//! and `bridges/ops_budget.py`. The module is compiled into this test crate with `#[path]`, so
//! `lib.rs` needs no change. Every database is a temporary file, the daemon is a local Unix
//! socket, and the clock is controlled by the test. Nothing touches a live service, a
//! credential or the network, and Python is never run.

#[allow(dead_code)]
#[path = "../src/ops_collector.rs"]
mod ops_collector;

#[allow(dead_code)]
#[path = "../src/ops_summary.rs"]
mod ops_summary;

#[allow(dead_code)]
#[path = "../src/ops_board_runtime.rs"]
mod ops_board_runtime;

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension, Router,
};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use ds_core::client::{FlockClient, UnixTransport};
use ds_web::ops_history;
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use ops_board_runtime::{
    owner_board_backend, project, BoardClock, BoardRuntime, BoardRuntimeConfig, Dispatch, OpenDb,
    SharedDaemon, Subscription, UsageCache, UsageReader, PROVIDERS,
};
use ops_collector::{CollectorConfig, GhCli};

mod routes {
    pub use ds_web::routes::*;
}
use routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    ops_board,
    ops_board::OpsBoardBackend,
    ops_metrics,
    ops_metrics::OpsMetricsBackend,
};

/// The board's fixed clock: 2026-11-05T12:00:00Z.
fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap()
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

enum Reply {
    Answer(Value),
    Refuse(&'static str),
    Garbage,
}

type Handler = Arc<dyn Fn(&str, &Map<String, Value>) -> Reply + Send + Sync>;

/// A local Unix-socket daemon that answers one JSON request per connection, as the real
/// daemon does. `calls` records every command name in arrival order.
struct Daemon {
    socket: PathBuf,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn count(&self, command: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|name| name.as_str() == command)
            .count()
    }
}

fn start_daemon(dir: &Path, handler: Handler) -> Daemon {
    let socket = dir.join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind the fake daemon socket");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&calls);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(reader_side) = stream.try_clone() else {
                continue;
            };
            let mut line = String::new();
            if BufReader::new(reader_side)
                .read_line(&mut line)
                .unwrap_or(0)
                == 0
            {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = request["id"].clone();
            let command = request["cmd"].as_str().unwrap_or("").to_owned();
            let args = request["args"].as_object().cloned().unwrap_or_default();
            log.lock().unwrap().push(command.clone());
            let _ = match handler(&command, &args) {
                Reply::Answer(result) => {
                    writeln!(
                        stream,
                        "{}",
                        json!({"id": id, "ok": true, "result": result})
                    )
                }
                Reply::Refuse(error) => {
                    writeln!(stream, "{}", json!({"id": id, "ok": false, "error": error}))
                }
                Reply::Garbage => writeln!(stream, "not json"),
            };
        }
    });
    Daemon { socket, calls }
}

/// The web desk is pane-less, so its client writes with a service token, as the composition
/// will. Without one the client refuses `task_update` locally (identity refused).
fn shared_client(socket: &Path) -> SharedDaemon {
    SharedDaemon::new(FlockClient::new(
        UnixTransport {
            path: socket.to_path_buf(),
        },
        None,
        "ops-board-test".to_owned(),
        Duration::from_secs(5),
        Some("ops-board-test-token".to_owned()),
    ))
}

/// A clock the test moves by hand.
struct TestClock(Mutex<DateTime<Utc>>);

impl TestClock {
    fn set(&self, at: DateTime<Utc>) {
        *self.0.lock().unwrap() = at;
    }
}

impl BoardClock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

/// Provider readings the test sets per provider. An unset provider fails like a missing command.
#[derive(Default)]
struct Usage {
    readings: Mutex<HashMap<String, Result<Value, String>>>,
    calls: Mutex<Vec<String>>,
}

impl Usage {
    fn set(&self, provider: &str, reading: Result<Value, String>) {
        self.readings
            .lock()
            .unwrap()
            .insert(provider.to_owned(), reading);
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl UsageReader for Usage {
    fn read(&self, provider: &str) -> Result<Value, String> {
        self.calls.lock().unwrap().push(provider.to_owned());
        self.readings
            .lock()
            .unwrap()
            .get(provider)
            .cloned()
            .unwrap_or_else(|| Err("no reading configured".to_owned()))
    }
}

/// The operations-database opener the runtime receives. The schema is the accepted one.
fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!("../src/ops_schema.sql"))?;
    Ok(conn)
}

struct Harness {
    dir: TempDir,
    daemon: Daemon,
    usage: Arc<Usage>,
    clock: Arc<TestClock>,
    runtime: BoardRuntime,
}

fn config(dir: &Path, timer: Duration) -> BoardRuntimeConfig {
    BoardRuntimeConfig {
        interval_s: 30,
        ops_db: dir.join("operations.sqlite3"),
        host_watch_state: dir.join("host-watch-state.json"),
        opencode_db: dir.join("opencode.db"),
        collector: CollectorConfig {
            message_db: dir.join("messages.sqlite3"),
            dunk_events_db: dir.join("metrics.db"),
        },
        github: GhCli {
            program: dir.join("gh"),
            repo_dir: dir.to_path_buf(),
            timeout: Duration::from_secs(5),
        },
        usage_refresh_s: 1800,
        usage_backoff_s: vec![300, 600, 1800],
        usage_max_age_s: 3600,
        timer_period: timer,
    }
}

fn write_host_sample(dir: &Path, at: DateTime<Utc>) {
    let sample = json!({
        "levels": {"tmp": "warning"},
        "last_sample": {
            "at": at.timestamp() as f64,
            "tmp_free_bytes": 1_073_741_824_u64,
            "tmp_used_fraction": 0.5,
            "tmp_inode_used_fraction": 0.1,
            "load1": 0.5,
            "load5": 0.4,
            "cpus": 8,
            "cpu_psi_some_avg60": 0.0,
        },
    });
    std::fs::write(dir.join("host-watch-state.json"), sample.to_string()).unwrap();
}

fn harness(handler: Handler, open: OpenDb, timer: Duration) -> Harness {
    let dir = TempDir::new().expect("temporary directory");
    let daemon = start_daemon(dir.path(), handler);
    write_host_sample(dir.path(), now());
    let usage = Arc::new(Usage::default());
    let clock = Arc::new(TestClock(Mutex::new(now())));
    let runtime = BoardRuntime::new(
        config(dir.path(), timer),
        shared_client(&daemon.socket),
        open,
        clock.clone(),
        usage.clone(),
        Dispatch::Inline,
    );
    Harness {
        dir,
        daemon,
        usage,
        clock,
        runtime,
    }
}

fn seed_subscription(dir: &Path, provider: &str, cents: i64) {
    let conn = open_db(&dir.join("operations.sqlite3")).unwrap();
    conn.execute(
        "INSERT INTO subscription_costs(project, provider, monthly_usd_cents, observed_at, evidence_ref) \
         VALUES ('poolrad', ?1, ?2, '2026-11-01T00:00:00Z', 'owner')",
        rusqlite::params![provider, cents],
    )
    .unwrap();
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

/// The standard daemon: one open task blocked on the owner, one urgent task, one finished
/// owner ask outside the 24-hour window, the usual lane, schedule and review shapes.
fn standard_answer(command: &str, args: &Map<String, Value>) -> Reply {
    let t = now().timestamp();
    let include_done = args
        .get("include_done")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let urgent = json!({"id": "t1", "title": "Urgent review", "status": "open", "priority": "urgent",
        "scope": "global", "assignees": [], "created_at": t - 7200, "updated_at": t - 60,
        "eligible": true, "claimable": true, "attempts": 0, "created_by": "owner@desk"});
    let blocked = json!({"id": "t2", "title": "Blocked on t1", "status": "blocked", "priority": "high",
        "scope": "global", "assignees": ["%5"], "created_at": t - 3600, "updated_at": t - 30,
        "block_reason": "needs-owner: Ship the release or hold?", "needs_owner": true,
        "depends_on": ["t1"], "lease_expires_at": t + 120, "created_by": "pi"});
    let finished = json!({"id": "t3", "title": "Old owner ask", "status": "done", "priority": "low",
        "scope": "global", "assignees": [], "created_at": t - 99_999, "created_by": "owner"});
    let value = match command {
        "task_list" if include_done => json!({"tasks": [urgent, blocked, finished]}),
        "task_list" => json!({"tasks": [urgent, blocked]}),
        "task_metrics" => json!({"throughput": {"completed": 7, "buckets": [
            {"bucket": t - 1800, "completed": 2},
            {"bucket": t - 30_000, "completed": 3},
            {"bucket": t - 100_000, "completed": 4}]}}),
        "list_panes" => json!({"panes": [
            {"pane_id": "%1", "agent": "pi", "agent_status": "working", "is_self": false},
            {"pane_id": "%2", "agent": "", "agent_status": null, "is_self": false},
            {"pane_id": "%3", "agent": "claude", "agent_status": "idle", "is_self": false}],
            "registry_gap": [{"pane": "%9", "alive": false}, {"pane_id": "%8", "alive": true}]}),
        "list_dunks" => json!({"dunks": [
            {"id": "d1", "name": "nudge", "target_pane_id": "%1", "running": true,
             "last_error": "pane gone", "last_sent_at": t - 600, "interval_minutes": 5},
            {"id": "d2", "target_agent": "pi", "running": false, "last_error": null}]}),
        "health" => json!({"ok": true, "problems": ["disk low"], "repairs": [], "duration_ms": 12,
            "checks": {"memory": {"ok": true, "sources": 3, "chunks": 10, "queue_depth": 0},
                       "stale_leases": {"expired_leases": 0},
                       "wal_bytes": 1024, "disk_free_bytes": 5_000_000_000_u64}}),
        "report_list" => json!({"count": 2, "reports": [
            {"name": "r1.md", "size": 10, "at": t - 100},
            {"name": "r2.md", "size": 20, "at": t - 200}]}),
        "poolrad_progress" => json!({"credited": 4, "identities": 2, "ceiling": 10,
            "credit_pending": 0, "main": "abc", "queue_exported_at": "2026-11-05T11:00:00Z"}),
        "review_list" => {
            json!({"reviews": [{"age_s": 600}, {"created_at": t - 1200}, {"id": "x"}]})
        }
        _ => json!({}),
    };
    Reply::Answer(value)
}

fn claude_reading() -> Value {
    let t = now();
    json!({"provider": "claude", "status": "ok", "meaning": "percent_used",
        "measured_at": iso(t - ChronoDuration::seconds(60)),
        "windows": [{"name": "weekly", "percent_used": 40, "percent_remaining": 60,
                     "resets_at": iso(t + ChronoDuration::days(1))}],
        "extra_usage": {"used_credits": 1.5, "monthly_limit": 100, "utilization": 0.2,
                        "spend_limit_reached": false}})
}

fn codex_reading() -> Value {
    json!({"provider": "codex", "status": "ok", "meaning": "percent_used",
        "measured_at": iso(now() - ChronoDuration::seconds(60)),
        "last_snapshot": {"percent_used": 37.5, "window_minutes": 10080}})
}

fn load_standard(harness: &Harness) {
    harness.usage.set("claude", Ok(claude_reading()));
    harness.usage.set("codex", Ok(codex_reading()));
    harness.usage.set(
        "pi",
        Ok(
            json!({"provider": "pi", "status": "error", "meaning": "unknown",
            "measured_at": iso(now() - ChronoDuration::seconds(30)),
            "error": "HTTP 429 rate_limit"}),
        ),
    );
    harness.usage.set("opencode-go", Err("oc down".to_owned()));
}

#[test]
fn cold_snapshot_reads_every_panel_in_oracle_shape() {
    let h = harness(
        Arc::new(standard_answer),
        open_db,
        Duration::from_secs(3600),
    );
    seed_subscription(h.dir.path(), "claude", 2000);
    load_standard(&h);
    let board = h.runtime.snapshot(None).expect("cold snapshot");

    assert_eq!(
        keys(&board),
        [
            "generated_at",
            "interval_s",
            "no_model",
            "brief",
            "attention",
            "reference",
            "reports",
            "poolrad_progress",
            "budget"
        ]
    );
    assert_eq!(board["generated_at"], "2026-11-05T12:00:00Z");
    assert_eq!(board["interval_s"], 30);
    assert_eq!(board["no_model"], true);

    // Backlog: two open tasks, the urgent one is oldest-ageable at 7200 s.
    let backlog = &board["brief"]["backlog"];
    assert_eq!(backlog["open"], 2);
    assert_eq!(backlog["oldest_age_s"], 7200);
    assert_eq!(backlog["ages_unknown"], 0);
    assert_eq!(backlog["complete"], true);
    assert_eq!(backlog["by_priority"]["open"]["urgent"], 1);
    assert_eq!(backlog["by_priority"]["blocked"]["high"], 1);

    // Throughput: the partial current hour and the 24-hour sum, from the daemon's buckets.
    let throughput = &board["brief"]["throughput"];
    assert_eq!(throughput["completed_current_bucket"], 2);
    assert_eq!(throughput["completed_last_24h"], 5);
    assert_eq!(throughput["completed_in_window"], 7);
    assert_eq!(throughput["error"], Value::Null);

    // Review queue: the daemon's age wins, and a row with no age is counted as unknown.
    assert_eq!(board["brief"]["review"]["awaiting"], 3);
    assert_eq!(board["brief"]["review"]["oldest_age_s"], 1200);
    assert_eq!(board["brief"]["review"]["ages_unknown"], 1);

    // Lanes: a bare shell is its own count, and an unlisted registry lane is unknown.
    let lanes = &board["brief"]["lanes"];
    assert_eq!(lanes["working"], 1);
    assert_eq!(lanes["idle"], 1);
    assert_eq!(lanes["dead"], 1);
    assert_eq!(lanes["unknown"], 1);
    assert_eq!(lanes["bare_panes"], 1);

    // Ownership, the lane's question verbatim, dependents, and the 24-hour owner asks.
    let owner_items = board["attention"]["owner"]["items"].as_array().unwrap();
    assert_eq!(owner_items.len(), 1);
    assert_eq!(owner_items[0]["id"], "t2");
    assert_eq!(
        owner_items[0]["needs_owner_question"],
        "Ship the release or hold?"
    );
    let tasks = board["reference"]["tasks"]["items"].as_array().unwrap();
    assert_eq!(tasks[0]["id"], "t1", "urgent sorts first");
    assert_eq!(tasks[0]["blocks"], json!(["t2"]));
    assert_eq!(board["reference"]["asks"]["count"], 1);

    // Alarms carry the source's own error text and the daemon's problem text.
    let alarms = board["attention"]["alarms"].as_array().unwrap();
    assert!(alarms.iter().any(|a| a["text"] == "d1: pane gone"));
    assert!(alarms
        .iter()
        .any(|a| a["source"] == "host" && a["text"] == "tmp: warning"));
    assert!(alarms
        .iter()
        .any(|a| a["source"] == "health" && a["text"] == "disk low"));

    // Budget: the subscription price is spelled as Python spells it, and each provider's
    // state follows its reading.
    let budget = &board["budget"];
    assert_eq!(budget["state"], "ok");
    assert_eq!(budget["subscription_error"], Value::Null);
    let providers = budget["providers"].as_array().unwrap();
    let claude = &providers[0];
    assert_eq!(claude["provider"], "claude");
    assert_eq!(claude["subscription"]["monthly_usd"].to_string(), "20.0");
    assert_eq!(claude["windows"][0]["percent_remaining"], 60);
    let codex = &providers[1];
    assert_eq!(codex["windows"][0]["name"], "weekly");
    assert_eq!(codex["windows"][0]["percent_remaining"].to_string(), "62.5");
    let pi = &providers[2];
    assert_eq!(pi["reason"], "pi endpoint rate-limited (HTTP 429)");
    let opencode = &providers[3];
    assert_eq!(opencode["reason"], "oc down");
    assert_eq!(opencode["state"], "error");
}

#[test]
fn failed_sources_stay_unavailable_with_exception_names_and_others_render() {
    let handler: Handler = Arc::new(|command: &str, args: &Map<String, Value>| match command {
        "list_panes" => Reply::Refuse("boom"),
        "task_metrics" => Reply::Refuse("boom"),
        "health" => Reply::Garbage,
        other => standard_answer(other, args),
    });
    let h = harness(handler, open_db, Duration::from_secs(3600));
    load_standard(&h);
    let board = h.runtime.snapshot(None).expect("partial snapshot");

    // No fake zero: the lane tile is null and carries its source's error name.
    assert_eq!(board["brief"]["lanes"]["state"], "unavailable");
    assert_eq!(board["brief"]["lanes"]["working"], Value::Null);
    assert_eq!(board["reference"]["lanes"]["error"], "DunkError");
    // Throughput fails alone and does not take the backlog down with it.
    assert_eq!(board["brief"]["throughput"]["error"], "DunkError");
    assert_eq!(board["brief"]["throughput"]["state"], "ok");
    assert_eq!(board["brief"]["backlog"]["state"], "ok");
    assert_eq!(board["brief"]["backlog"]["open"], 2);
    // Every failed source raises an alarm with its own text.
    let alarms = board["attention"]["alarms"].as_array().unwrap();
    assert!(alarms
        .iter()
        .any(|a| a["source"] == "panes" && a["text"] == "DunkError"));
    assert!(alarms
        .iter()
        .any(|a| a["source"] == "health" && a["text"] == "ValueError"));
    // Schedules and the budget are untouched by the lane failure.
    assert_eq!(board["reference"]["schedules"]["state"], "ok");
    assert_eq!(board["budget"]["state"], "ok");
}

#[test]
fn a_full_task_page_is_reported_as_truncated_and_missing_ages_are_counted() {
    let handler: Handler = Arc::new(|command: &str, args: &Map<String, Value>| match command {
        "task_list" => {
            let t = now().timestamp();
            let mut tasks: Vec<Value> = (0..999)
                .map(|i| {
                    json!({"id": format!("p{i}"), "title": "page", "status": "open",
                        "priority": "medium", "created_at": t - 100 - i, "scope": "global"})
                })
                .collect();
            tasks.push(json!({"id": "nodate", "status": "blocked", "priority": "low"}));
            Reply::Answer(json!({"tasks": tasks}))
        }
        other => standard_answer(other, args),
    });
    let h = harness(handler, open_db, Duration::from_secs(3600));
    let board = h.runtime.snapshot(None).expect("snapshot");
    let backlog = &board["brief"]["backlog"];
    assert_eq!(backlog["open"], 1000);
    assert_eq!(
        backlog["complete"], false,
        "a full page is never a definitive count"
    );
    assert_eq!(
        backlog["ages_unknown"], 1,
        "a task with no timestamp is unknown, not zero"
    );
    assert_eq!(backlog["oldest_age_s"], 100 + 998);
}

#[test]
fn provider_cadence_backoff_and_last_good_follow_the_oracle() {
    let reader = Arc::new(Usage::default());
    reader.set(
        "claude",
        Ok(
            json!({"provider": "claude", "status": "ok", "meaning": "percent_used",
            "measured_at": 1000.0, "windows": [{"name": "weekly", "percent_used": 10,
            "percent_remaining": 90}]}),
        ),
    );
    reader.set("codex", Err("codex down".to_owned()));
    reader.set("pi", Err("pi down".to_owned()));
    reader.set("opencode-go", Err("oc down".to_owned()));
    let clock = Arc::new(Mutex::new(1000.0_f64));
    let source = Arc::clone(&clock);
    let cache = UsageCache::new(
        1800,
        vec![300, 600, 1800],
        reader.clone(),
        Arc::new(move || *source.lock().unwrap()),
        Dispatch::Inline,
    );
    let row = |rows: &[Value], name: &str| -> Value {
        rows.iter().find(|r| r["provider"] == name).unwrap().clone()
    };

    // First read: every provider is due, and each row carries its next attempt.
    let rows = cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 4);
    assert_eq!(row(&rows, "claude")["next_attempt_at"], 2800.0);
    assert_eq!(
        row(&rows, "codex")["next_attempt_at"],
        1300.0,
        "first failure backs off 300 s"
    );

    // Before any provider is due again, nothing is read.
    *clock.lock().unwrap() = 1100.0;
    reader.set("claude", Err("HTTP 500".to_owned()));
    cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 4, "no provider is due at 1100 s");

    // At 1300 s the failing providers are due; claude is not, so its good row stands.
    *clock.lock().unwrap() = 1300.0;
    let rows = cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 7);
    assert_eq!(
        row(&rows, "codex")["next_attempt_at"],
        1900.0,
        "second failure backs off 600 s"
    );
    assert_eq!(row(&rows, "claude")["status"], "ok");

    // At 2800 s claude is due and fails: its last good reading is kept beside the error.
    *clock.lock().unwrap() = 2800.0;
    let rows = cache.snapshot().unwrap();
    let claude = row(&rows, "claude");
    assert_eq!(claude["status"], "error");
    assert_eq!(claude["last_good"]["status"], "ok");
    assert_eq!(claude["next_attempt_at"], 3100.0);
}

#[test]
fn budget_projection_spells_prices_and_windows_like_python() {
    let now = DateTime::<Utc>::from_timestamp_micros(
        Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0)
            .unwrap()
            .timestamp_micros()
            + 123_456,
    )
    .unwrap();
    let subscriptions = vec![
        Subscription {
            provider: "claude".into(),
            monthly_usd_cents: 2050,
            observed_at: "2026-11-01T00:00:00Z".into(),
        },
        Subscription {
            provider: "pi".into(),
            monthly_usd_cents: 2000,
            observed_at: "2026-11-01T00:00:00Z".into(),
        },
    ];
    let readings = vec![
        json!({"provider": "codex", "status": "ok", "meaning": "percent_used",
            "measured_at": "2026-11-05T11:59:00Z",
            "last_snapshot": {"percent_used": 37, "window_minutes": 300, "name": "5h"}}),
        json!({"provider": "pi", "status": "ok", "meaning": "percent_used",
            "measured_at": "2026-11-05T11:59:00Z",
            "windows": [{"name": "old", "percent_used": 5, "percent_remaining": 95,
                         "resets_at": "2026-11-05T11:00:00Z"}]}),
    ];
    let budget = project(&readings, &subscriptions, now, None, 3600).unwrap();

    assert_eq!(
        budget["generated_at"], "2026-11-05T12:00:00.123456Z",
        "microseconds show"
    );
    assert_eq!(budget["state"], "ok");
    assert_eq!(budget["dollars_remaining"], Value::Null);

    let providers = budget["providers"].as_array().unwrap();
    // Readings keep their order; a subscription with no reading comes last.
    assert_eq!(providers[0]["provider"], "codex");
    assert_eq!(providers[0]["state"], "ok");
    assert_eq!(providers[0]["windows"][0]["name"], "5h");
    assert_eq!(
        providers[0]["windows"][0]["percent_remaining"].to_string(),
        "63"
    );
    assert_eq!(providers[1]["provider"], "pi");
    assert_eq!(
        providers[1]["windows"][0]["state"], "expired",
        "a past reset is expired"
    );
    assert_eq!(
        providers[1]["subscription"]["monthly_usd"].to_string(),
        "20.0"
    );
    assert_eq!(providers[2]["provider"], "claude");
    assert_eq!(providers[2]["state"], "unavailable");
    assert_eq!(
        providers[2]["subscription"]["monthly_usd"].to_string(),
        "20.5"
    );
    assert_eq!(providers[2]["reason"], "no provider reading");

    // A cache-level failure is reported on the budget and degrades every provider row.
    let failed = project(&[], &subscriptions, now, Some("usage unavailable"), 3600).unwrap();
    assert_eq!(failed["state"], "unavailable");
    assert_eq!(failed["providers"][0]["reason"], "usage unavailable");
}

static FAIL_STALE: AtomicBool = AtomicBool::new(false);

fn open_switchable_for_stale(path: &Path) -> rusqlite::Result<Connection> {
    if FAIL_STALE.load(Ordering::SeqCst) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    open_db(path)
}

#[test]
fn stale_snapshot_keeps_the_last_good_board_with_refresh_error_and_age() {
    let h = harness(
        Arc::new(standard_answer),
        open_switchable_for_stale,
        Duration::from_secs(3600),
    );
    load_standard(&h);
    let fresh = h.runtime.snapshot(None).expect("first snapshot");
    assert_eq!(fresh["generated_at"], "2026-11-05T12:00:00Z");
    assert!(fresh.get("refresh_error").is_none());

    // Ten seconds on the 30-second cadence is inside three cadences: no refresh is tried.
    h.clock.set(now() + ChronoDuration::seconds(10));
    FAIL_STALE.store(true, Ordering::SeqCst);
    let young = h.runtime.snapshot(None).expect("cached snapshot");
    assert!(young.get("refresh_error").is_none());

    // Past three cadences the refresh is tried, fails, and the old board is served with the
    // exception name. Its generated_at still says when it was read.
    h.clock.set(now() + ChronoDuration::seconds(100));
    let stale = h.runtime.snapshot(None).expect("stale snapshot is served");
    assert_eq!(stale["refresh_error"], "OperationalError");
    assert_eq!(stale["generated_at"], "2026-11-05T12:00:00Z");
    assert_eq!(
        stale["brief"], fresh["brief"],
        "the last good data is not rewritten"
    );

    let explicit = h
        .runtime
        .refresh()
        .expect("a failed refresh keeps the snapshot");
    assert_eq!(explicit["refresh_error"], "OperationalError");

    // Recovery clears the marker and stamps the new read time.
    FAIL_STALE.store(false, Ordering::SeqCst);
    h.clock.set(now() + ChronoDuration::seconds(110));
    let recovered = h.runtime.refresh().expect("refresh after recovery");
    assert!(recovered.get("refresh_error").is_none());
    assert_eq!(recovered["generated_at"], "2026-11-05T12:01:50Z");
}

static FAIL_COLD: AtomicBool = AtomicBool::new(false);

fn open_switchable_for_cold(path: &Path) -> rusqlite::Result<Connection> {
    if FAIL_COLD.load(Ordering::SeqCst) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    open_db(path)
}

#[test]
fn a_cold_failure_is_an_error_and_a_later_read_recovers() {
    FAIL_COLD.store(true, Ordering::SeqCst);
    let h = harness(
        Arc::new(standard_answer),
        open_switchable_for_cold,
        Duration::from_secs(3600),
    );
    assert_eq!(h.runtime.snapshot(None).unwrap_err(), "OperationalError");
    assert_eq!(h.runtime.refresh().unwrap_err(), "OperationalError");

    FAIL_COLD.store(false, Ordering::SeqCst);
    let board = h.runtime.snapshot(None).expect("recovered cold read");
    assert!(board.get("refresh_error").is_none());
}

#[test]
fn the_timer_refreshes_on_its_period_and_stop_joins_before_returning() {
    let h = harness(
        Arc::new(standard_answer),
        open_db,
        Duration::from_millis(20),
    );
    h.runtime.start_timer().expect("start the board timer");
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.daemon.count("health") < 2 {
        assert!(Instant::now() < deadline, "the timer never refreshed");
        thread::sleep(Duration::from_millis(5));
    }
    h.runtime.stop_timer();
    let at_stop = h.daemon.count("health");
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        h.daemon.count("health"),
        at_stop,
        "no refresh runs after stop returns"
    );
}

#[test]
fn restarting_after_stop_resumes_the_timer() {
    let h = harness(
        Arc::new(standard_answer),
        open_db,
        Duration::from_millis(20),
    );
    h.runtime.start_timer().unwrap();
    h.runtime.stop_timer();
    let stopped_at = h.daemon.count("health");

    h.runtime.start_timer().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.daemon.count("health") <= stopped_at {
        assert!(
            Instant::now() < deadline,
            "the restarted timer never refreshed"
        );
        thread::sleep(Duration::from_millis(5));
    }
    h.runtime.stop();
}

struct SlowUsage {
    finished: Arc<AtomicBool>,
}

impl UsageReader for SlowUsage {
    fn read(&self, _provider: &str) -> Result<Value, String> {
        thread::sleep(Duration::from_millis(150));
        self.finished.store(true, Ordering::SeqCst);
        Err("slow read finished".to_owned())
    }
}

#[test]
fn shutdown_waits_for_the_usage_workers_it_started() {
    let finished = Arc::new(AtomicBool::new(false));
    let cache = UsageCache::new(
        1800,
        vec![300],
        Arc::new(SlowUsage {
            finished: Arc::clone(&finished),
        }),
        Arc::new(|| 1000.0),
        Dispatch::Threads,
    );
    let rows = cache.snapshot().unwrap();
    assert_eq!(rows.len(), PROVIDERS.len());
    cache.stop();
    assert!(
        finished.load(Ordering::SeqCst),
        "stop returns only after the provider workers have joined"
    );
}

struct AcceptAll;

impl AuthBackend for AcceptAll {
    fn login<'a>(&'a self, _: &'a str, _: &'a str) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unused".into())) })
    }
    fn identity<'a>(
        &'a self,
        _: Option<&'a str>,
        _: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Ok(Identity::Owner) })
    }
    fn logout<'a>(&'a self, _: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unused".into())) })
    }
    fn change_password<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unused".into())) })
    }
}

static FAIL_ROUTES: AtomicBool = AtomicBool::new(false);

fn open_switchable_for_routes(path: &Path) -> rusqlite::Result<Connection> {
    if FAIL_ROUTES.load(Ordering::SeqCst) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    open_db(path)
}

fn router_for(runtime: &BoardRuntime) -> Router {
    let auth: Arc<dyn AuthBackend> = Arc::new(AcceptAll);
    let metrics: Arc<dyn OpsMetricsBackend> = Arc::new(runtime.clone());
    let board: Arc<dyn OpsBoardBackend> = Arc::new(owner_board_backend(runtime));
    Router::new()
        .merge(
            ops_metrics::router_with_scheme(RequestScheme::Http)
                .layer(Extension(metrics))
                .layer(Extension(Arc::clone(&auth))),
        )
        .merge(ops_board::router(board, auth, RequestScheme::Http))
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(text) => {
            builder = builder.header("content-type", "application/json");
            Body::from(text.to_owned())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .expect("route responds");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[test]
fn routes_serve_cold_refresh_stale_failure_and_restart_states() {
    let runtime_async = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let h = harness(
        Arc::new(standard_answer),
        open_switchable_for_routes,
        Duration::from_secs(3600),
    );
    load_standard(&h);
    runtime_async.block_on(async {
        let app = router_for(&h.runtime);

        // Cold read through the real route.
        let (status, board) = send(&app, "GET", "/api/ops-board", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(board["no_model"], true);

        // Explicit refresh, and a deferral through the shared daemon client.
        let (status, _) = send(&app, "POST", "/api/ops-board/refresh", None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, deferred) = send(
            &app,
            "POST",
            "/api/ops-board/not-now",
            Some(r#"{"task_id":"t2"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deferred["priority"], "low");
        assert_eq!(h.daemon.count("task_update"), 1);

        // The history reader shares the operations database.
        let (status, history) = send(&app, "GET", "/api/ops-metrics/history?hours=24", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(history["window_hours"], 24);

        // The database fails and the snapshot is old: the route serves the last good board
        // with its refresh error, and the refresh endpoint does too.
        FAIL_ROUTES.store(true, Ordering::SeqCst);
        h.clock.set(now() + ChronoDuration::seconds(100));
        let (status, stale) = send(&app, "GET", "/api/ops-board", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stale["refresh_error"], "OperationalError");
        let (status, stale) = send(&app, "POST", "/api/ops-board/refresh", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stale["refresh_error"], "OperationalError");

        // A restarted process starts cold. A failing database is a 503 with the exception.
        let restarted = BoardRuntime::new(
            config(h.dir.path(), Duration::from_secs(3600)),
            shared_client(&h.daemon.socket),
            open_switchable_for_routes,
            Arc::new(TestClock(Mutex::new(*h.clock.0.lock().unwrap()))),
            h.usage.clone(),
            Dispatch::Inline,
        );
        let cold_app = router_for(&restarted);
        let (status, body) = send(&cold_app, "GET", "/api/ops-board", None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["detail"],
            "Operations board unavailable: OperationalError"
        );

        // Once the database is back, the restarted process serves a fresh board.
        FAIL_ROUTES.store(false, Ordering::SeqCst);
        let (status, board) = send(&cold_app, "GET", "/api/ops-board", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(board.get("refresh_error").is_none());
    });
}
