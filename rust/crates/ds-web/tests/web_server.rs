//! Loopback acceptance for the composed desk (`src/web_server.rs`). Every test boots the real
//! route assembly on 127.0.0.1 with a temporary data directory, a temporary built UI, and a daemon
//! socket that is deliberately absent, so no route can succeed by reaching a daemon. The happy-path
//! tests for the fleet, pane-send and operations-metrics routes are the exception: they point the
//! desk at a fake daemon socket that answers only the commands those routes need, and they assert
//! the response shapes that `web/app.py` and `bridges/ops_metrics.py` produce. Nothing touches a
//! live service, a credential or the network, and Python is never run.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
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
use serde_json::{json, Map, Value};
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

/// A command the desk sent to the fake daemon, with its arguments and the service token it sent.
#[derive(Clone, Debug)]
struct Sent {
    command: String,
    args: Map<String, Value>,
    token: Option<String>,
}

/// A local Unix-socket daemon for the happy-path tests. It answers one JSON line per connection, as
/// the daemon does, with `answer`'s result or its error, and it records every command it reads.
struct FakeDaemon {
    socket: PathBuf,
    sent: Arc<Mutex<Vec<Sent>>>,
}

impl FakeDaemon {
    fn start(dir: &Path, answer: fn(&str, &Map<String, Value>) -> Result<Value, String>) -> Self {
        let socket = dir.join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind the fake daemon socket");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&sent);
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
                let command = request["cmd"].as_str().unwrap_or_default().to_owned();
                let args = request["args"].as_object().cloned().unwrap_or_default();
                log.lock().unwrap().push(Sent {
                    command: command.clone(),
                    args: args.clone(),
                    token: request["service_token"].as_str().map(str::to_owned),
                });
                let reply = match answer(&command, &args) {
                    Ok(result) => json!({"id": request["id"], "ok": true, "result": result}),
                    Err(error) => json!({"id": request["id"], "ok": false, "error": error}),
                };
                let _ = writeln!(stream, "{reply}");
            }
        });
        FakeDaemon { socket, sent }
    }

    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

/// The lanes the fake daemon knows. `%7` is a guarded lane with a registry name and three unread
/// messages; `%9` is an owner-push lane with a label and two pending pushes; `%10` is a lane whose
/// capability says it is not alive; `%8` is a shell, and a shell is not a lane.
fn lanes_answer(command: &str, args: &Map<String, Value>) -> Result<Value, String> {
    let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
    match command {
        "list_panes" => Ok(json!({"panes": [
            {"pane_id": "%7", "agent": "claude", "cwd": "/work/alpha/"},
            {"pane_id": "%8", "agent": "", "cwd": "/work/shell"},
            {"pane_id": "%9", "agent": "pi", "cwd": "/work/beta", "label": "beta-label"},
            {"pane_id": "%10", "agent": "codex", "cwd": "/work/gamma"},
        ]})),
        "state_latest" => Ok(json!({"rows": [{"pane_id": "%7", "session_id": "sid-alpha"}]})),
        // Python keys a lane's unread count by its name and by its pane id, and a row adds both
        // lookups. This lane names only its name, so its three messages are counted once.
        "roster" => Ok(json!({"lanes": [
            {"name": "alpha-lane", "inbox": {"unread": 3}},
        ]})),
        "delivery_capability" => match args.get("target").and_then(Value::as_str) {
            Some("%7") => Ok(json!({"alive": true, "send": "guarded", "drains": false})),
            Some("%9") => Ok(json!({"alive": true, "send": "owner_push", "drains": true})),
            Some("%10") => Ok(json!({"alive": false, "send": null, "drains": false})),
            _ => Err("no such pane".to_owned()),
        },
        "owner_stats" => Ok(json!({"ok": true, "pending": [
            {"pane_id": "%9"}, {"pane_id": "%9"}, {"pane_id": "%7"},
        ]})),
        "send_text" => Ok(match text {
            "queue this" => {
                json!({"outcome": "queued", "message_id": 42, "message": "held by the typing guard"})
            }
            "refuse this" => {
                json!({"outcome": "refused", "message": "lane is not accepting input"})
            }
            _ => json!({"outcome": "sent", "message_id": 41, "message": "typed into the box"}),
        }),
        "owner_push" => Ok(json!({"outcome": "stored", "id": 5, "message": "stored for the lane"})),
        other => Err(format!("unexpected command {other}")),
    }
}

/// The keys of a JSON object, compared as a set, so a missing or an extra field fails with both.
fn assert_keys(value: &Value, expected: &[&str]) {
    let mut actual: Vec<&str> = value
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    actual.sort_unstable();
    let mut wanted = expected.to_vec();
    wanted.sort_unstable();
    assert_eq!(actual, wanted);
}

/// Python's `time.strftime("%Y-%m-%dT%H:%M:%SZ")`: twenty characters with the separators in place.
fn assert_stamp(value: &Value) {
    let text = value.as_str().expect("a timestamp string");
    let shaped = text.len() == 20
        && text.ends_with('Z')
        && text.as_bytes()[10] == b'T'
        && text.chars().filter(char::is_ascii_digit).count() == 14;
    assert!(shaped, "not a daemon stamp: {text}");
}

const FLEET_KEYS: &[&str] = &["count", "generated_at", "note", "rows", "state", "unrouted"];

const LANE_ROW_KEYS: &[&str] = &[
    "agent",
    "alive",
    "can_type",
    "cwd",
    "drains",
    "last_activity_s",
    "name",
    "pane_id",
    "pending",
    "pending_state",
    "project",
    "send",
    "session",
    "unread",
];

const OPS_SUMMARY_KEYS: &[&str] = &[
    "generated_at",
    "project",
    "latest_sample_at",
    "latest_sample_age_s",
    "sources",
    "source_freshness",
    "staff",
    "pane_roster",
    "quota",
    "quotas",
    "subscriptions",
    "monthly_fixed_usd",
    "fixed_subscription_usd_per_calendar_hour",
    "calendar_hours_in_month",
    "billing_month",
    "usage_cost_usd",
    "opencode_usage",
    "host_health",
    "goals",
    "function_progress",
    "completion_rate",
    "tracked_recent_prs",
    "vp_message_counts",
    "dunk_metrics",
    "experiment_lab",
    "limitations",
];

/// GET /api/fleet answers the Python payload: the envelope, one row per lane in project order, and
/// the same fourteen row keys. The fake daemon supplies the roster, the capabilities and the pending
/// pushes. The lane registry and the transcript are temporary files.
#[test]
fn fleet_lists_the_lanes_in_the_python_shape() {
    let (root, mut config) = fixture();
    let dir = root.path();
    let daemon = FakeDaemon::start(dir, lanes_answer);
    config.daemon_socket = daemon.socket.clone();
    std::fs::write(
        dir.join("config/lanes.json"),
        json!({"%7": "alpha-lane", "%10": {"mailbox": "gamma-box", "alive": false}}).to_string(),
    )
    .unwrap();
    let transcript = dir.join("home/.claude/projects/-work-alpha/sid-alpha.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&transcript)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(120))
        .unwrap();

    let server = Server::start(config);
    let addr = server.addr;
    let cookie = format!("vp_session={}", login(addr));
    let session = [("Cookie", cookie.as_str())];
    let fleet = request(addr, "GET", "/api/fleet", &session, None);
    assert_eq!(fleet.status, 200, "{}", fleet.text());
    let body = fleet.json();
    assert_keys(&body, FLEET_KEYS);
    assert_eq!(body["state"], "ok");
    assert_eq!(
        body["note"],
        "shells and dead panes are not lanes and are not listed"
    );
    assert_stamp(&body["generated_at"]);
    // No tmux exists in the fixture, so the inventory reports itself unavailable, with Python's
    // fields and no rows.
    assert_eq!(
        body["unrouted"],
        json!({"state": "unavailable", "rows": [], "reason": "Process or pane inventory unavailable"})
    );

    let rows = body["rows"].as_array().expect("rows is a list");
    assert_eq!(
        body["count"].as_u64(),
        Some(3),
        "the shell %8 is not a lane: {body}"
    );
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert_keys(row, LANE_ROW_KEYS);
    }

    let alpha = &rows[0];
    assert_eq!(alpha["pane_id"], "%7");
    assert_eq!(alpha["name"], "alpha-lane");
    assert_eq!(alpha["project"], "alpha");
    assert_eq!(alpha["cwd"], "/work/alpha/");
    assert_eq!(alpha["agent"], "claude");
    assert_eq!(alpha["alive"], true);
    assert_eq!(alpha["unread"], 3);
    assert_eq!(alpha["session"], "sid-alpha");
    assert_eq!(alpha["send"], "guarded");
    assert_eq!(alpha["drains"], false);
    assert_eq!(alpha["pending"], 1);
    assert_eq!(alpha["pending_state"], "ok");
    assert_eq!(alpha["can_type"], true);
    let idle = alpha["last_activity_s"]
        .as_i64()
        .expect("the transcript gives an age");
    assert!((115..=140).contains(&idle), "age {idle}");

    let beta = &rows[1];
    assert_eq!(beta["pane_id"], "%9");
    assert_eq!(beta["name"], "beta-label");
    assert_eq!(beta["project"], "beta");
    assert_eq!(beta["agent"], "pi");
    assert_eq!(beta["unread"], 0);
    assert_eq!(beta["send"], "owner_push");
    assert_eq!(beta["drains"], true);
    assert_eq!(beta["pending"], 2);
    assert_eq!(beta["can_type"], false);
    assert!(beta["session"].is_null());
    assert!(beta["last_activity_s"].is_null());

    let gamma = &rows[2];
    assert_eq!(gamma["pane_id"], "%10");
    assert_eq!(gamma["name"], "gamma-box");
    assert_eq!(gamma["project"], "gamma");
    assert_eq!(gamma["alive"], true);
    assert!(gamma["send"].is_null());
    assert_eq!(gamma["pending"], 0);
    assert_eq!(gamma["pending_state"], "ok");
    assert_eq!(gamma["can_type"], false);

    assert_eq!(server.shutdown(), Ok(()));
}

/// POST /api/pane-send to a guarded lane answers with the Python outcome shapes: `sent` and `queued`
/// are ok (`queued` also sets its flag), and `refused` is not ok. Each text reaches the daemon as
/// `send_text` for its target, under the desk's service token.
#[test]
fn pane_send_to_a_guarded_lane_answers_with_the_python_outcomes() {
    let (root, mut config) = fixture();
    let daemon = FakeDaemon::start(root.path(), lanes_answer);
    config.daemon_socket = daemon.socket.clone();
    let server = Server::start(config);
    let addr = server.addr;
    let cookie = format!("vp_session={}", login(addr));
    let session = [("Cookie", cookie.as_str())];
    let send = |text: &str| {
        let body = json!({"pane": "%7", "text": text}).to_string();
        request(addr, "POST", "/api/pane-send", &session, Some(&body))
    };

    let sent = send("hello lane");
    assert_eq!(sent.status, 200, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({
            "ok": true,
            "pane": "%7",
            "route": "guarded",
            "outcome": "sent",
            "message_id": 41,
            "detail": "typed into the box",
            "note": "Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it."
        })
    );

    let queued = send("queue this");
    assert_eq!(queued.status, 200, "{}", queued.text());
    assert_eq!(
        queued.json(),
        json!({
            "ok": true,
            "pane": "%7",
            "route": "guarded",
            "outcome": "queued",
            "queued": true,
            "message_id": 42,
            "detail": "held by the typing guard",
            "note": "Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered."
        })
    );

    let refused = send("refuse this");
    assert_eq!(refused.status, 200, "{}", refused.text());
    assert_eq!(
        refused.json(),
        json!({
            "ok": false,
            "pane": "%7",
            "route": "guarded",
            "outcome": "refused",
            "message_id": null,
            "detail": "lane is not accepting input"
        })
    );

    let typed: Vec<String> = daemon
        .sent()
        .iter()
        .filter(|call| call.command == "send_text")
        .map(|call| call.args["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(typed, ["hello lane", "queue this", "refuse this"]);
    assert!(daemon
        .sent()
        .iter()
        .all(|call| call.token.as_deref() == Some("web-desk-test-token")));
    assert_eq!(server.shutdown(), Ok(()));
}

/// POST /api/pane-send to an owner-push lane stores the message for the lane to drain, and the
/// answer says so: `stored` is not `read`, and nothing is typed into the pane.
#[test]
fn pane_send_to_an_owner_push_lane_reports_the_stored_shape() {
    let (root, mut config) = fixture();
    let daemon = FakeDaemon::start(root.path(), lanes_answer);
    config.daemon_socket = daemon.socket.clone();
    let server = Server::start(config);
    let addr = server.addr;
    let cookie = format!("vp_session={}", login(addr));
    let session = [("Cookie", cookie.as_str())];

    let body = json!({"pane": "%9", "text": "push this"}).to_string();
    let pushed = request(addr, "POST", "/api/pane-send", &session, Some(&body));
    assert_eq!(pushed.status, 200, "{}", pushed.text());
    assert_eq!(
        pushed.json(),
        json!({
            "ok": true,
            "pane": "%9",
            "route": "owner_push",
            "outcome": "stored",
            "push_id": 5,
            "detail": "stored for the lane",
            "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."
        })
    );

    let pushes: Vec<Sent> = daemon
        .sent()
        .into_iter()
        .filter(|call| call.command == "owner_push")
        .collect();
    assert_eq!(pushes.len(), 1, "one push, no typed send");
    assert_eq!(pushes[0].args["target"], "%9");
    assert_eq!(pushes[0].args["body"], "push this");
    assert_eq!(server.shutdown(), Ok(()));
}

/// GET /api/ops-metrics answers the Python `summary` dict: the same twenty-six top-level keys, with
/// the collection run, staff counts, roster and subscriptions read from a temporary operations
/// database. The fixed-cost figures follow the Python arithmetic.
#[test]
fn ops_metrics_reports_the_python_summary_shape() {
    let (root, mut config) = fixture();
    let daemon = FakeDaemon::start(root.path(), lanes_answer);
    config.daemon_socket = daemon.socket.clone();
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let conn = open_operations_db(&config.operations_db).unwrap();
    conn.execute_batch(
        "INSERT INTO collection_runs (id, observed_at, panes_state, github_state, vp_messages_state,
            dunk_metrics_state, accounting_state, agent_messages_state, errors_json)
         VALUES (1, '2026-11-06T10:00:00Z', 'ok', 'ok', 'ok', 'ok', 'ok', 'ok', '[]');
         INSERT INTO pane_samples (run_id, pane_id, project, agent_kind, agent_status, agent_name,
            lane_name, role)
         VALUES (1, '%7', 'poolrad', 'claude', 'working', 'alpha', 'alpha-lane', 'coder');
         INSERT INTO subscription_costs (project, provider, monthly_usd_cents, observed_at,
            evidence_ref)
         VALUES ('poolrad', 'claude', 2000, '2026-11-06T09:00:00Z', 'test:claude');",
    )
    .unwrap();
    drop(conn);

    let server = Server::start(config);
    let addr = server.addr;
    let cookie = format!("vp_session={}", login(addr));
    let session = [("Cookie", cookie.as_str())];
    let metrics = request(addr, "GET", "/api/ops-metrics", &session, None);
    assert_eq!(metrics.status, 200, "{}", metrics.text());
    let summary = metrics.json();
    assert_keys(&summary, OPS_SUMMARY_KEYS);

    assert_eq!(summary["project"], "poolrad");
    assert_eq!(summary["latest_sample_at"], "2026-11-06T10:00:00Z");
    assert_eq!(
        summary["sources"],
        json!({
            "panes": "ok",
            "github": "ok",
            "vp_messages": "ok",
            "agent_messages": "ok",
            "dunk_metrics": "ok",
            "accounting": "ok"
        })
    );
    assert_eq!(
        summary["source_freshness"]["panes"]["last_ok_at"],
        "2026-11-06T10:00:00Z"
    );
    assert_eq!(summary["staff"]["coder"]["working"], 1);
    assert_eq!(
        summary["pane_roster"],
        json!([{
            "pane_id": "%7",
            "role": "coder",
            "agent_kind": "claude",
            "agent_status": "working",
            "agent_name": "alpha",
            "lane_name": "alpha-lane"
        }])
    );

    let subscription = &summary["subscriptions"][0];
    assert_eq!(subscription["provider"], "claude");
    assert_eq!(subscription["monthly_usd"].as_f64(), Some(20.0));
    assert_eq!(summary["monthly_fixed_usd"].as_f64(), Some(20.0));
    let hours = summary["calendar_hours_in_month"]
        .as_i64()
        .expect("calendar hours");
    assert!([672, 696, 720, 744].contains(&hours), "hours {hours}");
    let hourly = summary["fixed_subscription_usd_per_calendar_hour"]
        .as_f64()
        .expect("hourly fixed cost");
    assert!(
        (hourly - 20.0 / hours as f64).abs() < 0.0001,
        "hourly {hourly}"
    );
    let generated = summary["generated_at"].as_str().expect("generated_at");
    assert_eq!(summary["billing_month"].as_str(), Some(&generated[..7]));

    assert!(summary["dunk_metrics"].is_null());
    assert!(summary["function_progress"].is_null());
    assert!(summary["completion_rate"].is_null());
    assert_eq!(summary["quotas"], json!({"codex": null, "claude": null}));
    assert_eq!(summary["goals"], json!([]));
    assert_eq!(summary["tracked_recent_prs"], json!([]));
    assert_eq!(summary["vp_message_counts"], json!([]));
    assert!(summary["limitations"].is_array());

    assert_eq!(server.shutdown(), Ok(()));
}
