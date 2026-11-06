#[path = "../src/ops_summary.rs"]
mod ops_summary;
mod roster {
    pub use ds_web::roster::*;
}
mod routes {
    pub use ds_web::routes::*;
}
#[path = "../src/owner_fleet_backend.rs"]
mod owner_fleet_backend;

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension, Router,
};
use chrono::{DateTime, Utc};
use ds_core::client::{FlockClient, UnixTransport};
use owner_fleet_backend::{OwnerFleetBackend, OwnerFleetConfig};
use routes::{
    auth::Identity,
    fleet::{FleetError, FleetSources},
    messages_write::MessageIn,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::symlink,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::AtomicBool,
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tempfile::{Builder, TempDir};
use tower::ServiceExt;

static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

fn fixture_dir() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.test-fixtures");
    fs::create_dir_all(&root).unwrap();
    Builder::new()
        .prefix(&format!(
            "owner-fleet-{}-{}-",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ))
        .tempdir_in(root)
        .unwrap()
}

fn fixture_socket() -> PathBuf {
    PathBuf::from(format!(
        "../../target/owner-fleet-{}-{}.sock",
        std::process::id(),
        FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn init_desk_db(path: &Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE sessions(token_hash TEXT PRIMARY KEY,expires_at REAL NOT NULL,viewport_width INTEGER,viewport_height INTEGER,viewport_dpr REAL,viewport_screen_width INTEGER,viewport_screen_height INTEGER,viewport_at REAL);
         CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT,channel TEXT NOT NULL,role TEXT NOT NULL,body TEXT NOT NULL,delivery TEXT NOT NULL DEFAULT 'posted',delivery_detail TEXT NOT NULL DEFAULT '',transport_id TEXT,transport_attempts INTEGER NOT NULL DEFAULT 0,transport_attempt_at REAL NOT NULL DEFAULT 0,external_id TEXT,created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),answer_id INTEGER,answered_at TEXT,direct_transport TEXT,delivered_at REAL,delivery_latency_s REAL);
         CREATE UNIQUE INDEX messages_external_id ON messages(channel,external_id) WHERE external_id IS NOT NULL;
         CREATE TABLE attachments(id TEXT PRIMARY KEY,filename TEXT NOT NULL,mime TEXT NOT NULL,size INTEGER NOT NULL,path TEXT NOT NULL,message_id INTEGER REFERENCES messages(id),created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));",
    ).unwrap();
}

/// What the scripted daemon answers. Fixed for the life of one daemon.
#[derive(Clone, Copy)]
struct Script {
    partial: bool,
    /// Send route reported by `delivery_capability`.
    route: &'static str,
    /// Reply selector for `send_text`.
    outcome: &'static str,
    /// Roster shape: "lane" (pane %7 is a lane), "lane+shell" (adds a plain pane %8), or "none".
    panes: &'static str,
}

impl Script {
    fn guarded(outcome: &'static str) -> Self {
        Self {
            partial: false,
            route: "guarded",
            outcome,
            panes: "lane",
        }
    }
}

struct DaemonState {
    script: Script,
    /// Liveness reported for every pane; tests flip it to model a dead pane.
    alive: AtomicBool,
    requests: Mutex<Vec<Value>>,
}

struct Daemon {
    stop: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
    state: Arc<DaemonState>,
    socket: PathBuf,
}

impl Daemon {
    fn count(&self, command: &str) -> usize {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["cmd"] == command)
            .count()
    }

    fn last_args(&self, command: &str) -> Value {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|request| request["cmd"] == command)
            .map(|request| request["args"].clone())
            .unwrap_or(Value::Null)
    }

    fn set_alive(&self, alive: bool) {
        self.state.alive.store(alive, Ordering::Relaxed);
    }

    fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        fs::remove_file(self.socket).unwrap();
    }
}

fn start_scripted(socket: &Path, script: Script) -> Daemon {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let state = Arc::new(DaemonState {
        script,
        alive: AtomicBool::new(true),
        requests: Mutex::new(Vec::new()),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let serving = state.clone();
    let thread = thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => handle_request(stream, &serving),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(_) => break,
            }
        }
    });
    Daemon {
        stop,
        thread,
        state,
        socket: socket.to_owned(),
    }
}

fn start_daemon(socket: &Path, partial: bool) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    start_daemon_with_outcome(socket, partial, "ok")
}

fn start_daemon_with_outcome(
    socket: &Path,
    partial: bool,
    transport_outcome: &'static str,
) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let daemon = start_scripted(
        socket,
        Script {
            partial,
            ..Script::guarded(transport_outcome)
        },
    );
    (daemon.stop, daemon.thread)
}

fn handle_request(mut stream: UnixStream, state: &DaemonState) {
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    let id = request["id"].clone();
    let command = request["cmd"].as_str().unwrap_or_default().to_owned();
    let script = state.script;
    let alive = state.alive.load(Ordering::Relaxed);
    let failed =
        script.partial && matches!(command.as_str(), "roster" | "state_latest" | "owner_stats");
    let result = match command.as_str() {
        "list_panes" => {
            let lane = json!({"pane_id":"%7","agent":"codex","agent_status":"working","label":"cos","tab_label":"desk","cwd":"/work/poolrad"});
            let shell = json!({"pane_id":"%8","label":"shell","cwd":"/work/shell"});
            match script.panes {
                "none" => json!({"panes":[]}),
                "lane+shell" => json!({"panes":[lane, shell]}),
                _ => json!({"panes":[lane]}),
            }
        }
        "delivery_capability" => {
            json!({"alive":alive,"agent_alive":alive,"send":script.route,"drains":false})
        }
        "owner_ingest" => {
            let external_id = request["args"]["external_id"].clone();
            let duplicate = external_id.is_string()
                && state.requests.lock().unwrap().iter().any(|earlier| {
                    earlier["cmd"] == "owner_ingest"
                        && earlier["args"]["external_id"] == external_id
                });
            json!({"message":{"message_id":"i42"},"duplicate":duplicate})
        }
        "owner_conversation" => {
            json!({"messages":[{"id":"i4","sender":null,"transport":"web","body":"hello","created_at":1793400000,"answered":false}],"waiting":[]})
        }
        "owner_stats" => json!({"ok":true,"pending":[]}),
        "state_latest" => {
            json!({"rows":[{"pane_id":"%7","at":1793400000,"session_id":"session-abc"}]})
        }
        "roster" => json!({"lanes":[{"name":"cos","pane_id":"%7","inbox":{"unread":2}}]}),
        "send_text" => match script.outcome {
            "refused" => {
                json!({"ok":false,"outcome":"refused","message_id":"i99","message":"fixture refusal"})
            }
            "queued" => json!({"ok":true,"outcome":"queued","message_id":"i99","message":"held"}),
            "unconfirmed" => {
                json!({"ok":true,"outcome":"unconfirmed","message_id":"i99","message":"neither witness fired"})
            }
            "missing" => json!({"ok":true,"message":"missing outcome"}),
            _ => json!({"ok":true,"outcome":"ok","message_id":"i99","message":"sent"}),
        },
        "owner_push" => json!({"ok":true,"outcome":"stored","id":"i99","message":"stored"}),
        _ => json!({}),
    };
    state.requests.lock().unwrap().push(request);
    let response = if failed {
        json!({"id":id,"ok":false,"error":"fixture source failed"})
    } else {
        json!({"id":id,"ok":true,"result":result})
    };
    writeln!(stream, "{}", response).unwrap();
}

fn rpc_backend(
    dir: &Path,
    socket: &Path,
) -> Arc<ds_web::panes_backend::DaemonPanesBackend<UnixTransport>> {
    let flock = FlockClient::new(
        UnixTransport {
            path: socket.to_owned(),
        },
        None,
        "private test service identity".into(),
        Duration::from_millis(500),
        Some("fixture-only-token".into()),
    );
    Arc::new(
        ds_web::panes_backend::DaemonPanesBackend::open(
            flock,
            dir,
            vec![json!({"id":"cos","pane":"%7"})],
        )
        .unwrap(),
    )
}

fn make_backend(
    dir: &Path,
    rpc: Arc<dyn ds_web::routes::panes::PanesWebBackend>,
    ops_db: PathBuf,
) -> Arc<OwnerFleetBackend> {
    make_backend_with(dir, rpc, ops_db, |config| config)
}

fn make_backend_with(
    dir: &Path,
    rpc: Arc<dyn ds_web::routes::panes::PanesWebBackend>,
    ops_db: PathBuf,
    tune: impl FnOnce(OwnerFleetConfig) -> OwnerFleetConfig,
) -> Arc<OwnerFleetBackend> {
    let config = OwnerFleetConfig::new(dir.join("messages.sqlite3"), ops_db)
        .with_local_sources(
            dir.join("lanes.json"),
            vec![dir.join("sessions")],
            dir.join("proc"),
            dir.join("home"),
            dir.join("tmux-fixture"),
            HashMap::from([("cos".into(), json!({"id":"cos","pane":"%7"}))]),
        )
        .with_desk_channel(json!({"id":"cos","pane":"%7"}))
        .with_summary_sources(dir.join("host-watch.json"), dir.join("opencode.sqlite3"))
        .with_summary_clock(|| {
            DateTime::parse_from_rfc3339("2026-11-05T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        });
    Arc::new(OwnerFleetBackend::open(rpc, tune(config)).unwrap())
}

struct Auth;
impl ds_web::routes::auth::AuthBackend for Auth {
    fn login<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
    ) -> ds_web::routes::auth::AuthFuture<'a, Result<String, ds_web::routes::auth::AuthError>> {
        Box::pin(async {
            Err(ds_web::routes::auth::AuthError::Internal(
                "unexpected".into(),
            ))
        })
    }
    fn identity<'a>(
        &'a self,
        _: Option<&'a str>,
        _: Option<&'a str>,
    ) -> ds_web::routes::auth::AuthFuture<
        'a,
        Result<ds_web::routes::auth::Identity, ds_web::routes::auth::AuthError>,
    > {
        Box::pin(async { Ok(ds_web::routes::auth::Identity::Owner) })
    }
    fn logout<'a>(
        &'a self,
        _: Option<&'a str>,
    ) -> ds_web::routes::auth::AuthFuture<'a, Result<(), ds_web::routes::auth::AuthError>> {
        Box::pin(async { Ok(()) })
    }
    fn change_password<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
    ) -> ds_web::routes::auth::AuthFuture<'a, Result<(), ds_web::routes::auth::AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

fn router(
    backend: Arc<OwnerFleetBackend>,
    rpc: Arc<dyn ds_web::routes::panes::PanesWebBackend>,
) -> Router {
    let auth: Arc<dyn ds_web::routes::auth::AuthBackend> = Arc::new(Auth);
    let owner_backend: Arc<dyn ds_web::routes::owner::OwnerWebBackend> = backend.clone();
    let owner = ds_web::routes::owner::router()
        .layer(Extension(auth.clone()))
        .layer(Extension(Arc::new(
            ds_web::routes::owner::OwnerWebState::new(owner_backend),
        )));
    let fleet = ds_web::routes::fleet::router(
        Arc::new(ds_web::routes::fleet::FleetState::new(rpc, backend)),
        auth,
        ds_web::routes::auth::RequestScheme::Http,
    );
    Router::new().merge(owner).merge(fleet)
}

async fn body(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn auth_request(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer fixture");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    builder
        .body(
            body.map(|body| Body::from(body.to_owned()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap()
}

async fn post_to(app: &Router, uri: &str, payload: &str) -> (StatusCode, Value) {
    body(
        app.clone()
            .oneshot(auth_request("POST", uri, Some(payload)))
            .await
            .unwrap(),
    )
    .await
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    body(
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await
}

/// One codex process, its session file, and a tmux pane pid, all owned by the test user.
fn write_codex_process(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let pid = 43210u32;
    let process = dir.join("proc").join(pid.to_string());
    fs::create_dir_all(process.join("fd")).unwrap();
    fs::write(
        process.join("stat"),
        format!("{pid} (codex) S 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"),
    )
    .unwrap();
    fs::write(process.join("cmdline"), b"codex\0run\0--private-arg\0").unwrap();
    symlink("/usr/bin/codex", process.join("exe")).unwrap();
    let project = dir.join("project");
    fs::create_dir_all(&project).unwrap();
    symlink(&project, process.join("cwd")).unwrap();
    let session = dir.join("home/.codex/sessions/test/session_abc.jsonl");
    fs::create_dir_all(session.parent().unwrap()).unwrap();
    fs::write(&session, "fixture only").unwrap();
    symlink(&session, process.join("fd/3")).unwrap();
    let script = dir.join("tmux-fixture");
    fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '%9 {pid}'\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn owner_routes_and_channel_send_use_shared_real_socket_and_persistent_db() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let (stop, server) = start_daemon(&socket, false);
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend.clone(), rpc);

    let (status, cap) = body(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/capability?pane=%257")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cap["send"], "guarded");
    let (status, sent) = body(
        app.clone()
            .oneshot(auth_request(
                "POST",
                "/api/owner-send",
                Some(r#"{"text":"hello owner"}"#),
            ))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sent["message_id"], "i42");
    assert_eq!(sent["notify"]["state"], "sent");
    let (status, thread) = body(
        app.clone()
            .oneshot(auth_request("GET", "/api/owner-thread?limit=5", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(thread["items"][0]["message_id"], "i4");

    let (status, message) = body(
        app.oneshot(auth_request(
            "POST",
            "/api/pane-send",
            Some(r#"{"channel":"cos","pane":"%7","text":"hi lane"}"#),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(message["transport"]["outcome"], "ok");
    let db = Connection::open(dir.path().join("messages.sqlite3")).unwrap();
    let saved: String = db
        .query_row("SELECT body FROM messages WHERE channel='cos'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(saved, "hi lane");
    drop(db);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn fleet_keeps_partial_sources_unknown_and_inventory_private() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    fs::write(dir.path().join("lanes.json"), b"not-json").unwrap();
    write_codex_process(dir.path());
    let socket = fixture_socket();
    let (stop, server) = start_daemon(&socket, true);
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend, rpc);
    let (status, fleet) = body(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/fleet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fleet["state"], "ok");
    assert_eq!(fleet["rows"][0]["pending_state"], "unknown");
    assert_eq!(fleet["rows"][0]["unread"], 0);
    let inventory = fleet["unrouted"]["rows"][0].as_object().unwrap();
    assert_eq!(inventory["agent"], "codex");
    assert!(!inventory.contains_key("cmdline"));
    assert!(!inventory.contains_key("ppid"));
    assert_eq!(inventory["pane_id"], "%9");
    let (status, summary) = body(
        app.oneshot(auth_request("GET", "/api/ops-metrics", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(summary["detail"]
        .as_str()
        .unwrap()
        .contains("OperationalError"));
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn missing_daemon_and_bad_summary_path_are_reported_without_starting_services() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(
        dir.path(),
        rpc.clone(),
        dir.path().join("missing-ops.sqlite3"),
    );
    let app = router(backend, rpc);
    let (status, cap) = body(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/capability?pane=%257")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(cap["detail"]
        .as_str()
        .unwrap()
        .contains("could not describe"));
    let (status, fleet) = body(
        app.oneshot(
            Request::builder()
                .uri("/api/fleet")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fleet["state"], "unavailable");
}

#[tokio::test]
async fn stale_daemon_socket_without_listener_is_reported_unavailable() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    // A dead daemon leaves its socket file behind with nothing listening on it.
    {
        let _stale_listener = UnixListener::bind(&socket).unwrap();
    }
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(
        dir.path(),
        rpc.clone(),
        dir.path().join("missing-ops.sqlite3"),
    );
    let app = router(backend, rpc);
    let (status, cap) = body(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/api/capability?pane=%257")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(cap["detail"]
        .as_str()
        .unwrap()
        .contains("could not describe"));
    let (status, fleet) = body(
        app.oneshot(
            Request::builder()
                .uri("/api/fleet")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fleet["state"], "unavailable");
    fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn owner_push_route_records_stored_and_never_types_into_the_lane() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(
        &socket,
        Script {
            route: "owner_push",
            ..Script::guarded("ok")
        },
    );
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend, rpc);
    let (status, message) = post_to(
        &app,
        "/api/pane-send",
        r#"{"channel":"cos","pane":"%7","text":"queued for the lane"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(message["delivery"], "stored");
    assert_eq!(message["transport"]["outcome"], "stored");
    assert_eq!(daemon.count("owner_push"), 1);
    assert_eq!(daemon.count("send_text"), 0);
    daemon.finish();
}

#[tokio::test]
async fn guarded_send_outcomes_are_recorded_as_the_daemon_reports_them() {
    for (outcome, recorded) in [
        ("queued", "queued"),
        ("unconfirmed", "unconfirmed"),
        ("missing", "uncertain"),
    ] {
        let dir = fixture_dir();
        init_desk_db(&dir.path().join("messages.sqlite3"));
        let socket = fixture_socket();
        let daemon = start_scripted(&socket, Script::guarded(outcome));
        let rpc = rpc_backend(dir.path(), &socket);
        let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
        let app = router(backend, rpc);
        let (status, message) = post_to(
            &app,
            "/api/pane-send",
            r#"{"channel":"cos","pane":"%7","text":"outcome check"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "daemon outcome {outcome}");
        assert_eq!(message["delivery"], recorded, "daemon outcome {outcome}");
        daemon.finish();
    }
}

#[tokio::test]
async fn send_reads_liveness_fresh_while_discovery_keeps_its_cache() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(
        &socket,
        Script {
            panes: "lane+shell",
            ..Script::guarded("ok")
        },
    );
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let shell_message = |body: &str| MessageIn {
        channel: "pane-8".into(),
        body: body.into(),
        attachment_ids: Vec::new(),
        external_id: None,
    };
    // Discovery caches a live answer for the plain pane %8 while this send is delivered.
    let warm = backend
        .post_direct_message(shell_message("warm"), Identity::Owner)
        .await
        .unwrap();
    assert_eq!(warm["delivery"], "ok");
    assert_eq!(daemon.count("send_text"), 1);
    // The pane dies inside the 20 s window. Discovery still reuses the live answer, so the
    // pre-check passes, but the send must read the daemon fresh and refuse.
    daemon.set_alive(false);
    let refused = backend
        .post_direct_message(shell_message("after the pane died"), Identity::Owner)
        .await
        .unwrap();
    assert_eq!(refused["delivery"], "uncertain");
    assert_eq!(
        refused["transport"]["error"],
        "%8 is not alive: nothing to send to"
    );
    assert_eq!(daemon.count("send_text"), 1);
    assert_eq!(daemon.count("owner_push"), 0);
    daemon.finish();
}

#[tokio::test]
async fn discovery_cache_is_refreshed_after_twenty_seconds() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(
        &socket,
        Script {
            panes: "lane+shell",
            ..Script::guarded("ok")
        },
    );
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let shell_message = || MessageIn {
        channel: "pane-8".into(),
        body: "probe".into(),
        attachment_ids: Vec::new(),
        external_id: None,
    };
    backend
        .post_direct_message(shell_message(), Identity::Owner)
        .await
        .unwrap();
    let reads_before = daemon.count("delivery_capability");
    daemon.set_alive(false);
    // Within 20 s the cached live answer still admits the channel. After 20 s discovery must
    // read the daemon again and refuse it.
    thread::sleep(Duration::from_secs(21));
    match backend
        .post_direct_message(shell_message(), Identity::Owner)
        .await
    {
        Err(FleetError::Http(status, detail)) => {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(detail, "No active agent is available in this channel");
        }
        other => panic!("expected a 409 once the cache expired, got {other:?}"),
    }
    assert!(daemon.count("delivery_capability") > reads_before);
    assert_eq!(daemon.count("send_text"), 1);
    daemon.finish();
}

#[tokio::test]
async fn unknown_channel_and_empty_message_are_refused_before_any_send() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(&socket, Script::guarded("ok"));
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend.clone(), rpc);
    let (status, unknown) = post_to(
        &app,
        "/api/pane-send",
        r#"{"channel":"nope","pane":"%7","text":"hello"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(unknown["detail"], "Unknown channel");
    let (status, empty) = post_to(
        &app,
        "/api/pane-send",
        r#"{"channel":"cos","pane":"%7","text":"   "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(empty["detail"], "Message is empty");
    let (status, owner_empty) = post_to(&app, "/api/owner-send", r#"{"text":"   "}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(owner_empty["detail"], "nothing to send");
    // The backend enforces the same rules for any caller, not only the route.
    match backend
        .post_direct_message(
            MessageIn {
                channel: "nope".into(),
                body: "hello".into(),
                attachment_ids: Vec::new(),
                external_id: None,
            },
            Identity::Owner,
        )
        .await
    {
        Err(FleetError::Http(status, detail)) => {
            assert_eq!(status, StatusCode::NOT_FOUND);
            assert_eq!(detail, "Unknown channel");
        }
        other => panic!("expected 404 for an unknown channel, got {other:?}"),
    }
    match backend
        .post_direct_message(
            MessageIn {
                channel: "cos".into(),
                body: "   ".into(),
                attachment_ids: Vec::new(),
                external_id: None,
            },
            Identity::Owner,
        )
        .await
    {
        Err(FleetError::Http(status, detail)) => {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(detail, "Message is empty");
        }
        other => panic!("expected 422 for an empty body, got {other:?}"),
    }
    assert_eq!(daemon.count("send_text"), 0);
    assert_eq!(daemon.count("owner_push"), 0);
    daemon.finish();
}

#[tokio::test]
async fn no_agent_in_channel_is_refused_as_409_without_a_send() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(
        &socket,
        Script {
            panes: "none",
            ..Script::guarded("ok")
        },
    );
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend.clone(), rpc);
    let (status, _) = post_to(
        &app,
        "/api/pane-send",
        r#"{"channel":"cos","pane":"%7","text":"anyone there?"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    match backend
        .post_direct_message(
            MessageIn {
                channel: "cos".into(),
                body: "anyone there?".into(),
                attachment_ids: Vec::new(),
                external_id: None,
            },
            Identity::Owner,
        )
        .await
    {
        Err(FleetError::Http(status, detail)) => {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(detail, "No active agent is available in this channel");
        }
        other => panic!("expected 409 with no agent, got {other:?}"),
    }
    let db = Connection::open(dir.path().join("messages.sqlite3")).unwrap();
    let stored: i64 = db
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored, 0);
    drop(db);
    assert_eq!(daemon.count("send_text"), 0);
    assert_eq!(daemon.count("owner_push"), 0);
    daemon.finish();
}

#[tokio::test]
async fn external_id_replay_returns_the_stored_message_without_a_second_send() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(&socket, Script::guarded("ok"));
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let replayed = || MessageIn {
        channel: "cos".into(),
        body: "once only".into(),
        attachment_ids: Vec::new(),
        external_id: Some("ext-1".into()),
    };
    let first = backend
        .post_direct_message(replayed(), Identity::Owner)
        .await
        .unwrap();
    let replay = backend
        .post_direct_message(replayed(), Identity::Owner)
        .await
        .unwrap();
    assert_eq!(first["delivery"], "ok");
    assert_eq!(replay["id"], first["id"]);
    assert_eq!(daemon.count("send_text"), 1);
    let db = Connection::open(dir.path().join("messages.sqlite3")).unwrap();
    let rows: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE external_id='ext-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    drop(db);
    daemon.finish();
}

#[tokio::test]
async fn owner_send_forwards_external_id_and_passes_duplicate_through() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let daemon = start_scripted(&socket, Script::guarded("ok"));
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend, rpc);
    let payload = r#"{"text":"hello owner","external_id":"ext-42"}"#;
    let (status, first) = post_to(&app, "/api/owner-send", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["message_id"], "i42");
    assert_eq!(first["duplicate"], false);
    let (status, second) = post_to(&app, "/api/owner-send", payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["message_id"], "i42");
    assert_eq!(second["duplicate"], true);
    assert_eq!(daemon.last_args("owner_ingest")["external_id"], "ext-42");
    daemon.finish();
}

#[tokio::test]
async fn inventory_lists_only_processes_owned_by_this_process() {
    use std::os::unix::fs::MetadataExt;
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    write_codex_process(dir.path());
    let socket = fixture_socket();
    let daemon = start_scripted(&socket, Script::guarded("ok"));
    let rpc = rpc_backend(dir.path(), &socket);
    let own_uid = fs::metadata("/proc/self").unwrap().uid();
    let owned = router(
        make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3")),
        rpc.clone(),
    );
    let (status, fleet) = get_json(&owned, "/api/fleet").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fleet["unrouted"]["rows"][0]["agent"], "codex");
    let foreign = router(
        make_backend_with(
            dir.path(),
            rpc.clone(),
            dir.path().join("ops.sqlite3"),
            |config| config.with_process_uid(own_uid.wrapping_add(1)),
        ),
        rpc,
    );
    let (status, fleet) = get_json(&foreign, "/api/fleet").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fleet["state"], "ok");
    assert_eq!(fleet["unrouted"]["rows"].as_array().map(Vec::len), Some(0));
    daemon.finish();
}

#[tokio::test]
async fn operations_summary_reads_existing_collector_schema_and_returns_real_payload() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let ops_db = dir.path().join("operations.sqlite3");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let python="import sys;sys.path.insert(0,'bridges');from pathlib import Path;import ops_metrics;ops_metrics.connect(Path(sys.argv[1])).close()";
    let result = Command::new("python3")
        .arg("-c")
        .arg(python)
        .arg(&ops_db)
        .current_dir(&repository)
        .status()
        .unwrap();
    assert!(result.success());
    let socket = fixture_socket();
    let (stop, server) = start_daemon(&socket, false);
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), ops_db);
    let app = router(backend, rpc);
    let (status, summary) = body(
        app.oneshot(auth_request("GET", "/api/ops-metrics", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    for key in [
        "project",
        "generated_at",
        "sources",
        "source_freshness",
        "staff",
        "pane_roster",
    ] {
        assert!(summary.get(key).is_some(), "summary is missing {key}");
    }
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    fs::remove_file(socket).unwrap();
}

#[tokio::test]
async fn direct_refusal_is_persisted_without_becoming_success() {
    let dir = fixture_dir();
    init_desk_db(&dir.path().join("messages.sqlite3"));
    let socket = fixture_socket();
    let (stop, server) = start_daemon_with_outcome(&socket, false, "refused");
    let rpc = rpc_backend(dir.path(), &socket);
    let backend = make_backend(dir.path(), rpc.clone(), dir.path().join("ops.sqlite3"));
    let app = router(backend, rpc);

    let (status, message) = body(
        app.oneshot(auth_request(
            "POST",
            "/api/pane-send",
            Some(r#"{"channel":"cos","pane":"%7","text":"must not be claimed delivered"}"#),
        ))
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(message["transport"]["ok"], false);
    assert_eq!(message["transport"]["outcome"], "refused");
    let db = Connection::open(dir.path().join("messages.sqlite3")).unwrap();
    let delivery: String = db
        .query_row(
            "SELECT delivery FROM messages WHERE channel='cos'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(delivery, "refused");

    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    fs::remove_file(socket).unwrap();
}
