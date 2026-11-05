//! Production `PanesWebBackend` for the desk's pane, channel, and viewport routes.
//!
//! Startup supplies everything this backend uses: an authenticated daemon client, the captured
//! channel list, and the explicit data directory whose `messages.sqlite3` holds the desk sessions.
//! The SQLite connection is opened once. Daemon RPC and every SQLite statement run on Tokio's
//! blocking pool, so the async executor never waits on a socket or a database lock. Nothing here
//! reads the process environment, a global store, or a socket the caller did not pass in.
//!
//! Pane authority mirrors `web/app.py::_can_message`: a pane with a truthy `pane_id` is allowed
//! when it is a lane (`roster::is_lane`), and otherwise when the daemon's `delivery_capability`
//! answers `agent_alive: true`. Answers are cached per pane for 20 seconds. Any failure to ask or
//! to answer means "not allowed" (fail closed), never a guess.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use chrono::Utc;
use ds_core::{
    client::{ClientError, FlockClient, Transport},
    error::DunkError,
};
use rusqlite::{params, types::ValueRef, Connection};
use serde_json::{json, Map, Value};

use crate::{
    auth_backend::{session_column_names, session_key, SESSION_VIEWPORT_COLUMNS},
    roster::{build_roster, is_lane, RosterError},
    routes::panes::{PanesWebBackend, PanesWebError, PanesWebFuture, ViewportSample},
};

/// The desk database file that `PersistentAuth` also opens.
const DATABASE_FILE: &str = "messages.sqlite3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// Python `_CAP_TTL_S`: how long one `delivery_capability` answer is reused.
const CAPABILITY_TTL_S: f64 = 20.0;
/// The viewport keys, in the same order as the Python `SELECT ... AS` list.
const VIEWPORT_KEYS: [&str; 6] = [
    "innerWidth",
    "innerHeight",
    "devicePixelRatio",
    "screenWidth",
    "screenHeight",
    "viewport_at",
];

type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// A startup failure to open the desk store. Per-request failures use [`PanesWebError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanesBackendOpenError(pub String);

impl std::fmt::Display for PanesBackendOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PanesBackendOpenError {}

/// The production backend: one authenticated daemon client and one desk database connection.
pub struct DaemonPanesBackend<T: Transport + Send + 'static> {
    flock: Arc<Mutex<FlockClient<T>>>,
    db: Arc<Mutex<Connection>>,
    channels: Arc<Vec<Value>>,
    capabilities: Arc<Mutex<HashMap<String, (f64, Value)>>>,
    clock: Clock,
}

impl<T: Transport + Send + 'static> DaemonPanesBackend<T> {
    /// Opens `data_dir/messages.sqlite3` once. `PersistentAuth` must already have initialized the
    /// directory, so `sessions` and its viewport columns exist; a missing one is a startup error,
    /// never a per-request surprise. Building the daemon client does not contact the daemon.
    pub fn open(
        flock: FlockClient<T>,
        data_dir: impl AsRef<Path>,
        channels: Vec<Value>,
    ) -> Result<Self, PanesBackendOpenError> {
        let db = Connection::open(data_dir.as_ref().join(DATABASE_FILE)).map_err(open_error)?;
        db.busy_timeout(BUSY_TIMEOUT).map_err(open_error)?;
        let columns = session_column_names(&db).map_err(open_error)?;
        for (name, _) in SESSION_VIEWPORT_COLUMNS {
            if !columns.iter().any(|column| column == name) {
                return Err(PanesBackendOpenError(format!(
                    "sessions.{name} is missing; initialize the desk data directory with PersistentAuth first"
                )));
            }
        }
        Ok(Self {
            flock: Arc::new(Mutex::new(flock)),
            db: Arc::new(Mutex::new(db)),
            channels: Arc::new(channels),
            capabilities: Arc::new(Mutex::new(HashMap::new())),
            clock: Arc::new(system_clock),
        })
    }

    /// Replaces the wall clock. Tests use it to exercise the capability TTL and session expiry.
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    /// Python `_capability` behind the `_can_message` cache. Returns `{}` when the daemon cannot
    /// answer, and only a dict answer is cached.
    async fn capability(&self, pane_id: &str) -> Value {
        let cached = self
            .capabilities
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(pane_id)
            .cloned();
        if let Some((seen, payload)) = cached {
            if self.now() - seen < CAPABILITY_TTL_S {
                return payload;
            }
        }
        match self
            .rpc("delivery_capability", json!({"target": pane_id}))
            .await
        {
            Ok(payload @ Value::Object(_)) => {
                self.capabilities
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(pane_id.to_owned(), (self.now(), payload.clone()));
                payload
            }
            _ => json!({}),
        }
    }

    /// Whether the daemon reports a live agent process for this pane (`agent_alive is True`).
    async fn agent_alive(&self, pane_id: &str) -> bool {
        self.capability(pane_id)
            .await
            .get("agent_alive")
            .and_then(Value::as_bool)
            == Some(true)
    }
}

impl<T: Transport + Send + 'static> PanesWebBackend for DaemonPanesBackend<T> {
    fn rpc<'a>(
        &'a self,
        method: &'a str,
        args: Value,
    ) -> PanesWebFuture<'a, Result<Value, PanesWebError>> {
        let flock = Arc::clone(&self.flock);
        let method = method.to_owned();
        Box::pin(blocking(move || {
            let Value::Object(args) = args else {
                return Err(PanesWebError::Failure(
                    "daemon arguments must be a JSON object".into(),
                ));
            };
            let mut client = flock.lock().unwrap_or_else(PoisonError::into_inner);
            client.call(&method, args).map_err(daemon_error)
        }))
    }

    fn update_viewport<'a>(
        &'a self,
        cookie: &'a str,
        sample: ViewportSample,
        now_seconds: f64,
    ) -> PanesWebFuture<'a, Result<bool, PanesWebError>> {
        let db = Arc::clone(&self.db);
        let token_hash = session_key(cookie);
        Box::pin(blocking(move || {
            let db = db.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = db
                .execute(
                    "UPDATE sessions SET viewport_width=?,viewport_height=?,viewport_dpr=?,\
                     viewport_screen_width=?,viewport_screen_height=?,viewport_at=? \
                     WHERE token_hash=? AND expires_at>?",
                    params![
                        sample.inner_width,
                        sample.inner_height,
                        sample.device_pixel_ratio,
                        sample.screen_width,
                        sample.screen_height,
                        now_seconds,
                        token_hash,
                        now_seconds,
                    ],
                )
                .map_err(storage_error)?;
            Ok(changed > 0)
        }))
    }

    fn viewports<'a>(&'a self) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>> {
        let db = Arc::clone(&self.db);
        let now = self.now();
        Box::pin(blocking(move || {
            let db = db.lock().unwrap_or_else(PoisonError::into_inner);
            let mut statement = db
                .prepare(
                    "SELECT viewport_width AS innerWidth,viewport_height AS innerHeight,\
                     viewport_dpr AS devicePixelRatio,viewport_screen_width AS screenWidth,\
                     viewport_screen_height AS screenHeight,viewport_at \
                     FROM sessions WHERE expires_at>? AND viewport_at IS NOT NULL ORDER BY rowid",
                )
                .map_err(storage_error)?;
            let rows = statement
                .query_map(params![now], |row| {
                    let mut item = Map::new();
                    for (index, key) in VIEWPORT_KEYS.iter().enumerate() {
                        item.insert((*key).to_owned(), sql_value(row.get_ref(index)?));
                    }
                    Ok(Value::Object(item))
                })
                .map_err(storage_error)?;
            let viewports = rows
                .collect::<rusqlite::Result<Vec<Value>>>()
                .map_err(storage_error)?;
            Ok(viewports)
        }))
    }

    fn roster_rows<'a>(
        &'a self,
        panes: &'a [Value],
    ) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>> {
        Box::pin(async move {
            // Pass 1 asks the roster builder which panes it will ask authority about. That set
            // depends only on the configured rows and the pane records, never on the answers, so
            // the answers can be awaited here and used by the synchronous build below.
            let asked = collect_asked(self.channels.as_slice(), panes).map_err(roster_failure)?;
            let mut decisions: HashMap<String, bool> = HashMap::new();
            for pane in &asked {
                let Some(pane_id) = truthy_pane_id(pane) else {
                    continue;
                };
                // Lanes take the fast path and never ask the daemon.
                if is_lane(Some(pane)).unwrap_or(false) || decisions.contains_key(&pane_id) {
                    continue;
                }
                let allowed = self.agent_alive(&pane_id).await;
                decisions.insert(pane_id, allowed);
            }
            build_with_policy(self.channels.as_slice(), panes, &decisions).map_err(roster_failure)
        })
    }

    fn now_seconds(&self) -> f64 {
        self.now()
    }

    fn checked_at(&self) -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }
}

/// Runs blocking daemon or SQLite work on Tokio's blocking pool and returns its result. A worker
/// that panicked or was cancelled is a failure, never an empty success.
async fn blocking<R, F>(work: F) -> Result<R, PanesWebError>
where
    R: Send + 'static,
    F: FnOnce() -> Result<R, PanesWebError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| PanesWebError::Failure(format!("blocking worker stopped: {error}")))?
}

/// Pass 1: the panes the roster builder asks authority about, in call order. Answers are ignored.
fn collect_asked(configured: &[Value], panes: &[Value]) -> Result<Vec<Value>, RosterError> {
    let mut asked = Vec::new();
    let mut record = |pane: Option<&Value>| {
        asked.extend(pane.cloned());
        false
    };
    build_roster(configured, panes, false, Some(&mut record))?;
    Ok(asked)
}

/// Pass 2: the synchronous roster build, using the answers gathered by pass 1.
fn build_with_policy(
    configured: &[Value],
    panes: &[Value],
    decisions: &HashMap<String, bool>,
) -> Result<Vec<Value>, RosterError> {
    let mut policy = |pane: Option<&Value>| can_message_policy(pane, decisions);
    build_roster(configured, panes, false, Some(&mut policy))
}

/// Python `_can_message` once the answers are known. No pane, or no pane id, is refused; a lane
/// is allowed without asking; any other pane is allowed only when its answer said so.
fn can_message_policy(pane: Option<&Value>, decisions: &HashMap<String, bool>) -> bool {
    let Some(pane) = pane else {
        return false;
    };
    let Some(pane_id) = truthy_pane_id(pane) else {
        return false;
    };
    if is_lane(Some(pane)).unwrap_or(false) {
        return true;
    }
    decisions.get(&pane_id).copied().unwrap_or(false)
}

/// Python `pane.get("pane_id")` truthiness. A non-string id is refused rather than sent on.
fn truthy_pane_id(pane: &Value) -> Option<String> {
    pane.get("pane_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// A reply that never came back because no daemon is listening is "unavailable", the same signal
/// the desk uses for a missing daemon. A refused or malformed reply is a failure.
fn daemon_error(error: ClientError) -> PanesWebError {
    match error {
        ClientError::Unavailable(message) | ClientError::Dunk(DunkError::Unavailable(message)) => {
            unavailable(message)
        }
        ClientError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            unavailable(error.to_string())
        }
        other => PanesWebError::Failure(other.to_string()),
    }
}

fn unavailable(message: String) -> PanesWebError {
    PanesWebError::Http {
        status: StatusCode::SERVICE_UNAVAILABLE,
        detail: format!("Dunking Sheep unavailable: {message}"),
    }
}

fn storage_error(error: rusqlite::Error) -> PanesWebError {
    PanesWebError::Failure(format!("desk storage failed: {error}"))
}

fn roster_failure(error: RosterError) -> PanesWebError {
    PanesWebError::Failure(format!("roster failed: {error}"))
}

fn open_error(error: impl std::fmt::Display) -> PanesBackendOpenError {
    PanesBackendOpenError(format!("desk storage unavailable: {error}"))
}

fn system_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// A SQL scalar as JSON. A blob has no JSON form here and reads as `null`.
fn sql_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(number) => json!(number),
        ValueRef::Real(number) => {
            serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
        }
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth_backend::PersistentAuth,
        routes::{
            auth::{AuthBackend, RequestScheme},
            panes::{router, PanesWebState},
        },
    };
    use axum::{
        body::{to_bytes, Body},
        http::{header, Request},
        response::Response,
        Extension, Router,
    };
    use ds_core::client::UnixTransport;
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixListener,
    };
    use tower::ServiceExt;

    const PASSWORD: &str = "test-password";
    const VALID_SAMPLE: &str = r#"{"innerWidth":1280,"innerHeight":800,"screenWidth":1920,"screenHeight":1080,"devicePixelRatio":1.5}"#;
    const CAPABILITY_CMD: &str = "delivery_capability";

    type Requests = Arc<Mutex<Vec<(String, Value)>>>;

    /// A real Unix-socket stand-in for the daemon: one request line in, one reply line out, for
    /// every connection, until the test process exits. Every request is recorded.
    fn fake_daemon(
        socket: &Path,
        reply: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> Requests {
        let listener = UnixListener::bind(socket).expect("bind fake daemon socket");
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let Ok(reader_side) = stream.try_clone() else {
                    continue;
                };
                let mut line = String::new();
                if BufReader::new(reader_side).read_line(&mut line).is_err() {
                    continue;
                }
                let request: Value =
                    serde_json::from_str(line.trim_end()).expect("request line is JSON");
                let cmd = request["cmd"].as_str().unwrap_or_default().to_owned();
                let args = request["args"].clone();
                seen.lock().unwrap().push((cmd.clone(), args.clone()));
                let mut reply_line = reply(&cmd, &args).to_string();
                reply_line.push('\n');
                let _ = stream.write_all(reply_line.as_bytes());
            }
        });
        requests
    }

    fn ok(result: Value) -> Value {
        json!({"ok": true, "result": result})
    }

    fn refused(error: &str) -> Value {
        json!({"ok": false, "error": error})
    }

    fn flock(socket: &Path) -> FlockClient<UnixTransport> {
        FlockClient::new(
            UnixTransport {
                path: socket.to_path_buf(),
            },
            None,
            "panes-backend-test".into(),
            Duration::from_secs(5),
            Some("test-service-token".into()),
        )
    }

    /// The desk data directory as startup initializes it: `PersistentAuth` creates `sessions`.
    fn desk_store(dir: &Path) -> PersistentAuth {
        PersistentAuth::open_with_password(dir, Some(PASSWORD)).expect("desk store")
    }

    fn channel(id: &str, name: &str, kind: &str, match_text: &str) -> Value {
        json!({"id": id, "name": name, "kind": kind, "match": match_text})
    }

    fn pane(pane_id: &str, tab: &str, agent_status: Value) -> Value {
        json!({
            "pane_id": pane_id,
            "tab_label": tab,
            "workspace_label": "desk",
            "cwd": format!("/work/{tab}"),
            "agent": "",
            "agent_status": agent_status,
        })
    }

    fn sample() -> ViewportSample {
        ViewportSample {
            inner_width: 1280,
            inner_height: 800,
            screen_width: 1920,
            screen_height: 1080,
            device_pixel_ratio: 1.5,
        }
    }

    fn calls_for(requests: &Requests, cmd: &str, target: &str) -> usize {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen, args)| seen == cmd && args["target"] == target)
            .count()
    }

    fn router_app(backend: Arc<dyn PanesWebBackend>, auth: &PersistentAuth) -> Router {
        let auth: Arc<dyn AuthBackend> = Arc::new(auth.clone());
        router()
            .layer(Extension(RequestScheme::Http))
            .layer(Extension(auth))
            .layer(Extension(Arc::new(PanesWebState::new(backend))))
    }

    fn get(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header(header::HOST, "desk.test")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn post_viewport(cookie: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/viewport")
            .header(header::HOST, "desk.test")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    async fn read_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn viewport_is_keyed_by_the_hashed_session_and_read_as_dimensions_only() {
        let dir = tempfile::tempdir().unwrap();
        let auth = desk_store(dir.path());
        let token = auth.login("127.0.0.1", PASSWORD).await.unwrap();
        let backend = DaemonPanesBackend::open(
            flock(&dir.path().join("absent.sock")),
            dir.path(),
            Vec::new(),
        )
        .unwrap();
        let now = backend.now_seconds();

        assert!(backend
            .update_viewport(&token, sample(), now)
            .await
            .unwrap());
        // No cookie matches no session, so nothing is written.
        assert!(!backend.update_viewport("", sample(), now).await.unwrap());

        let listed = backend.viewports().await.unwrap();
        assert_eq!(
            listed,
            vec![json!({
                "innerWidth": 1280,
                "innerHeight": 800,
                "devicePixelRatio": 1.5,
                "screenWidth": 1920,
                "screenHeight": 1080,
                "viewport_at": now,
            })]
        );
        let keys: Vec<&str> = listed[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, VIEWPORT_KEYS, "reads keep the Python column order");

        let db = Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        let by_hash: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token_hash=?",
                params![session_key(&token)],
                |row| row.get(0),
            )
            .unwrap();
        let by_raw_cookie: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE token_hash=?",
                params![token],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (by_hash, by_raw_cookie),
            (1, 0),
            "the cookie is stored only as its SHA-256"
        );
    }

    #[tokio::test]
    async fn expired_sessions_neither_accept_a_viewport_nor_appear_in_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let auth = desk_store(dir.path());
        let token = auth.login("127.0.0.1", PASSWORD).await.unwrap();
        let backend = DaemonPanesBackend::open(
            flock(&dir.path().join("absent.sock")),
            dir.path(),
            Vec::new(),
        )
        .unwrap();
        let now = backend.now_seconds();
        assert!(backend
            .update_viewport(&token, sample(), now)
            .await
            .unwrap());

        let db = Connection::open(dir.path().join(DATABASE_FILE)).unwrap();
        db.execute(
            "INSERT INTO sessions(token_hash,expires_at,viewport_width,viewport_at) VALUES(?,?,?,?)",
            params![session_key("expired-cookie"), now - 1.0, 320, now - 5.0],
        )
        .unwrap();
        assert!(!backend
            .update_viewport("expired-cookie", sample(), now)
            .await
            .unwrap());
        assert_eq!(
            backend.viewports().await.unwrap().len(),
            1,
            "only the live session is listed"
        );
    }

    #[tokio::test]
    async fn absent_daemon_is_unavailable_without_starting_one_and_channels_still_answer() {
        let dir = tempfile::tempdir().unwrap();
        let auth = desk_store(dir.path());
        let token = auth.login("127.0.0.1", PASSWORD).await.unwrap();
        let socket = dir.path().join("no-daemon.sock");
        let backend = Arc::new(
            DaemonPanesBackend::open(
                flock(&socket),
                dir.path(),
                vec![channel("desk", "Desk", "lane", "desk")],
            )
            .unwrap(),
        );
        let cookie = format!("vp_session={token}");
        let posted = router_app(backend.clone(), &auth)
            .oneshot(post_viewport(&cookie, VALID_SAMPLE))
            .await
            .unwrap();
        assert_eq!(posted.status(), StatusCode::OK);

        match backend.rpc("list_panes", json!({})).await {
            Err(PanesWebError::Http { status, .. }) => {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            }
            other => panic!("expected a 503 for an absent daemon, got {other:?}"),
        }
        assert!(!socket.exists(), "the backend never starts a daemon");

        let (status, body) = read_json(
            router_app(backend, &auth)
                .oneshot(get("/api/channels", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["daemon"], false);
        assert_eq!(body["channels"][0]["online"], false);
        assert_eq!(body["viewports"][0]["innerWidth"], 1280);
    }

    #[tokio::test]
    async fn roster_joins_configured_and_discovered_lanes_and_skips_lanes_in_authority() {
        let dir = tempfile::tempdir().unwrap();
        let _auth = desk_store(dir.path());
        let socket = dir.path().join("daemon.sock");
        let listing = json!([
            pane("%1", "desk", json!("idle")),
            pane("%3", "scratch", json!("working")),
            pane("%4", "shell", Value::Null),
        ]);
        let requests = fake_daemon(&socket, move |cmd, _| match cmd {
            "list_panes" => ok(json!({"panes": listing.clone()})),
            _ => refused("no answer for this command"),
        });
        let backend = DaemonPanesBackend::open(
            flock(&socket),
            dir.path(),
            vec![channel("desk", "Desk", "lane", "desk")],
        )
        .unwrap();
        let listed = backend.rpc("list_panes", json!({})).await.unwrap();
        let rows = backend
            .roster_rows(listed["panes"].as_array().unwrap())
            .await
            .unwrap();

        assert_eq!(
            rows.len(),
            2,
            "a bare shell is neither configured nor discovered"
        );
        assert_eq!(rows[0]["id"], "desk");
        assert_eq!(rows[0]["pane_id"], "%1");
        assert_eq!(rows[0]["can_message"], true);
        assert_eq!(rows[1]["id"], "pane-3");
        assert_eq!(rows[1]["kind"], "discovered");
        assert_eq!(rows[1]["can_message"], true);
        assert_eq!(
            calls_for(&requests, CAPABILITY_CMD, "%1") + calls_for(&requests, CAPABILITY_CMD, "%3"),
            0,
            "lanes take the fast path and never ask the daemon"
        );
    }

    #[tokio::test]
    async fn capability_answers_gate_messages_and_are_cached_for_twenty_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let _auth = desk_store(dir.path());
        let socket = dir.path().join("daemon.sock");
        let listing = json!([
            pane("%7", "ops", Value::Null),
            pane("%8", "archive", Value::Null),
            pane("%9", "broken", Value::Null),
        ]);
        let requests = fake_daemon(&socket, move |cmd, args| {
            match (cmd, args["target"].as_str()) {
                ("list_panes", _) => ok(json!({"panes": listing.clone()})),
                (CAPABILITY_CMD, Some("%7")) => ok(json!({"agent_alive": true})),
                (CAPABILITY_CMD, Some("%8")) => ok(json!({"agent_alive": false})),
                (CAPABILITY_CMD, _) => refused("capability unavailable"),
                _ => refused("unexpected command"),
            }
        });
        let clock = Arc::new(Mutex::new(1_000.0_f64));
        let ticks = Arc::clone(&clock);
        let channels = vec![
            channel("ops", "Ops", "lane", "ops"),
            channel("archive", "Archive", "lane", "archive"),
            channel("broken", "Broken", "lane", "broken"),
        ];
        let backend = DaemonPanesBackend::open(flock(&socket), dir.path(), channels)
            .unwrap()
            .with_clock(move || *ticks.lock().unwrap());
        let listed = backend.rpc("list_panes", json!({})).await.unwrap();
        let panes_listed = listed["panes"].as_array().unwrap();

        let allowed = |rows: &[Value]| {
            rows.iter()
                .map(|row| {
                    (
                        row["id"].as_str().unwrap().to_owned(),
                        row["can_message"].clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let expected = vec![
            ("ops".to_owned(), json!(true)),
            ("archive".to_owned(), json!(false)),
            ("broken".to_owned(), json!(false)),
        ];
        let calls = || {
            [
                calls_for(&requests, CAPABILITY_CMD, "%7"),
                calls_for(&requests, CAPABILITY_CMD, "%8"),
                calls_for(&requests, CAPABILITY_CMD, "%9"),
            ]
        };

        let first = backend.roster_rows(panes_listed).await.unwrap();
        assert_eq!(allowed(first.as_slice()), expected);
        assert_eq!(calls(), [1, 1, 1], "each pane is asked once");

        // Dict answers are reused within the TTL; a failed call is not cached and is asked again.
        let second = backend.roster_rows(panes_listed).await.unwrap();
        assert_eq!(allowed(second.as_slice()), expected);
        assert_eq!(calls(), [1, 1, 2]);

        *clock.lock().unwrap() += 21.0;
        let third = backend.roster_rows(panes_listed).await.unwrap();
        assert_eq!(allowed(third.as_slice()), expected);
        assert_eq!(calls(), [2, 2, 3], "answers expire after twenty seconds");
    }

    #[tokio::test]
    async fn malformed_daemon_roster_is_a_failure_not_an_empty_roster() {
        let dir = tempfile::tempdir().unwrap();
        let _auth = desk_store(dir.path());
        let socket = dir.path().join("daemon.sock");
        fake_daemon(&socket, |_, _| ok(json!({"panes": [1, "not a pane"]})));
        let backend = DaemonPanesBackend::open(
            flock(&socket),
            dir.path(),
            vec![channel("desk", "Desk", "lane", "desk")],
        )
        .unwrap();
        let listed = backend.rpc("list_panes", json!({})).await.unwrap();
        let error = backend
            .roster_rows(listed["panes"].as_array().unwrap())
            .await
            .unwrap_err();
        match error {
            PanesWebError::Failure(text) => {
                assert!(
                    text.contains("must be an object"),
                    "unexpected error text: {text}"
                );
            }
            other => panic!("expected a roster failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn viewport_and_channels_routes_run_over_the_real_socket_and_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let auth = desk_store(dir.path());
        let token = auth.login("127.0.0.1", PASSWORD).await.unwrap();
        let socket = dir.path().join("daemon.sock");
        let listing = json!([pane("%1", "desk", json!("idle"))]);
        fake_daemon(&socket, move |cmd, _| match cmd {
            "list_panes" => ok(json!({"panes": listing.clone()})),
            _ => refused("unexpected command"),
        });
        let backend = Arc::new(
            DaemonPanesBackend::open(
                flock(&socket),
                dir.path(),
                vec![channel("desk", "Desk", "lane", "desk")],
            )
            .unwrap(),
        );
        let app = router_app(backend, &auth);
        let cookie = format!("vp_session={token}");

        let posted = app
            .clone()
            .oneshot(post_viewport(&cookie, VALID_SAMPLE))
            .await
            .unwrap();
        assert_eq!(posted.status(), StatusCode::OK);
        let (status, body) = read_json(
            app.clone()
                .oneshot(get("/api/channels", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["daemon"], true);
        assert_eq!(body["channels"][0]["can_message"], true);
        assert_eq!(body["viewports"][0]["innerWidth"], 1280);
        assert!(
            body["viewports"][0].get("token_hash").is_none(),
            "the status never exposes session identifiers"
        );

        let forged = app
            .oneshot(post_viewport("vp_session=forged", VALID_SAMPLE))
            .await
            .unwrap();
        assert_eq!(forged.status(), StatusCode::UNAUTHORIZED);
    }
}
