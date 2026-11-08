//! t1702 stat-adapter gate, runtime composition (owner decision): the composed desk serves an
//! upload's metadata from the file's real stat.
//!
//! This opens the real `Desk` (`Desk::open`, then `Desk::router`), which is the runtime that
//! `serve` mounts, and drives it through its own login and upload routes. A unit test of the
//! adapter alone does not show that the runtime hands that adapter to the router; this does.
//!
//! The expected values are the Python desk's, recorded in `fixtures/uploads_range_oracle.json`
//! (the base case: the same 20-byte content and the same pinned mtime).

use std::{
    net::SocketAddr,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Method, Request, StatusCode},
};
use ds_web::{
    routes::auth::RequestScheme,
    web_server::{Desk, DeskConfig},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const CONTENT: &[u8] = b"0123456789ABCDEFGHIJ";
const MTIME: u64 = 1_700_000_000;
const ORACLE: &str = include_str!("fixtures/uploads_range_oracle.json");

/// The operations database, opened with the accepted schema, as the desk's own config expects.
fn open_operations_db(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!("../src/ops_schema.sql"))?;
    Ok(conn)
}

fn one_channel() -> Vec<Value> {
    vec![json!({"id": "cos", "name": "Chief of staff", "match": "cos", "kind": "active"})]
}

/// A temporary desk: every path under `dir`, with the bundle the static mount expects.
fn config(dir: &Path) -> DeskConfig {
    std::fs::create_dir_all(dir.join("ui/dist")).unwrap();
    std::fs::write(
        dir.join("ui/dist/index.html"),
        "<!doctype html><title>desk</title>",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    DeskConfig {
        data_dir: dir.join("desk"),
        config_dir: dir.join("config"),
        home: dir.join("home"),
        // No daemon listens here: this test does not need one.
        daemon_socket: dir.join("absent-daemon.sock"),
        service_token: Some("web-desk-test-token".to_owned()),
        client_timeout: Duration::from_secs(2),
        channels: one_channel(),
        ui_dist: dir.join("ui/dist"),
        dashboard_dir: dir.join("dashboard"),
        dashboard_repo: dir.join("poolrad"),
        poolrad_repo: dir.join("poolrad"),
        operations_db: dir.join("desk/operations.sqlite3"),
        host_watch_state: dir.join("desk/host-watch-state.json"),
        opencode_db: dir.join("home/opencode.db"),
        dunk_events_db: dir.join("config/metrics.db"),
        usage_config: dir.join("config/usage.json"),
        usage_refresh_s: 1800,
        usage_backoff_s: vec![300, 600, 1800],
        usage_max_age_s: 3600,
        board_interval_s: 3600,
        gh_program: dir.join("missing-gh"),
        tmux_program: dir.join("missing-tmux"),
        proc_root: dir.join("proc"),
        herdr_webui_url: None,
        herdr_webui_label: Some("Herdr web UI".to_owned()),
        initial_password: Some(PASSWORD.to_owned()),
        scheme: RequestScheme::Http,
        open_operations_db: open_operations_db as fn(&Path) -> rusqlite::Result<Connection>,
    }
}

fn multipart(filename: &str, mime: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----claude-desk-composition";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[test]
fn the_composed_desk_serves_an_upload_with_its_real_stat_metadata() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let base = oracle["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == "base")
        .expect("the oracle's base case");
    let want_headers = &base["headers"];

    let root = tempfile::TempDir::new().expect("temporary desk root");
    let config = config(root.path());
    let data_dir = config.data_dir.clone();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        // Built inside the runtime, as `serve` builds it.
        let desk = Desk::open(config).unwrap_or_else(|error| panic!("the desk opens: {error}"));
        let router = desk.router();

        // The desk's own login gives the owner session the upload needs.
        let login = json!({"password": PASSWORD, "next": "/dash/cos"}).to_string();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    // The login route reads the peer address (rate limiting). A real server supplies
                    // it; a oneshot request has none, so the extractor would answer 500 before the handler.
                    .extension(ConnectInfo(
                        "127.0.0.1:4000".parse::<SocketAddr>().expect("address"),
                    ))
                    .body(Body::from(login))
                    .expect("login request"),
            )
            .await
            .expect("login answers");
        assert_eq!(response.status(), StatusCode::OK, "login");
        let cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("vp_session="))
            .and_then(|value| value.split(';').next())
            .expect("login sets the vp_session cookie")
            .to_owned();

        let (content_type, body) = multipart("notes.txt", "text/plain", CONTENT);
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/uploads")
                    .header(header::COOKIE, cookie.as_str())
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .expect("upload request"),
            )
            .await
            .expect("upload answers");
        assert_eq!(response.status(), StatusCode::OK, "upload");
        let uploaded = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let id = serde_json::from_slice::<Value>(&uploaded).expect("upload JSON")["id"]
            .as_str()
            .expect("attachment id")
            .to_owned();

        // Pin the stored file's mtime to the oracle's, so the stat-derived headers are the
        // Python desk's. The stored file is the one the composed backend wrote.
        let stored = std::fs::read_dir(data_dir.join("uploads"))
            .expect("uploads folder")
            .map(|entry| entry.expect("entry").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&id))
            })
            .expect("the stored upload file");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stored)
            .expect("open the stored upload")
            .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))
            .expect("pin the stored mtime");
        let real_length = std::fs::metadata(&stored)
            .expect("stat the stored upload")
            .len();

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/uploads/{id}"))
                    .header(header::COOKIE, cookie.as_str())
                    .body(Body::empty())
                    .expect("download request"),
            )
            .await
            .expect("download answers");
        assert_eq!(response.status(), StatusCode::OK, "download");
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("download body")
            .to_vec();

        // The length the composed desk reports is the file's real stat length, and the bytes
        // are the stored content.
        assert_eq!(
            headers
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some(real_length.to_string().as_str()),
            "content-length is the file's stat length"
        );
        assert_eq!(body, CONTENT, "the bytes served are the stored content");

        // Every stat-derived header is the Python desk's value for the same file and mtime.
        for name in ["content-length", "etag", "last-modified", "content-type"] {
            let got = headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let want = want_headers[name].as_str().map(str::to_owned);
            assert_eq!(got, want, "{name} differs from the Python desk");
        }
    });
}
