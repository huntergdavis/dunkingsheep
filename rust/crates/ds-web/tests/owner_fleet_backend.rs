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
        Arc,
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

fn start_daemon(socket: &Path, partial: bool) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    start_daemon_with_outcome(socket, partial, "ok")
}

fn start_daemon_with_outcome(
    socket: &Path,
    partial: bool,
    transport_outcome: &'static str,
) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let thread = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => handle_request(stream, partial, transport_outcome),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(_) => break,
            }
        }
    });
    (stopping, thread)
}

fn handle_request(mut stream: UnixStream, partial: bool, transport_outcome: &str) {
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let request: Value = serde_json::from_str(&line).unwrap();
    let id = request["id"].clone();
    let command = request["cmd"].as_str().unwrap_or_default();
    let failed = partial && matches!(command, "roster" | "state_latest" | "owner_stats");
    let result = match command {
        "list_panes" => {
            json!({"panes":[{"pane_id":"%7","agent":"codex","agent_status":"working","label":"cos","tab_label":"desk","cwd":"/work/poolrad"}]})
        }
        "delivery_capability" => {
            json!({"alive":true,"agent_alive":true,"send":"guarded","drains":false})
        }
        "owner_ingest" => json!({"message":{"message_id":"i42"},"duplicate":false}),
        "owner_conversation" => {
            json!({"messages":[{"id":"i4","sender":null,"transport":"web","body":"hello","created_at":1793400000,"answered":false}],"waiting":[]})
        }
        "owner_stats" => json!({"ok":true,"pending":[]}),
        "state_latest" => {
            json!({"rows":[{"pane_id":"%7","at":1793400000,"session_id":"session-abc"}]})
        }
        "roster" => json!({"lanes":[{"name":"cos","pane_id":"%7","inbox":{"unread":2}}]}),
        "send_text" => match transport_outcome {
            "refused" => {
                json!({"ok":false,"outcome":"refused","message_id":"i99","message":"fixture refusal"})
            }
            "queued" => json!({"ok":true,"outcome":"queued","message_id":"i99","message":"held"}),
            "missing" => json!({"ok":true,"message":"missing outcome"}),
            _ => json!({"ok":true,"outcome":"ok","message_id":"i99","message":"sent"}),
        },
        "owner_push" => json!({"ok":true,"outcome":"stored","id":"i99"}),
        _ => json!({}),
    };
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
    Arc::new(OwnerFleetBackend::open(rpc, config).unwrap())
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
    let proc_root = dir.path().join("proc");
    let home = dir.path().join("home");
    let pid = 43210u32;
    let process = proc_root.join(pid.to_string());
    fs::create_dir_all(process.join("fd")).unwrap();
    fs::write(
        process.join("stat"),
        format!("{pid} (codex) S 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n"),
    )
    .unwrap();
    fs::write(process.join("cmdline"), b"codex\0run\0--private-arg\0").unwrap();
    symlink("/usr/bin/codex", process.join("exe")).unwrap();
    let project = dir.path().join("project");
    fs::create_dir_all(&project).unwrap();
    symlink(&project, process.join("cwd")).unwrap();
    let session = home.join(".codex/sessions/test/session_abc.jsonl");
    fs::create_dir_all(session.parent().unwrap()).unwrap();
    fs::write(&session, "fixture only").unwrap();
    symlink(&session, process.join("fd/3")).unwrap();
    let script = dir.path().join("tmux-fixture");
    fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '%9 {pid}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
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
    assert!(summary.get("source_freshness").is_some());
    assert!(summary.get("generated_at").is_some());
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
