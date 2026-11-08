//! t3721 (t1702 leaf): viewport, password, ops-board answer and ops-metrics refresh parity.
//!
//! The oracle is `fixtures/t3721_parity_oracle.json`, recorded by
//! `fixtures/generate_t3721_parity_oracle.py` against the Python desk's FastAPI routes (the
//! served venv's interpreter, FastAPI 0.162.0 / Starlette 1.10.0 / Pydantic 2.14.2) on scratch
//! data and config only. This test drives the real `Desk` router (the runtime `serve` mounts) in
//! the same order, with the same passwords, and compares status, content type and body for every
//! case. Probes check the side effects: the stored viewport (through `/api/channels`), the session
//! sweep after a password change, the rewritten owner-credentials file, and the login results.
//!
//! Known divergences are NOT hidden. Each one is listed in `KNOWN_GAPS` with the Python behaviour,
//! and the exact Rust response is pinned (status and body). The test fails if a gap answers
//! differently from its pin, if a gap starts to match the oracle, or if the set of divergent cases
//! changes. A gap is closed only by a scoped follow-up that changes the Rust route.
//!
//! The replay inventory is checked too: every case in the oracle must be replayed here, and no
//! other name may be recorded, so a regenerated oracle with new cases fails until the test names them.
//!
//! Security note (owner question, not a parity choice): the pinned Python 422 bodies echo the
//! submitted body. `password_missing_current` therefore echoes the new password in its `input`, and
//! this port matches that echo to stay faithful to the pinned route. Whether a password should be
//! echoed in an error body is an owner decision; until it is made, the echo is pinned as Python's.
//!
//! NOT covered here: the ops-board answer success path (it needs a live or scratch daemon with an
//! owner-issued token), the ops-metrics refresh owner path (it calls `gh` and reads host state),
//! the live desk on 127.0.0.1:8787, and any live owner database, token or config.

use std::{net::SocketAddr, path::Path, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Method, Request},
    Router,
};
use ds_web::{
    routes::auth::RequestScheme,
    web_server::{Desk, DeskConfig},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "t3721-initial-scratch";
const NEW_PASSWORD: &str = "t3721-changed-scratch";
const ORACLE: &str = include_str!("fixtures/t3721_parity_oracle.json");

/// A divergence from the pinned Python oracle. The Rust response is pinned exactly, so a gap cannot
/// drift to a different wrong answer unnoticed. Each entry is an owner-scoped follow-up, not an
/// accepted difference.
struct Gap {
    name: &'static str,
    reason: &'static str,
    rust_status: u16,
    rust_body: &'static str,
}

const KNOWN_GAPS: &[Gap] = &[
    Gap {
        name: "password_not_json",
        reason: "Python's json_invalid ctx.error is the Python decoder's text ('Expecting value'); Rust carries the serde_json text. The shape matches.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"expected ident at line 1 column 2"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "answer_missing_reply",
        reason: "Python answers 400 'a reply is required'. Rust checks the version first: the one-click contract, owner decision pending.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
    Gap {
        name: "answer_blank_reply",
        reason: "Python answers 400 'a reply is required'. Rust checks the version first: the one-click contract, owner decision pending.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
    Gap {
        name: "answer_no_daemon",
        reason: "Python's no-daemon answer is 503 'Could not unblock t1: <exception>'. The oracle's exception is the isolated client's RuntimeError; the production exception type depends on the host and is not captured. Rust checks the version first.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
];

fn open_operations_db(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!("../src/ops_schema.sql"))?;
    Ok(conn)
}

fn channels() -> Vec<Value> {
    vec![json!({"id": "cos", "name": "Chief of staff", "match": "cos", "kind": "active"})]
}

/// A temporary desk: every path under `dir`, with the bundle the static mount expects. The daemon
/// socket is absent, as in the Python oracle (scratch config, no daemon).
fn config(dir: &Path) -> DeskConfig {
    std::fs::create_dir_all(dir.join("ui/dist")).unwrap();
    std::fs::write(
        dir.join("ui/dist/index.html"),
        "<!doctype html><title>desk</title>",
    )
    .unwrap();
    for sub in ["config", "home"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    DeskConfig {
        data_dir: dir.join("desk"),
        config_dir: dir.join("config"),
        home: dir.join("home"),
        daemon_socket: dir.join("absent-daemon.sock"),
        service_token: None,
        client_timeout: Duration::from_secs(2),
        channels: channels(),
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

/// A request to the desk, with the Host header the Python TestClient sends (`testserver`), so the
/// same-origin check compares the same origin.
fn request(method: Method, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "testserver")
        .extension(ConnectInfo(
            "127.0.0.1:4000".parse::<SocketAddr>().expect("address"),
        ))
}

struct Reply {
    status: u16,
    content_type: Option<String>,
    body: Value,
    cookie: Option<String>,
}

async fn send(router: &Router, builder: axum::http::request::Builder, body: Body) -> Reply {
    let response = router
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("the router answers");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("vp_session="))
        .and_then(|value| value.split(';').next())
        .map(str::to_owned);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&bytes)}));
    Reply {
        status,
        content_type,
        body,
        cookie,
    }
}

fn json_body(value: &Value) -> Body {
    Body::from(value.to_string())
}

async fn login(router: &Router, password: &str) -> Reply {
    let builder =
        request(Method::POST, "/api/login").header(header::CONTENT_TYPE, "application/json");
    send(
        router,
        builder,
        json_body(&json!({"password": password, "next": "/dash/cos"})),
    )
    .await
}

fn case<'a>(oracle: &'a Value, name: &str) -> &'a Value {
    oracle["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("oracle case {name}"))
}

/// Compares one HTTP case with the oracle. Returns the failure text when they differ. A case the
/// oracle recorded without a body (a status-only probe) is compared on status alone.
fn compare(name: &str, reply: &Reply, want: &Value) -> Option<String> {
    let want_status = want["status"].as_u64().expect("status") as u16;
    if want.get("body").is_none() {
        return (reply.status != want_status).then(|| {
            format!(
                "{name}: status {} but the Python oracle is {want_status}",
                reply.status
            )
        });
    }
    let want_type = want["content_type"].as_str().map(str::to_owned);
    if reply.status != want_status || reply.content_type != want_type || reply.body != want["body"]
    {
        Some(format!(
            "{name}: got {} {:?} {} but the Python oracle is {} {:?} {}",
            reply.status, reply.content_type, reply.body, want_status, want_type, want["body"]
        ))
    } else {
        None
    }
}

fn viewport_payload() -> Value {
    json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})
}

#[test]
fn the_composed_desk_matches_the_python_desk_for_viewport_password_answer_and_refresh() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let root = tempfile::TempDir::new().expect("temporary desk root");
    let data_dir = root.path().join("desk");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let desk = Desk::open(config(root.path())).unwrap_or_else(|error| panic!("the desk opens: {error}"));
        let router = desk.router();
        let mut failures = Vec::new();
        // Ad-hoc probe failures, merged into `failures` after the last `check` call.
        let mut extra: Vec<String> = Vec::new();
        let mut diverged: Vec<String> = Vec::new();
        // Every replayed case is recorded here; the inventory is checked against the oracle at the end.
        let mut replayed: Vec<String> = Vec::new();
        // Probe names recorded outside `check` (their side effects are checked by hand below).
        let mut probes: Vec<String> = Vec::new();
        let mut check = |name: &str, reply: &Reply| {
            replayed.push(name.to_owned());
            let differs = compare(name, reply, case(&oracle, name));
            match KNOWN_GAPS.iter().find(|gap| gap.name == name) {
                None => {
                    if let Some(text) = differs {
                        failures.push(text);
                    }
                }
                Some(gap) => {
                    if differs.is_none() {
                        failures.push(format!("{name}: now matches the Python oracle; remove it from KNOWN_GAPS"));
                    }
                    let pinned: Value = serde_json::from_str(gap.rust_body).expect("pinned gap body is JSON");
                    if reply.status != gap.rust_status || reply.body != pinned {
                        failures.push(format!(
                            "{name}: Rust answered {} {} but the pinned gap is {} {} ({})",
                            reply.status, reply.body, gap.rust_status, pinned, gap.reason
                        ));
                    }
                    diverged.push(name.to_owned());
                }
            }
        };

        // Owner session for the viewport cases.
        let owner_login = login(&router, PASSWORD).await;
        check("login_owner", &owner_login);
        // The login reply's Set-Cookie value already reads `vp_session=<token>`.
        let owner_cookie_header = owner_login.cookie.clone().expect("login sets vp_session");

        // Viewport cases, in the oracle's order.
        let viewport = |name: &str, body: Body, extra: &[(&str, &str)], cookie: Option<&str>| {
            let mut builder = request(Method::POST, "/api/viewport").header(header::CONTENT_TYPE, "application/json");
            for (key, value) in extra {
                builder = builder.header(*key, *value);
            }
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            let _ = name;
            (builder, body)
        };
        let run = |_name: &str, (builder, body): (axum::http::request::Builder, Body)| {
            let router = router.clone();
            async move { send(&router, builder, body).await }
        };
        let base = viewport_payload();

        let reply = run("viewport_ok", viewport("viewport_ok", json_body(&base), &[], Some(&owner_cookie_header))).await;
        check("viewport_ok", &reply);

        // Side-effect probe: the stored viewport is read through /api/channels.
        let probe = send(
            &router,
            request(Method::GET, "/api/channels").header(header::COOKIE, owner_cookie_header.as_str()),
            Body::empty(),
        )
        .await;
        probes.push("viewport_ok_probe_channels".to_owned());
        let want_probe = case(&oracle, "viewport_ok_probe_channels");
        let rows = probe.body["viewports"].as_array().cloned().unwrap_or_default();
        let stored: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.as_object_mut().map(|object| object.remove("viewport_at"));
                row
            })
            .collect();
        let presence: Vec<bool> = rows.iter().map(|row| row.get("viewport_at").is_some()).collect();
        if probe.status != want_probe["status"].as_u64().expect("status") as u16
            || stored != want_probe["stored"].as_array().cloned().unwrap_or_default()
            || presence != want_probe["viewport_at_present"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect::<Vec<_>>()
        {
            extra.push(format!("viewport_ok_probe_channels: stored {stored:?} presence {presence:?} status {}", probe.status));
        }

        let reply = run("viewport_same_origin", viewport("", json_body(&base), &[("origin", "http://testserver")], Some(&owner_cookie_header))).await;
        check("viewport_same_origin", &reply);
        let reply = run("viewport_cross_origin", viewport("", json_body(&base), &[("origin", "https://evil.example")], Some(&owner_cookie_header))).await;
        check("viewport_cross_origin", &reply);
        let reply = run("viewport_no_cookie", viewport("", json_body(&base), &[], None)).await;
        check("viewport_no_cookie", &reply);
        let reply = run("viewport_bogus_cookie", viewport("", json_body(&base), &[], Some("vp_session=bogus-session"))).await;
        check("viewport_bogus_cookie", &reply);

        let variants: Vec<(&str, Value)> = vec![
            ("viewport_extra_key", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0, "extra": 1})),
            ("viewport_missing_key", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "devicePixelRatio": 2.0})),
            ("viewport_string_int", json!({"innerWidth": "1280", "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_bool_int", json!({"innerWidth": true, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_float_int", json!({"innerWidth": 1280.5, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_zero_width", json!({"innerWidth": 0, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_width_over_max", json!({"innerWidth": 32769, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_dpr_zero", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 0})),
            ("viewport_dpr_over_max", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 17})),
            ("viewport_dpr_int", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2})),
        ];
        for (name, payload) in &variants {
            let reply = run(name, viewport(name, json_body(payload), &[], Some(&owner_cookie_header))).await;
            check(name, &reply);
        }
        let nan = Body::from(
            r#"{"innerWidth":1,"innerHeight":1,"screenWidth":1,"screenHeight":1,"devicePixelRatio":NaN}"#,
        );
        let reply = run("viewport_nan_dpr", viewport("", nan, &[], Some(&owner_cookie_header))).await;
        check("viewport_nan_dpr", &reply);
        let reply = run("viewport_not_json", viewport("", Body::from("not json"), &[], Some(&owner_cookie_header))).await;
        check("viewport_not_json", &reply);

        // Password cases.
        let change = |body: Value, cookie: Option<&str>, bearer: Option<&str>| {
            let mut builder = request(Method::POST, "/api/password").header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            if let Some(token) = bearer {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            (builder, json_body(&body))
        };
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), None, None);
        check("password_no_cookie", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": "not-the-password", "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_wrong_current", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "short"}), Some(&owner_cookie_header), None);
        check("password_short_new", &send(&router, b, body).await);
        let (b, body) = change(json!({"new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_missing_current", &send(&router, b, body).await);
        // Ordering and shape cases (t3722): none changes state.
        let (b, body) = change(json!({"new_password": "short"}), None, None);
        check("password_no_cookie_invalid_body", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": "not-the-password", "new_password": "short"}), Some(&owner_cookie_header), None);
        check("password_wrong_current_short_new", &send(&router, b, body).await);
        let (b, body) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_missing_both", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": 12345678901234u64, "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_current_not_string", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "x".repeat(201)}), Some(&owner_cookie_header), None);
        check("password_new_too_long", &send(&router, b, body).await);
        let (b, _) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_not_json", &send(&router, b, Body::from("not json")).await);
        let (b, body) = change(json!([1, 2]), Some(&owner_cookie_header), None);
        check("password_list_body", &send(&router, b, body).await);

        // The API token is the scratch record's token, read from the scratch data dir only.
        let auth: Value = serde_json::from_str(&std::fs::read_to_string(data_dir.join("auth.json")).expect("scratch auth record"))
            .expect("auth record JSON");
        let api_token = auth["token"].as_str().expect("token").to_owned();
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "short"}), None, Some(&api_token));
        check("password_api_identity_invalid_body", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), None, Some(&api_token));
        check("password_api_identity", &send(&router, b, body).await);

        // A second owner session, to observe the sweep the change performs.
        let other = login(&router, PASSWORD).await;
        check("login_second_session", &other);
        let other_cookie = other.cookie.clone().expect("second session cookie");

        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_ok", &send(&router, b, body).await);

        let credentials = std::fs::read_to_string(data_dir.join("owner-credentials")).expect("credentials file");
        probes.push("password_ok_credentials_file".to_owned());
        let want_creds = case(&oracle, "password_ok_credentials_file");
        let lines: Vec<&str> = credentials.lines().collect();
        let got_creds = (
            lines.first().copied() == Some(&format!("Password: {NEW_PASSWORD}")),
            lines.iter().any(|line| line.starts_with("API token: ")),
        );
        if got_creds != (want_creds["first_line_is_new_password"].as_bool().unwrap(), want_creds["has_api_token_line"].as_bool().unwrap()) {
            extra.push(format!("password_ok_credentials_file: got {got_creds:?}"));
        }

        let current = send(&router, request(Method::GET, "/api/channels").header(header::COOKIE, owner_cookie_header.as_str()), Body::empty()).await;
        check("password_ok_probe_current_session", &current);
        let other_probe = send(&router, request(Method::GET, "/api/channels").header(header::COOKIE, other_cookie.as_str()), Body::empty()).await;
        check("password_ok_probe_other_session", &other_probe);

        check("login_new_password", &login(&router, NEW_PASSWORD).await);
        check("login_old_password", &login(&router, PASSWORD).await);

        // Ops-board answer, from the owner session (the cookie kept by the password change).
        let answer = |body: Body, cookie: Option<&str>| {
            let mut builder = request(Method::POST, "/api/ops-board/answer").header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            (builder, body)
        };
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "ok"})), None);
        check("answer_no_cookie", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!([1, 2])), Some(&owner_cookie_header));
        check("answer_list_payload", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({})), Some(&owner_cookie_header));
        check("answer_empty_payload", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "   ", "reply": "ok"})), Some(&owner_cookie_header));
        check("answer_blank_task", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1"})), Some(&owner_cookie_header));
        check("answer_missing_reply", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "   "})), Some(&owner_cookie_header));
        check("answer_blank_reply", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "ok"})), Some(&owner_cookie_header));
        check("answer_no_daemon", &send(&router, b, body).await);

        // Ops-metrics refresh: the auth gate only (the owner path calls gh and reads host state).
        let refresh = send(&router, request(Method::POST, "/api/ops-metrics/refresh"), Body::empty()).await;
        check("metrics_refresh_no_cookie", &refresh);

        // Known gaps must be exactly the divergent cases, no more and no fewer.
        let mut expected: Vec<&str> = KNOWN_GAPS.iter().map(|gap| gap.name).collect();
        expected.sort_unstable();
        diverged.sort();
        diverged.dedup();
        failures.extend(extra);
        // Replay inventory (reviewer L6): every oracle case is replayed exactly once, and nothing else.
        replayed.extend(probes);
        let mut replay_want: Vec<String> = oracle["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .map(|case| case["name"].as_str().expect("case name").to_owned())
            .collect();
        replay_want.sort();
        replayed.sort();
        assert_eq!(replayed, replay_want, "the replayed cases must be exactly the oracle's cases");
        assert!(failures.is_empty(), "t3721 parity differs from the Python oracle:\n{}", failures.join("\n"));
        assert_eq!(
            diverged.iter().map(String::as_str).collect::<Vec<_>>(),
            expected,
            "the divergent cases changed; update KNOWN_GAPS only with an owner-scoped follow-up"
        );
    });
}
