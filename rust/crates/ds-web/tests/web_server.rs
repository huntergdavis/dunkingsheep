//! Loopback acceptance for the composed desk (`src/web_server.rs`). Every test boots the real
//! route assembly on 127.0.0.1 with a temporary data directory, a temporary built UI, and a daemon
//! socket that is deliberately absent, so no route can succeed by reaching a daemon. Nothing
//! touches a live service, a credential or the network, and Python is never run.

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use ds_web::{
    ops_board_runtime::OpenDb,
    web_server::{self, DeskConfig, DeskError},
    RequestScheme,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "correct horse battery";
const INDEX: &str = "<!doctype html><title>desk</title>";

/// The operations database, opened with the accepted schema for the board's reads.
fn open_operations_db(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!("../src/ops_schema.sql"))?;
    Ok(conn)
}

fn one_channel() -> Vec<Value> {
    vec![json!({
        "id": "cos",
        "name": "Chief of staff",
        "match": "cos",
        "kind": "active"
    })]
}

/// A temporary desk: a built UI with an index and one asset, and every path under the temp root.
fn fixture() -> (TempDir, DeskConfig) {
    let root = TempDir::new().expect("temporary desk root");
    let dir = root.path();
    std::fs::create_dir_all(dir.join("ui/dist/assets")).unwrap();
    std::fs::create_dir_all(dir.join("ui/dist/assets/docs")).unwrap();
    std::fs::write(dir.join("ui/dist/index.html"), INDEX).unwrap();
    std::fs::write(dir.join("ui/dist/assets/docs/index.html"), "<h1>docs</h1>").unwrap();
    std::fs::create_dir_all(dir.join("ui/dist/assets/café")).unwrap();
    std::fs::write(dir.join("ui/dist/assets/café/index.html"), "<h1>cafe</h1>").unwrap();
    std::fs::write(dir.join("ui/dist/404.html"), "<h1>missing page</h1>").unwrap();
    std::fs::write(dir.join("ui/dist/assets/app.js"), "console.log('desk');").unwrap();
    std::fs::create_dir_all(dir.join("config")).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let config = DeskConfig {
        data_dir: dir.join("desk"),
        config_dir: dir.join("config"),
        home: dir.join("home"),
        // No daemon listens here: daemon-backed routes must fail closed, never succeed.
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
        open_operations_db: open_operations_db as OpenDb,
    };
    (root, config)
}

/// A desk running on a background thread with its own runtime, stopped through `shutdown`.
struct Server {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: mpsc::Receiver<Result<(), String>>,
}

impl Server {
    fn start(config: DeskConfig) -> Self {
        let (addr_tx, addr_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let result = runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("loopback listener");
                addr_tx
                    .send(listener.local_addr().expect("local address"))
                    .expect("address handed back");
                web_server::serve(config, listener, async {
                    let _ = stop_rx.await;
                })
                .await
                .map_err(|error: DeskError| error.to_string())
            });
            let _ = done_tx.send(result);
        });
        let addr = addr_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the desk bound a loopback address");
        Server {
            addr,
            stop: Some(stop_tx),
            done: done_rx,
        }
    }

    /// Stops the desk and returns what `serve` returned. A hang here is a shutdown defect.
    fn shutdown(mut self) -> Result<(), String> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.done
            .recv_timeout(Duration::from_secs(20))
            .expect("the desk stopped after shutdown")
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("JSON body ({error}): {}", self.text()))
    }

    fn session_cookie(&self) -> Option<String> {
        self.headers
            .iter()
            .filter(|(key, _)| key == "set-cookie")
            .find_map(|(_, value)| {
                value
                    .strip_prefix("vp_session=")
                    .map(|rest| rest.split(';').next().unwrap_or_default().to_owned())
            })
    }
}

fn decode_chunked(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .expect("chunk size line");
        let size_text = String::from_utf8_lossy(&rest[..line_end]).into_owned();
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .expect("chunk size");
        rest = &rest[line_end + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

fn parse(raw: &[u8]) -> Response {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response header terminator");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .expect("status code");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let rest = &raw[split + 4..];
    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.contains("chunked"));
    let body = if chunked {
        decode_chunked(rest)
    } else {
        rest.to_vec()
    };
    Response {
        status,
        headers,
        body,
    }
}

/// One HTTP/1.1 request over a fresh loopback connection. The server closes after each reply, so
/// reading to end-of-stream is the whole response.
fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Response {
    let mut stream = TcpStream::connect(addr).expect("connect to the desk");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(payload) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    head.push_str("\r\n");
    if let Some(payload) = body {
        head.push_str(payload);
    }
    stream.write_all(head.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse(&raw)
}

fn login(addr: SocketAddr) -> String {
    let body = json!({"password": PASSWORD, "next": "/dash/cos"}).to_string();
    let response = request(addr, "POST", "/api/login", &[], Some(&body));
    assert_eq!(response.status, 200, "login: {}", response.text());
    assert_eq!(response.json()["ok"], true);
    response
        .session_cookie()
        .expect("login sets the vp_session cookie")
}

#[test]
fn the_bundle_and_its_assets_are_served_without_a_session() {
    let (_root, config) = fixture();
    let server = Server::start(config);

    let index = request(server.addr, "GET", "/", &[], None);
    assert_eq!(index.status, 200);
    assert!(index.text().contains("<title>desk</title>"));
    assert!(index
        .header("content-type")
        .unwrap()
        .starts_with("text/html"));

    let asset = request(server.addr, "GET", "/assets/app.js", &[], None);
    assert_eq!(asset.status, 200);
    assert_eq!(asset.text(), "console.log('desk');");
    assert!(asset
        .header("content-type")
        .unwrap()
        .starts_with("text/javascript"));

    let head = request(server.addr, "HEAD", "/", &[], None);
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(
        head.header("content-length"),
        Some(INDEX.len().to_string().as_str())
    );

    // A missing file serves the bundle's 404.html, as Starlette's HTML mode does.
    let missing = request(server.addr, "GET", "/assets/missing.js", &[], None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.text(), "<h1>missing page</h1>");

    // The legacy shell catches /docs before the static mount, as it does in Python.
    let retired = request(server.addr, "GET", "/docs", &[], None);
    assert_eq!(retired.status, 307);
    assert_eq!(retired.header("location"), Some("/dash/cos"));

    // A directory with an index under /assets redirects a URL without a trailing slash, to the
    // absolute slash URL that Starlette builds.
    let directory = request(server.addr, "GET", "/assets/docs", &[], None);
    assert_eq!(directory.status, 307);
    let expected = format!("http://{}/assets/docs/", server.addr);
    assert_eq!(directory.header("location"), Some(expected.as_str()));
    let served = request(server.addr, "GET", "/assets/docs/", &[], None);
    assert_eq!(served.status, 200);
    assert_eq!(served.text(), "<h1>docs</h1>");

    // A non-ASCII directory name redirects to a percent-encoded, ASCII Location.
    let accented = request(server.addr, "GET", "/assets/caf%C3%A9", &[], None);
    assert_eq!(accented.status, 307);
    let accented_location = format!("http://{}/assets/caf%C3%A9/", server.addr);
    assert_eq!(
        accented.header("location"),
        Some(accented_location.as_str())
    );

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn legacy_bookmarks_redirect_into_the_dash_shell_before_routing() {
    let (_root, config) = fixture();
    let server = Server::start(config);
    let cases = [
        ("/poolrad?x=1", "/dash/poolrad"),
        ("/operations", "/dash/operations"),
        ("/play", "/dash/play"),
        ("/cos", "/dash/cos"),
        ("/desk", "/dash/cos"),
        ("/vp", "/dash/cos"),
        ("/fleet", "/dash/fleet"),
        ("/poolrad/", "/dash/cos"),
        ("/index.html", "/dash/cos"),
        ("/unknown/page", "/dash/cos"),
    ];
    for (target, location) in cases {
        let response = request(server.addr, "GET", target, &[], None);
        assert_eq!(response.status, 307, "{target}");
        assert_eq!(response.header("location"), Some(location), "{target}");
    }
    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn dash_routes_apply_the_session_gate_and_the_shell_redirects() {
    let (_root, config) = fixture();
    let server = Server::start(config);
    let addr = server.addr;

    let root = request(addr, "GET", "/dash", &[], None);
    assert_eq!(root.status, 307);
    assert_eq!(root.header("location"), Some("/dash/cos"));

    // Without a session a dashboard path goes to the password page with a return target.
    let anonymous = request(addr, "GET", "/dash/fleet", &[], None);
    assert_eq!(anonymous.status, 302);
    assert_eq!(anonymous.header("location"), Some("/?next=%2Fdash%2Ffleet"));

    // A trailing slash redirects before the session check, as the Python route does.
    let slashed = request(addr, "GET", "/dash/fleet/", &[], None);
    assert_eq!(slashed.status, 307);
    assert_eq!(slashed.header("location"), Some("/dash/fleet"));

    let cookie = login(addr);
    let cookie_header = format!("vp_session={cookie}");
    let session = [("Cookie", cookie_header.as_str())];

    let shell = request(addr, "GET", "/dash/fleet", &session, None);
    assert_eq!(shell.status, 200);
    assert!(shell.text().contains("<title>desk</title>"));
    assert_eq!(shell.header("cache-control"), Some("no-store"));
    assert_eq!(shell.header("x-content-type-options"), Some("nosniff"));

    let poolrad = request(addr, "GET", "/dash/poolrad", &session, None);
    assert_eq!(poolrad.status, 307);
    assert_eq!(poolrad.header("location"), Some("/dash/operations#poolrad"));

    let logout = request(addr, "POST", "/api/logout", &session, None);
    assert_eq!(logout.status, 200);
    let after = request(addr, "GET", "/dash/fleet", &session, None);
    assert_eq!(
        after.status, 302,
        "a logged-out cookie no longer opens the shell"
    );

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn api_fallbacks_and_traversal_follow_the_static_mount() {
    let (root, config) = fixture();
    let server = Server::start(config);
    let addr = server.addr;

    // A missing path under /api serves the bundle's 404.html, as Starlette's HTML mode does.
    let unknown = request(addr, "GET", "/api/nope", &[], None);
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.text(), "<h1>missing page</h1>");

    // Without a bundle 404.html the same miss answers Starlette's JSON 404.
    std::fs::remove_file(root.path().join("ui/dist/404.html")).unwrap();
    let bare = request(addr, "GET", "/api/nope", &[], None);
    assert_eq!(bare.status, 404);
    assert_eq!(bare.json(), json!({"detail": "Not Found"}));

    let wrong_method = request(addr, "POST", "/api/nope", &[], Some("{}"));
    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.header("allow"), Some("GET, HEAD"));
    assert_eq!(wrong_method.json(), json!({"detail": "Method Not Allowed"}));

    // A wrong method on a registered route is not a route either: Starlette's mount answers it.
    let wrong_on_route = request(addr, "POST", "/api/health", &[], Some("{}"));
    assert_eq!(wrong_on_route.status, 405, "{}", wrong_on_route.text());
    assert_eq!(wrong_on_route.header("allow"), Some("GET, HEAD"));
    assert_eq!(
        wrong_on_route.json(),
        json!({"detail": "Method Not Allowed"})
    );

    // HEAD is not a method of the GET-only route, so the mount looks the path up and misses.
    let head_on_route = request(addr, "HEAD", "/api/health", &[], None);
    assert_eq!(head_on_route.status, 404, "{}", head_on_route.text());

    // Routing sees the decoded path, and `..` inside the bundle normalises as in Starlette.
    let decoded_route = request(addr, "GET", "/%61pi/health", &[], None);
    assert_eq!(decoded_route.status, 200, "{}", decoded_route.text());
    let normalised = request(addr, "GET", "/assets/../assets/app.js", &[], None);
    assert_eq!(normalised.status, 200);

    let encoded = request(addr, "GET", "/assets/%2e%2e/%2e%2e/etc/passwd", &[], None);
    assert_eq!(
        encoded.status, 404,
        "an encoded dot-dot never leaves the bundle"
    );

    let raw = request(addr, "GET", "/assets/../../../etc/passwd", &[], None);
    assert_ne!(
        raw.status, 200,
        "a raw dot-dot never serves a file outside the bundle"
    );

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn public_routes_answer_without_auth_and_protected_routes_fail_closed() {
    let (_root, config) = fixture();
    let server = Server::start(config);
    let addr = server.addr;

    let health = request(addr, "GET", "/api/health", &[], None);
    assert_eq!(health.status, 200, "{}", health.text());
    let version = request(addr, "GET", "/api/version", &[], None);
    assert_eq!(version.status, 200, "{}", version.text());

    let anonymous = request(addr, "GET", "/api/channels", &[], None);
    assert_eq!(anonymous.status, 401);

    // Signed in, but the daemon is absent. Python's /api/channels catches the daemon outage and
    // still answers 200 with daemon false, no panes, and every channel offline. The Rust route
    // must do the same: the roster says the daemon is down, and nothing reads as online.
    let cookie = login(addr);
    let cookie_header = format!("vp_session={cookie}");
    let session = [("Cookie", cookie_header.as_str())];
    let channels = request(addr, "GET", "/api/channels", &session, None);
    assert_eq!(channels.status, 200, "{}", channels.text());
    let roster = channels.json();
    assert_eq!(roster["daemon"], false);
    assert_eq!(roster["viewports"], json!([]));
    assert_eq!(roster["channels"][0]["id"], "cos");
    assert_eq!(roster["channels"][0]["online"], false);
    assert_eq!(roster["channels"][0]["status"], "offline");

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn an_absent_daemon_leaves_the_board_unavailable_and_never_zero() {
    let (_root, config) = fixture();
    let server = Server::start(config);
    let addr = server.addr;
    let cookie = login(addr);
    let cookie_header = format!("vp_session={cookie}");
    let session = [("Cookie", cookie_header.as_str())];

    let board = request(addr, "GET", "/api/ops-board", &session, None);
    assert_eq!(board.status, 200, "{}", board.text());
    let body = board.json();
    assert_eq!(body["no_model"], true);
    let backlog = &body["brief"]["backlog"];
    assert_eq!(backlog["state"], "unavailable");
    assert_eq!(
        backlog["open"],
        Value::Null,
        "an unreadable backlog is null, never zero"
    );
    assert_eq!(body["brief"]["lanes"]["state"], "unavailable");
    assert_eq!(body["brief"]["lanes"]["working"], Value::Null);

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn the_channel_registry_is_validated_like_the_python_loader() {
    let root = TempDir::new().unwrap();
    let registry = root.path().join("channels.json");
    let check = |payload: &str| -> Result<Vec<Value>, DeskError> {
        std::fs::write(&registry, payload).unwrap();
        web_server::load_channel_registry(Some(registry.as_path()), root.path())
    };

    let valid = check(r#"[{"id":"cos","name":"CoS","match":"cos","kind":"active"}]"#).unwrap();
    assert_eq!(valid.len(), 1);

    let message = |result: Result<Vec<Value>, DeskError>| result.unwrap_err().to_string();
    assert!(message(check("[]")).ends_with("expected a nonempty list of channels"));
    assert!(message(check(
        r#"[{"id":"Bad","name":"x","match":"x","kind":"active"}]"#
    ))
    .ends_with("invalid channel id 'Bad'"));
    assert!(message(check(
        r#"[{"id":"a","name":"x","match":"x","kind":"busy"}]"#
    ))
    .contains("invalid kind for channel"));
    assert!(message(check(
        r#"[{"id":"a","name":"x","match":"x","kind":"active"},{"id":"a","name":"y","match":"y","kind":"active"}]"#
    ))
    .contains("duplicate channel id"));
    assert!(message(check(
        r#"[{"id":"a","name":"x","match":"x","kind":"active","dashboard":"yes"}]"#
    ))
    .contains("dashboard must be true or false"));
    assert!(message(check(r#"[{"id":"a","name":""}]"#))
        .ends_with("each channel needs nonempty id, name, match, and kind"));

    // An explicit override that does not exist is an error that names the fix.
    let missing = root.path().join("absent.json");
    let error =
        web_server::load_channel_registry(Some(missing.as_path()), root.path()).unwrap_err();
    let text = error.to_string();
    assert!(text.starts_with("VP_CHANNELS_FILE does not exist:"));
    assert!(text.contains("(cp <checkout>/channels.json "));
    assert!(text.ends_with("fall back to bridges/channels.example.json."));

    // Without an override the owner's file is used, and the checked-in example is the fallback.
    std::fs::remove_file(&registry).unwrap();
    let fallback_root = TempDir::new().unwrap();
    std::fs::create_dir_all(fallback_root.path().join("bridges")).unwrap();
    std::fs::write(
        fallback_root.path().join("bridges/channels.example.json"),
        r#"[{"id":"example","name":"Example","match":"example","kind":"monitor"}]"#,
    )
    .unwrap();
    let example = web_server::load_channel_registry(None, fallback_root.path()).unwrap();
    assert_eq!(example[0]["id"], "example");
}

#[test]
fn fleet_routes_fail_closed_without_a_daemon() {
    let (_root, mut config) = fixture();
    // An operations database that cannot be opened: the metrics route must refuse, never zero.
    let unopenable = config.operations_db.clone();
    std::fs::create_dir_all(&unopenable).unwrap();
    config.operations_db = unopenable;
    let server = Server::start(config);
    let addr = server.addr;

    // The fleet view is public, as in Python. With no daemon it answers an unavailable roster.
    let fleet = request(addr, "GET", "/api/fleet", &[], None);
    assert_eq!(fleet.status, 200, "{}", fleet.text());
    assert!(fleet.text().contains("unavailable"), "{}", fleet.text());

    // Sending to a pane needs a session, and with no daemon it never reports a delivery.
    let payload = r#"{"pane":"%9","text":"hi"}"#;
    let anonymous = request(addr, "POST", "/api/pane-send", &[], Some(payload));
    assert_eq!(anonymous.status, 401);
    let cookie = login(addr);
    let cookie_header = format!("vp_session={cookie}");
    let session = [("Cookie", cookie_header.as_str())];
    let send = request(addr, "POST", "/api/pane-send", &session, Some(payload));
    assert!(matches!(send.status, 502 | 503), "{}", send.text());
    assert!(send.json()["detail"].is_string());

    // Operations metrics need a session, and refuse when the database cannot be read.
    let metrics_anonymous = request(addr, "GET", "/api/ops-metrics", &[], None);
    assert_eq!(metrics_anonymous.status, 401);
    let metrics = request(addr, "GET", "/api/ops-metrics", &session, None);
    assert_eq!(metrics.status, 503, "{}", metrics.text());
    assert!(metrics.json()["detail"]
        .as_str()
        .unwrap_or_default()
        .starts_with("Operations metrics unavailable"));

    assert_eq!(server.shutdown(), Ok(()));
}

#[test]
fn shutdown_stops_the_background_work_and_returns_cleanly() {
    let (_root, config) = fixture();
    let server = Server::start(config);
    let addr = server.addr;
    let served = request(addr, "GET", "/", &[], None);
    assert_eq!(served.status, 200);
    // A shutdown that completes within the timeout proves the timer and usage workers joined.
    assert_eq!(server.shutdown(), Ok(()));
}

/// The Python routes the table is checked against: every decorator in `web/app.py`, then the
/// router `app.include_router` adds from `web/character_editor.py`, under that router's prefix.
/// The sources are read as text, and Python is never run.
fn python_routes() -> Vec<(String, String)> {
    const APP: &str = include_str!("../../../../web/app.py");
    const EDITOR: &str = include_str!("../../../../web/character_editor.py");
    assert!(
        EDITOR.contains("prefix='/api/character-editor'"),
        "web/character_editor.py no longer declares the prefix this check reads"
    );
    let mut routes = decorated_routes(APP, "app", "");
    routes.extend(decorated_routes(EDITOR, "routes", "/api/character-editor"));
    assert!(
        routes
            .iter()
            .any(|(_, path)| path.starts_with("/api/character-editor/")),
        "no character editor route was read from web/character_editor.py"
    );
    routes
}

/// The `(method, path)` of each `@<receiver>.<method>("<path>", ...)` decorator in `source`, with
/// `prefix` in front of the path. Both quote styles are read, because the Python files use both.
fn decorated_routes(source: &str, receiver: &str, prefix: &str) -> Vec<(String, String)> {
    const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    let marker = format!("@{receiver}.");
    source
        .lines()
        .filter_map(|line| {
            let (method, args) = line
                .trim_start()
                .strip_prefix(marker.as_str())?
                .split_once('(')?;
            let method = method.to_ascii_uppercase();
            if !METHODS.contains(&method.as_str()) {
                return None;
            }
            let quote = args.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let path = args[1..].split(quote).next()?;
            Some((method, format!("{prefix}{path}")))
        })
        .collect()
}

/// A request path that `pattern` matches. Each `{name}` becomes one segment and each `{name:path}`
/// becomes two, so the path converter has something to match.
fn concrete_path(pattern: &str) -> String {
    let segments: Vec<&str> = pattern
        .split('/')
        .map(|segment| {
            match segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
            {
                Some(name) if name.ends_with(":path") => "probe/nested",
                Some(_) => "probe",
                None => segment,
            }
        })
        .collect();
    segments.join("/")
}

/// What the router answered for one probe request.
struct Probe {
    status: StatusCode,
    allow: String,
    body: Vec<u8>,
}

/// Sends one request with no session and no body. `PROBE` is registered by no route, so a matched
/// path answers with the router's own 405 and an unmatched path reaches the static mount. No
/// handler runs either way.
async fn probe(router: &Router, method: &str, path: &str) -> Probe {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .expect("probe request");
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("the router does not fail");
    let status = response.status();
    let allow = response
        .headers()
        .get(header::ALLOW)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("probe body")
        .to_vec();
    Probe {
        status,
        allow,
        body,
    }
}

/// The methods the mounted routes at a path answer, from the `Allow` the router sets on its 405.
/// An unmatched path reaches the static mount, whose 405 is the same for every path, so that
/// answer means no route is mounted there.
fn mounted_methods(unmatched: &Probe, answer: &Probe) -> Vec<String> {
    if answer.status == unmatched.status && answer.body == unmatched.body {
        return Vec::new();
    }
    answer
        .allow
        .split(',')
        .map(|method| method.trim().to_ascii_uppercase())
        .filter(|method| !method.is_empty())
        .collect()
}

/// `PYTHON_ROUTES` and the mounted routes must agree both ways, and the table must be Python's.
/// The router is probed directly, below the path gate, so each route is judged by what the router
/// mounts. HEAD is not compared: axum answers HEAD for every GET route, and FastAPI's APIRoute does
/// not. `POST /api/ops-board/question` is mounted but not declared by Python, so this check does
/// not cover it (t3328).
#[test]
fn python_routes_match_the_mounted_routes() {
    let (_root, config) = fixture();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    let failures = runtime.block_on(async {
        // Built inside the runtime, as `serve` builds it, so the router sees a Tokio context.
        let desk = web_server::Desk::open(config)
            .unwrap_or_else(|error| panic!("the desk opens: {error}"));
        let router = desk.router();
        let unmatched = probe(&router, "PROBE", "/api/__not_a_route__").await;
        assert_eq!(unmatched.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(unmatched.allow, "GET, HEAD");

        let python = python_routes();
        let table = web_server::PYTHON_ROUTES;
        let mut failures = Vec::new();

        for &(method, pattern) in table {
            let declared = python
                .iter()
                .any(|(m, p)| m.as_str() == method && p.as_str() == pattern);
            if !declared {
                failures.push(format!(
                    "{method} {pattern} is in PYTHON_ROUTES, but web/app.py does not declare it"
                ));
            }
        }

        for &(method, pattern) in table {
            let answer = probe(&router, "PROBE", &concrete_path(pattern)).await;
            let methods = mounted_methods(&unmatched, &answer);
            if !methods.iter().any(|mounted| mounted.as_str() == method) {
                failures.push(format!(
                    "{method} {pattern} has no mounted route (status {}, Allow {methods:?})",
                    answer.status
                ));
            }
        }

        for (method, pattern) in &python {
            let listed = table
                .iter()
                .any(|&(m, p)| m == method.as_str() && p == pattern.as_str());
            if listed {
                continue;
            }
            let answer = probe(&router, "PROBE", &concrete_path(pattern)).await;
            let methods = mounted_methods(&unmatched, &answer);
            if methods
                .iter()
                .any(|mounted| mounted.as_str() == method.as_str())
            {
                failures.push(format!(
                    "{method} {pattern} is mounted, but PYTHON_ROUTES does not list it"
                ));
            }
        }
        failures
    });
    assert!(
        failures.is_empty(),
        "PYTHON_ROUTES and the mounted routes disagree:\n{}",
        failures.join("\n")
    );
}
