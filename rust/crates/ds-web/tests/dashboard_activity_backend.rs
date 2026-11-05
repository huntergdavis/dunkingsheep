//! Integration tests for `dashboard_activity_backend` (t3213).
//!
//! `src/lib.rs` is not edited by this batch, so the module is compiled here from its file. The
//! library's `routes` and `roster` modules are re-exported under the same names so the module's
//! `crate::` paths resolve here exactly as they will once startup integration declares it.
//!
//! Oracle values were captured by running the real Python modules (`dashboard.py`,
//! `ops_metrics.py`, `poolrad_paths.py`, `web/roster.py`) and the real `web/app.py` route bodies
//! (decorators stripped, fastapi not imported). Only the daemon client and HTTPException were
//! stubbed on the Python side. Pane text and checkpoint commits are compared byte for byte.

pub(crate) use ds_web::{roster, routes};

#[allow(dead_code)]
#[path = "../src/dashboard_activity_backend.rs"]
mod dashboard_activity_backend;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use dashboard_activity_backend::{
    pacific_iso, percent, py_splitlines, trim_pane_text, ActivityPoolradBackend, Clock,
    DashBackend, DashboardPaths, PoolradDashboardBackend, SystemClock,
};
use ds_core::client::{FlockClient, UnixTransport};
use ds_web::auth_backend::PersistentAuth;
use ds_web::routes::activity_poolrad::{
    router as activity_router, ActivityPoolradBackend as ActivityRoutes, ActivityPoolradError,
};
use ds_web::routes::auth::AuthBackend;
use ds_web::routes::dash::{router as dash_router, DashBackend as DashRoutes};
use ds_web::routes::poolrad_dashboard::{
    router as dashboard_router, PoolradDashboardBackend as DashboardRoutes,
};
use ds_web::RequestScheme;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";

/// Lines of a 58-line pane text: 55 numbered rows, then `last`, `end`, `final`. The mixed
/// separators (CRLF, U+2028, VT) are the ones Python `splitlines()` treats as boundaries.
fn long_pane_text() -> String {
    let mut text: String = (0..55).map(|row| format!("row {row}\n")).collect();
    text.pop();
    text.push_str("\r\nlast\u{2028}end\u{b}final\n");
    text
}

fn pane_fixtures() -> Value {
    json!([
        {"pane_id": "w1:p1", "agent": "claude", "agent_status": "working", "cwd": "/home/x/a", "label": "alpha"},
        {"pane_id": "w1:p2", "agent": "pi", "agent_status": "blocked", "cwd": "/home/x/b", "label": "beta"},
        {"pane_id": "%9", "agent": "", "cwd": "/home/x/c"}
    ])
}

/// The daemon's answers for the panes and channel scenarios (the same script the oracle used).
fn scripted_reply(cmd: &str, args: &Value) -> Result<Value, String> {
    match cmd {
        "list_panes" => Ok(json!({"panes": pane_fixtures()})),
        "read_pane" => match args["target"].as_str() {
            Some("w1:p1") => Ok(json!({"agent_status": "idle", "text": long_pane_text()})),
            Some("w1:p2") => Err("socket reset".into()),
            Some("%9") => Ok(json!({"text": "solo\n"})),
            _ => Err("no such pane".into()),
        },
        "delivery_capability" => match args["target"].as_str() {
            Some("%9") => Ok(json!({"agent_alive": true})),
            _ => Err("no capability".into()),
        },
        other => Err(format!("unexpected command {other}")),
    }
}

type RequestLog = Arc<StdMutex<Vec<(String, Value)>>>;

/// A real Unix socket that answers one `\n`-terminated JSON request per connection.
fn spawn_daemon<F>(socket: &Path, handler: F) -> RequestLog
where
    F: Fn(&str, &Value) -> Result<Value, String> + Send + 'static,
{
    let listener = UnixListener::bind(socket).expect("bind scripted daemon socket");
    let log: RequestLog = Arc::new(StdMutex::new(Vec::new()));
    let recorded = Arc::clone(&log);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(reader_side) = stream.try_clone() else {
                continue;
            };
            let mut line = String::new();
            if BufReader::new(reader_side).read_line(&mut line).is_err() {
                continue;
            }
            let request: Value = serde_json::from_str(line.trim_end()).expect("request json");
            let cmd = request["cmd"].as_str().unwrap_or_default().to_string();
            let args = request["args"].clone();
            recorded.lock().unwrap().push((cmd.clone(), args.clone()));
            let reply = match handler(&cmd, &args) {
                Ok(result) => json!({"id": request["id"], "ok": true, "result": result}),
                Err(error) => json!({"id": request["id"], "ok": false, "error": error}),
            };
            let _ = writeln!(stream, "{reply}");
        }
    });
    log
}

fn daemon_client(socket: &Path) -> FlockClient<UnixTransport> {
    FlockClient::new(
        UnixTransport {
            path: socket.to_path_buf(),
        },
        None,
        "dashboard-activity-test".into(),
        Duration::from_secs(5),
        Some("test-service-token".into()),
    )
}

/// Manual time: monotonic seconds move only when a test moves them.
struct ManualClock {
    monotonic: StdMutex<f64>,
    unix: f64,
}

impl Clock for ManualClock {
    fn monotonic(&self) -> f64 {
        *self.monotonic.lock().unwrap()
    }
    fn unix_seconds(&self) -> f64 {
        self.unix
    }
}

fn paths_in(dir: &Path) -> DashboardPaths {
    let dashboard_dir = dir.join("dashboard");
    fs::create_dir_all(dashboard_dir.join("state").join("reports")).unwrap();
    DashboardPaths {
        ui_dist: dir.join("ui-dist"),
        dashboard_dir,
        dashboard_repo: dir.join("poolrad"),
        operations_db: dir.join("operations.sqlite3"),
    }
}

fn activity_backend(
    socket: &Path,
    paths: DashboardPaths,
    clock: Arc<dyn Clock>,
) -> ActivityPoolradBackend<UnixTransport> {
    ActivityPoolradBackend::new(daemon_client(socket), Vec::new(), paths, clock)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// A real persistent owner session: the auth store is created with a known password and logged in.
fn owner_session(data: &Path) -> (Arc<dyn AuthBackend>, String) {
    let auth = PersistentAuth::open_with_password(data, Some(PASSWORD)).expect("auth store");
    let token = runtime()
        .block_on(auth.login("127.0.0.1", PASSWORD))
        .expect("owner login");
    (Arc::new(auth), token)
}

fn send(
    app: &Router,
    method: &str,
    uri: &str,
    session: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "desk.test");
    if let Some(token) = session {
        builder = builder.header(header::COOKIE, format!("vp_session={token}"));
    }
    let request = builder.body(Body::empty()).unwrap();
    let rt = runtime();
    let response = rt.block_on(app.clone().oneshot(request)).unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = rt
        .block_on(to_bytes(response.into_body(), usize::MAX))
        .unwrap();
    (status, headers, body.to_vec())
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "oracle")
        .env("GIT_AUTHOR_EMAIL", "oracle@example.invalid")
        .env("GIT_COMMITTER_NAME", "oracle")
        .env("GIT_COMMITTER_EMAIL", "oracle@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-11-05T10:00:00-08:00")
        .env("GIT_COMMITTER_DATE", "2026-11-05T10:00:00-08:00")
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A poolrad checkout with the two published reports and one replacement record, committed.
fn poolrad_checkout(repo: &Path, replaced: &str) -> String {
    fs::create_dir_all(repo.join("docs/re")).unwrap();
    fs::write(
        repo.join("docs/re/SLOT_A_FIRST_EGA_ROW64.md"),
        "# EGA row 64\n\n**320 indexed pixels match all 320** at the saved return.\n",
    )
    .unwrap();
    fs::write(
        repo.join("docs/re/SLOT_A_ROW6_COL38_PRINTER_PAGE.md"),
        "# Printer page\n\n**28,160/28,160** pixel indices equal in the crop.\n",
    )
    .unwrap();
    fs::write(repo.join("docs/re/replaced_bodies.json"), replaced).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "fixture"]);
    git(repo, &["rev-parse", "HEAD"])
}

const GOOD_REPLACEMENTS: &str = r#"{
 "schema": "replaced/1",
 "truth": "proven",
 "bodies": [
  {"id": "OVL_0001", "proven_at": "2026-10-01T00:00:00Z", "evidence": "e1"},
  {"id": "OVL_0002", "proven_at": "2026-10-02T00:00:00Z", "evidence": "e2"}
 ]
}"#;

const DUPLICATE_REPLACEMENTS: &str = r#"{"schema": "replaced/1", "bodies": [
  {"id": "A", "proven_at": "x", "evidence": "e"}, {"id": "A", "proven_at": "x", "evidence": "e"}]}"#;

const ACCOUNTING_TABLE: &str = "CREATE TABLE accounting_snapshots (id INTEGER PRIMARY KEY, \
    project TEXT NOT NULL, observed_at TEXT NOT NULL, commit_oid TEXT NOT NULL, \
    strict_full_c INTEGER NOT NULL, verified_game_identities INTEGER NOT NULL, \
    ceiling INTEGER NOT NULL, bound_kind TEXT NOT NULL CHECK(bound_kind IN ('exact','upper_bound')), \
    source_signature TEXT NOT NULL, evidence_ref TEXT NOT NULL, UNIQUE(project,observed_at,commit_oid));";

/// Adds one `poolrad` checkpoint row (the reviewed 1200 strict / 1300 identities / 3000 ceiling).
fn record_checkpoint(db: &Path, observed_at: &str, commit: &str) {
    let connection = rusqlite::Connection::open(db).unwrap();
    connection.execute_batch(ACCOUNTING_TABLE).ok();
    connection
        .execute(
            "INSERT INTO accounting_snapshots(project,observed_at,commit_oid,strict_full_c,\
             verified_game_identities,ceiling,bound_kind,source_signature,evidence_ref) \
             VALUES('poolrad',?1,?2,1200,1300,3000,'upper_bound','sig',\
             'docs/re/function_ceiling_census.json')",
            rusqlite::params![observed_at, commit],
        )
        .unwrap();
}

fn is_pacific_stamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.len() == 25
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && (text.ends_with("-07:00") || text.ends_with("-08:00"))
}

fn is_utc_stamp(text: &str) -> bool {
    text.len() == 20 && text.ends_with('Z') && text.as_bytes()[10] == b'T'
}

// ---------------------------------------------------------------------------
// Pure helpers: Python oracle comparisons.
// ---------------------------------------------------------------------------

#[test]
fn percent_matches_python_oracle() {
    let cases = [
        ((0, 3000, "exact"), 0.0),
        ((1, 3, "exact"), 33.3),
        ((2, 3, "exact"), 66.7),
        ((1, 3, "upper_bound"), 33.3),
        ((1200, 3000, "upper_bound"), 40.0),
        ((1300, 3000, "exact"), 43.3),
        ((5, 0, "exact"), 0.0),
        ((1, 80, "exact"), 1.2),
        ((3, 400, "exact"), 0.8),
        ((7, 8, "exact"), 87.5),
        ((1, 800, "exact"), 0.1),
        ((2999, 3000, "upper_bound"), 99.9),
        ((-1, 3000, "upper_bound"), -0.1),
        ((-3, 8, "exact"), -37.5),
    ];
    for ((count, ceiling, bound), expected) in cases {
        assert_eq!(
            percent(count, ceiling, bound),
            expected,
            "{count}/{ceiling} {bound}"
        );
    }
}

#[test]
fn pacific_stamps_match_python_zoneinfo_across_dst_edges() {
    let cases = [
        (1772963999, "2026-03-08T01:59:59-08:00"),
        (1772964000, "2026-03-08T03:00:00-07:00"),
        (1793523599, "2026-11-01T01:59:59-07:00"),
        (1793523600, "2026-11-01T01:00:00-08:00"),
        (1793527199, "2026-11-01T01:59:59-08:00"),
        (1783193400, "2026-07-04T12:30:00-07:00"),
        (1768478400, "2026-01-15T04:00:00-08:00"),
        (1772366400, "2026-03-01T04:00:00-08:00"),
        (1798761599, "2026-12-31T15:59:59-08:00"),
    ];
    for (epoch, expected) in cases {
        assert_eq!(pacific_iso(epoch), expected, "epoch {epoch}");
    }
}

#[test]
fn splitlines_matches_python_boundaries() {
    let cases: [(&str, Vec<&str>); 11] = [
        ("", vec![]),
        ("a", vec!["a"]),
        ("a\n", vec!["a"]),
        ("a\r\nb", vec!["a", "b"]),
        ("a\rb\nc", vec!["a", "b", "c"]),
        ("x\n\n", vec!["x", ""]),
        (
            "a\u{b}b\u{c}c\u{1c}d\u{1d}e\u{1e}f",
            vec!["a", "b", "c", "d", "e", "f"],
        ),
        ("a\u{85}b\u{2028}c\u{2029}d", vec!["a", "b", "c", "d"]),
        ("\n", vec![""]),
        ("\r\n", vec![""]),
        ("tail\r", vec!["tail"]),
    ];
    for (text, expected) in cases {
        assert_eq!(py_splitlines(text), expected, "{text:?}");
    }
}

#[test]
fn pane_text_keeps_the_last_forty_lines_and_32000_characters() {
    let trimmed = trim_pane_text(&long_pane_text());
    let expected: String = (18..55)
        .map(|row| format!("row {row}"))
        .chain(["last", "end", "final"].map(String::from))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(trimmed, expected);

    let long_line = format!("{}END", "x".repeat(40_000));
    let bounded = trim_pane_text(&long_line);
    assert_eq!(bounded.chars().count(), 32_000);
    assert!(bounded.ends_with("END"));
}

// ---------------------------------------------------------------------------
// Pane activity and channel activity over a scripted daemon socket.
// ---------------------------------------------------------------------------

fn expected_panes_body() -> Value {
    let trimmed_w1p1: String = (18..55)
        .map(|row| format!("row {row}"))
        .chain(["last", "end", "final"].map(String::from))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "panes": [
            {
                "activity_group": "active", "agent": "claude", "can_message": true,
                "dashboard": false, "error": null, "id": "pane-w1-p1", "kind": "discovered",
                "name": "claude", "online": true, "pane_id": "w1:p1", "status": "idle",
                "tab_label": null, "text": trimmed_w1p1, "workspace": null
            },
            {
                "activity_group": "active", "agent": "pi", "can_message": true,
                "dashboard": false, "error": "Pane output unavailable", "id": "pane-w1-p2",
                "kind": "discovered", "name": "pi", "online": true, "pane_id": "w1:p2",
                "status": "waiting", "tab_label": null, "text": "", "workspace": null
            },
            {
                "activity_group": "standby", "agent": "", "can_message": false,
                "dashboard": false, "error": null, "id": "pane-9", "kind": "discovered",
                "name": "%9", "online": true, "pane_id": "%9", "status": "unknown",
                "tab_label": null, "text": "solo", "workspace": null
            }
        ],
        "refresh_after_seconds": 4
    })
}

#[test]
fn panes_activity_matches_python_oracle_and_refreshes_once_per_ttl() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("daemon.sock");
    let log = spawn_daemon(&socket, scripted_reply);
    let backend = activity_backend(&socket, paths_in(dir.path()), Arc::new(SystemClock::new()));

    let mut body = backend.panes_activity().unwrap().expect("panes snapshot");
    let checked_at = body["checked_at"].as_str().unwrap().to_string();
    assert!(is_utc_stamp(&checked_at), "checked_at {checked_at}");
    body.as_object_mut().unwrap().remove("checked_at");
    assert_eq!(body, expected_panes_body());

    let calls = log.lock().unwrap().clone();
    let commands: Vec<&str> = calls.iter().map(|(cmd, _)| cmd.as_str()).collect();
    assert_eq!(
        commands,
        ["list_panes", "read_pane", "read_pane", "read_pane"]
    );
    assert!(calls[1..]
        .iter()
        .all(|(_, args)| args["lines"] == json!(40)));

    // Inside the 4 s TTL a second request must not contact the daemon again.
    backend.panes_activity().unwrap();
    assert_eq!(log.lock().unwrap().len(), calls.len());
}

#[test]
fn channel_activity_matches_python_oracle_and_channel_errors() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("daemon.sock");
    spawn_daemon(&socket, scripted_reply);
    let backend = activity_backend(&socket, paths_in(dir.path()), Arc::new(SystemClock::new()));

    let lane = backend
        .activity("pane-w1-p1")
        .unwrap()
        .expect("lane activity");
    assert_eq!(lane["channel"], "pane-w1-p1");
    assert_eq!(lane["pane_id"], "w1:p1");
    assert_eq!(lane["status"], "idle");
    assert_eq!(
        lane["text"],
        json!(long_pane_text()),
        "channel text is not trimmed"
    );

    let agent_alive = backend
        .activity("pane-9")
        .unwrap()
        .expect("agent_alive activity");
    assert_eq!(
        agent_alive,
        json!({"channel": "pane-9", "pane_id": "%9", "status": "unknown", "text": "solo\n"})
    );

    assert_eq!(backend.activity("pane-missing"), Ok(None));
    assert_eq!(
        backend.activity("nope"),
        Err(ActivityPoolradError::Http {
            status: StatusCode::NOT_FOUND,
            detail: "Unknown channel".into(),
        })
    );
}

#[test]
fn absent_daemon_is_unavailable_not_invented() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("no-daemon.sock");
    let backend = activity_backend(&socket, paths_in(dir.path()), Arc::new(SystemClock::new()));
    assert_eq!(
        backend.activity("pane-w1-p1"),
        Err(ActivityPoolradError::Http {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Dunking Sheep unavailable".into(),
        })
    );
    assert_eq!(backend.panes_activity(), Ok(None));
}

// ---------------------------------------------------------------------------
// Poolrad progress: published checkpoint in SQLite, real checkout, real git.
// ---------------------------------------------------------------------------

#[test]
fn progress_matches_python_oracle_for_the_published_checkpoint() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(
        paths.dashboard_dir.join("dashboard.py"),
        "# present; never executed here\n",
    )
    .unwrap();
    let sha = poolrad_checkout(&paths.dashboard_repo, GOOD_REPLACEMENTS);
    record_checkpoint(&paths.operations_db, "2026-11-05T09:30:00-08:00", &sha);

    let backend = activity_backend(
        &dir.path().join("unused.sock"),
        paths.clone(),
        Arc::new(SystemClock::new()),
    );
    let mut view = backend.poolrad_progress().unwrap().expect("progress view");

    let generated = view["generated"].as_str().unwrap().to_string();
    assert!(is_pacific_stamp(&generated), "generated {generated}");
    assert!(view["replacements"]["observed_at"].as_str().is_some());
    view.as_object_mut().unwrap().remove("generated");
    view["replacements"]
        .as_object_mut()
        .unwrap()
        .remove("observed_at");

    let short = sha[..8].to_string();
    let expected = json!({
        "accounting_source": "docs/re/function_accounting_checkpoint.json",
        "bound_kind": "upper_bound",
        "ceiling": 3000,
        "commit": sha.clone(),
        "comparisons": [
            {
                "compared": 320, "label": "EGA first-view row 64", "matched": 320,
                "method": "published scoped report; not rerun by dashboard",
                "scope": "one 320-pixel row at the saved first-view return",
                "source": "docs/re/SLOT_A_FIRST_EGA_ROW64.md", "status": "reported in source"
            },
            {
                "compared": 28160, "label": "Printer page, row 6 column 38", "matched": 28160,
                "method": "published scoped report; not rerun by dashboard",
                "scope": "one reached printer call; 320×88 diagnostic crop",
                "source": "docs/re/SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
                "status": "reported in source"
            }
        ],
        "evidence_doc": "docs/re/function_ceiling_census.json",
        "evidence_line": format!("PM-reviewed checkpoint {short} · 1200 strict / 1300 identities · ceiling 3000 (upper bound) · 2 published scoped comparison(s) (320/320 px, 28160/28160 px) · not whole-game completion"),
        "identities": 1300,
        "identity_percent": 43.3,
        "observed_at": "2026-11-05T09:30:00-08:00",
        "refresh_after_seconds": 60,
        "remaining": 1800,
        "replacements": {
            "commit": sha.clone(), "count": 2, "source": "docs/re/replaced_bodies.json",
            "state": "ok", "truth": "proven"
        },
        "source": "published main",
        "stale": false,
        "status": "reviewed",
        "strict_full_c": 1200,
        "strict_percent": 40.0
    });
    assert_eq!(view, expected);
}

#[test]
fn progress_without_a_published_row_is_unavailable_never_zero() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    poolrad_checkout(&paths.dashboard_repo, GOOD_REPLACEMENTS);
    let backend = activity_backend(
        &dir.path().join("unused.sock"),
        paths,
        Arc::new(SystemClock::new()),
    );
    // No database file at all: the Python fallback (dashboard.py progress) is not ported.
    assert_eq!(
        backend.poolrad_progress(),
        Err(ActivityPoolradError::Failure)
    );
}

#[test]
fn progress_reports_duplicate_replacement_ids_as_unavailable() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    let sha = poolrad_checkout(&paths.dashboard_repo, DUPLICATE_REPLACEMENTS);
    record_checkpoint(&paths.operations_db, "2026-11-05T09:30:00-08:00", &sha);
    let backend = activity_backend(
        &dir.path().join("unused.sock"),
        paths,
        Arc::new(SystemClock::new()),
    );
    let view = backend.poolrad_progress().unwrap().expect("view");
    assert_eq!(view["replacements"]["state"], "unavailable");
    assert_eq!(view["replacements"]["count"], Value::Null);
    assert!(view["replacements"].get("truth").is_none());
}

#[test]
fn progress_cache_holds_for_sixty_seconds_then_rereads_the_checkpoint() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    let sha = poolrad_checkout(&paths.dashboard_repo, GOOD_REPLACEMENTS);
    record_checkpoint(&paths.operations_db, "2026-11-05T09:30:00-08:00", &sha);
    let clock = Arc::new(ManualClock {
        monotonic: StdMutex::new(1000.0),
        unix: 1_793_520_000.0,
    });
    let backend = activity_backend(
        &dir.path().join("unused.sock"),
        paths.clone(),
        clock.clone(),
    );
    let first = backend.poolrad_progress().unwrap().unwrap();
    assert_eq!(first["observed_at"], "2026-11-05T09:30:00-08:00");

    record_checkpoint(&paths.operations_db, "2026-11-05T10:00:00-08:00", &sha);
    *clock.monotonic.lock().unwrap() = 1030.0;
    let cached = backend.poolrad_progress().unwrap().unwrap();
    assert_eq!(
        cached["observed_at"], "2026-11-05T09:30:00-08:00",
        "inside the TTL"
    );

    *clock.monotonic.lock().unwrap() = 1061.0;
    let refreshed = backend.poolrad_progress().unwrap().unwrap();
    assert_eq!(
        refreshed["observed_at"], "2026-11-05T10:00:00-08:00",
        "after the TTL"
    );
    assert_eq!(refreshed["refresh_after_seconds"], json!(60));
}

// ---------------------------------------------------------------------------
// Static dashboard page, reports, refresh, and the desk bundle.
// ---------------------------------------------------------------------------

#[test]
fn dashboard_page_reports_and_refresh_contracts() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    let page = b"<!doctype html><title>poolrad</title>".to_vec();
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    fs::write(paths.dashboard_dir.join("dashboard.html"), &page).unwrap();
    let report = b"# EGA row 64\n".to_vec();
    fs::write(
        paths
            .dashboard_dir
            .join("state/reports/SLOT_A_FIRST_EGA_ROW64.md"),
        &report,
    )
    .unwrap();
    let backend = PoolradDashboardBackend::new(paths.clone());

    assert!(backend.dashboard_available());
    assert_eq!(backend.dashboard_html(), Some(page));
    assert_eq!(backend.report("SLOT_A_FIRST_EGA_ROW64.md"), Some(report));
    assert_eq!(
        backend.report("SLOT_A_ROW6_COL38_PRINTER_PAGE.md"),
        None,
        "listed but absent"
    );
    assert_eq!(
        backend.report("../dashboard.py"),
        None,
        "outside the allowlist"
    );
    // Refresh needs the Python renderer and a network read, so it refuses and writes nothing.
    assert!(backend.refresh().is_err());
    assert!(
        !paths.operations_db.exists(),
        "refresh must not create the operations database"
    );
}

#[test]
fn dashboard_page_is_absent_when_the_checkout_is_not_built() {
    let dir = TempDir::new().unwrap();
    let backend = PoolradDashboardBackend::new(paths_in(dir.path()));
    assert!(!backend.dashboard_available());
    assert_eq!(backend.dashboard_html(), None);
    assert_eq!(backend.report("SLOT_A_FIRST_EGA_ROW64.md"), None);
}

#[test]
fn desk_bundle_is_served_only_when_built() {
    let dir = TempDir::new().unwrap();
    let ui = dir.path().join("ui-dist");
    let missing = DashBackend::new(ui.clone());
    assert_eq!(missing.index_html(), None);
    fs::create_dir_all(&ui).unwrap();
    fs::write(ui.join("index.html"), b"<html>desk</html>").unwrap();
    assert_eq!(
        DashBackend::new(ui).index_html(),
        Some(b"<html>desk</html>".to_vec())
    );
}

// ---------------------------------------------------------------------------
// Route requests with the real persistent session.
// ---------------------------------------------------------------------------

#[test]
fn activity_routes_answer_success_unknown_and_error_over_http() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("daemon.sock");
    spawn_daemon(&socket, scripted_reply);
    let (auth, session) = owner_session(&dir.path().join("desk"));
    let backend = Arc::new(activity_backend(
        &socket,
        paths_in(dir.path()),
        Arc::new(SystemClock::new()),
    ));
    let app = activity_router(
        backend.clone() as Arc<dyn ActivityRoutes>,
        auth.clone(),
        RequestScheme::Http,
    );

    let (status, headers, body) = send(&app, "GET", "/api/panes/activity", Some(&session));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(json_of(&body)["refresh_after_seconds"], json!(4));

    let (status, _, body) = send(
        &app,
        "GET",
        "/api/activity?channel=pane-w1-p1",
        Some(&session),
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body)["pane_id"], "w1:p1");

    let (status, _, body) = send(&app, "GET", "/api/activity?channel=nope", Some(&session));
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&body), json!({"detail": "Unknown channel"}));

    let (status, _, body) = send(
        &app,
        "GET",
        "/api/activity?channel=pane-missing",
        Some(&session),
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&body), json!({"detail": "Pane unavailable"}));

    let (status, _, _) = send(&app, "GET", "/api/activity?channel=pane-w1-p1", None);
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "signed-out callers get nothing"
    );
}

#[test]
fn progress_route_serves_the_checkpoint_and_reports_missing_sources() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    let sha = poolrad_checkout(&paths.dashboard_repo, GOOD_REPLACEMENTS);
    record_checkpoint(&paths.operations_db, "2026-11-05T09:30:00-08:00", &sha);
    let (auth, session) = owner_session(&dir.path().join("desk"));

    let published = Arc::new(activity_backend(
        &dir.path().join("unused.sock"),
        paths.clone(),
        Arc::new(SystemClock::new()),
    ));
    let app = activity_router(
        published as Arc<dyn ActivityRoutes>,
        auth.clone(),
        RequestScheme::Http,
    );
    let (status, _, body) = send(
        &app,
        "GET",
        "/api/poolrad-dashboard/progress",
        Some(&session),
    );
    assert_eq!(status, StatusCode::OK);
    let view = json_of(&body);
    assert_eq!(view["source"], "published main");
    assert_eq!(view["strict_percent"], json!(40.0));
    assert_eq!(view["identity_percent"], json!(43.3));
    assert_eq!(view["observed_at"], "2026-11-05T09:30:00-08:00");
    assert_eq!(view["refresh_after_seconds"], json!(60));
    assert_eq!(view["replacements"]["state"], "ok");
    assert_eq!(view["replacements"]["count"], json!(2));

    // A fresh backend has an empty cache. Without the checkpoint row the unported fallback
    // applies, so the route answers unavailable rather than a zero.
    fs::remove_file(&paths.operations_db).unwrap();
    let unpublished = Arc::new(activity_backend(
        &dir.path().join("unused.sock"),
        paths.clone(),
        Arc::new(SystemClock::new()),
    ));
    let app = activity_router(
        unpublished as Arc<dyn ActivityRoutes>,
        auth.clone(),
        RequestScheme::Http,
    );
    let (status, _, body) = send(
        &app,
        "GET",
        "/api/poolrad-dashboard/progress",
        Some(&session),
    );
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        json_of(&body),
        json!({"detail": "Poolrad dashboard progress unavailable"})
    );

    // Without dashboard.py the progress route is absent, as in Python.
    fs::remove_file(paths.dashboard_dir.join("dashboard.py")).unwrap();
    let absent = Arc::new(activity_backend(
        &dir.path().join("unused.sock"),
        paths,
        Arc::new(SystemClock::new()),
    ));
    let app = activity_router(absent as Arc<dyn ActivityRoutes>, auth, RequestScheme::Http);
    let (status, _, body) = send(
        &app,
        "GET",
        "/api/poolrad-dashboard/progress",
        Some(&session),
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_of(&body),
        json!({"detail": "Poolrad dashboard unavailable"})
    );
}

#[test]
fn dashboard_and_dash_routes_serve_only_the_allowlisted_static_contracts() {
    let dir = TempDir::new().unwrap();
    let paths = paths_in(dir.path());
    fs::write(paths.dashboard_dir.join("dashboard.py"), "# present\n").unwrap();
    fs::write(
        paths.dashboard_dir.join("dashboard.html"),
        b"<p>poolrad</p>",
    )
    .unwrap();
    fs::write(
        paths
            .dashboard_dir
            .join("state/reports/SLOT_A_FIRST_EGA_ROW64.md"),
        b"report",
    )
    .unwrap();
    fs::create_dir_all(&paths.ui_dist).unwrap();
    fs::write(paths.ui_dist.join("index.html"), b"<html>desk</html>").unwrap();
    let (auth, session) = owner_session(&dir.path().join("desk"));

    let dashboard = dashboard_router(
        Arc::new(PoolradDashboardBackend::new(paths.clone())) as Arc<dyn DashboardRoutes>,
        auth.clone(),
        RequestScheme::Http,
    );
    let (status, headers, body) = send(
        &dashboard,
        "GET",
        "/api/poolrad-dashboard/dashboard.html",
        Some(&session),
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"<p>poolrad</p>");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(
        headers["content-security-policy"],
        "sandbox allow-scripts; default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'self'"
    );

    let (status, _, body) = send(
        &dashboard,
        "GET",
        "/api/poolrad-dashboard/state/reports/SLOT_A_FIRST_EGA_ROW64.md",
        Some(&session),
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"report");
    let (status, _, body) = send(
        &dashboard,
        "GET",
        "/api/poolrad-dashboard/state/reports/secrets.md",
        Some(&session),
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_of(&body), json!({"detail": "Report unavailable"}));
    let (status, _, _) = send(
        &dashboard,
        "POST",
        "/api/poolrad-dashboard/refresh",
        Some(&session),
    );
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "refresh stays unavailable, never a fake success"
    );

    let dash = dash_router(
        Arc::new(DashBackend::new(paths.ui_dist.clone())) as Arc<dyn DashRoutes>,
        auth.clone(),
        RequestScheme::Http,
    );
    let (status, _, body) = send(&dash, "GET", "/dash/cos", Some(&session));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"<html>desk</html>");
    let (status, headers, _) = send(&dash, "GET", "/dash", None);
    assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(headers[header::LOCATION], "/dash/cos");
    let (status, _, _) = send(&dash, "GET", "/dash/cos", None);
    assert_eq!(status, StatusCode::FOUND, "no session goes to sign-in");
}
