//! Axum response boundary for the Python-compatible HTTP JSON codec.
//!
//! The request body passed here is already framed by the HTTP server. Raw
//! framing, duplicate length headers and short reads are handled by the server
//! task before `request_json` is called.

use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    extract::Extension,
    http::{header, HeaderValue, Method, Request, Response, StatusCode, Uri},
    routing::any,
    Router,
};
use ds_core::{
    client::{connect_or_start_system, ClientError, FlockClient, UnixTransport},
    error::DunkError,
    http::{ApiError, Backend, Daemon, DaemonError},
    terminal::{tmux::TmuxBackend, Pane, Terminal},
};

use crate::python_json::{self, JsonError, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl EncodedResponse {
    /// Serialize with Python `json.dumps` semantics before calculating any
    /// transport length. `pretty=true` corresponds to `indent=2`.
    pub fn json(status: u16, payload: &Value, pretty: bool) -> Self {
        Self {
            status,
            body: python_json::response_bytes(payload, pretty.then_some(2)),
        }
    }

    pub fn content_length(&self) -> usize {
        self.body.len()
    }

    pub fn into_axum(self) -> Result<Response<Body>, axum::http::status::InvalidStatusCode> {
        let status = StatusCode::from_u16(self.status)?;
        let length = HeaderValue::from_str(&self.body.len().to_string())
            .expect("decimal byte length is a valid HTTP header");
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = status;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(header::CONTENT_LENGTH, length);
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Ok(response)
    }
}

/// Decode one body after the HTTP layer has completed framing and size checks.
pub fn request_json(body: &[u8]) -> Result<Value, JsonError> {
    python_json::loads(body)
}

#[derive(Clone, Copy)]
pub(crate) struct PythonNegativeLength;

pub const DEFAULT_API_HOST: &str = "127.0.0.1";
pub const DEFAULT_API_PORT: u16 = 8792;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiBindConfig {
    pub host: String,
    pub port: u16,
}

pub fn api_bind_config() -> Result<ApiBindConfig, String> {
    api_bind_config_from(|name| std::env::var(name).ok())
}

pub fn api_bind_config_from(
    mut get: impl FnMut(&str) -> Option<String>,
) -> Result<ApiBindConfig, String> {
    let host = get("DUNKINGSHEEP_API_HOST").unwrap_or_else(|| DEFAULT_API_HOST.to_owned());
    let port = match get("DUNKINGSHEEP_API_PORT") {
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| format!("invalid DUNKINGSHEEP_API_PORT: {value:?}"))?,
        None => DEFAULT_API_PORT,
    };
    Ok(ApiBindConfig { host, port })
}

pub async fn bind_listener(host: &str, port: u16) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind((host, port)).await
}

pub async fn serve_listener(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    let address = listener.local_addr()?;
    serve_configured(listener, app, &address.ip().to_string(), address.port()).await
}

pub async fn serve_configured(
    listener: tokio::net::TcpListener,
    app: Router,
    host: &str,
    port: u16,
) -> std::io::Result<()> {
    println!("dunkingsheep api listening on http://{host}:{port}");
    crate::python_http::serve(listener, app).await
}

pub async fn serve_with_shutdown<F>(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    crate::python_http::serve_with_shutdown(listener, app, shutdown).await
}

/// Production route graph. Daemon connection/start is lazy, so `/healthz`
/// remains available without changing the daemon's socket or startup gates.
pub fn system_router(daemon_executable: PathBuf) -> Result<Router, DunkError> {
    let backend = ds_core::server_runtime::configured_backend()?;
    Ok(router(
        SocketDaemon {
            daemon_executable,
            client: Mutex::new(None),
        },
        TmuxHttpBackend(Mutex::new(backend)),
    ))
}

struct SocketDaemon {
    daemon_executable: PathBuf,
    client: Mutex<Option<FlockClient<UnixTransport>>>,
}

impl Daemon for SocketDaemon {
    fn call(
        &self,
        command: &str,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        let mut slot = self
            .client
            .lock()
            .map_err(|_| DaemonError::Failed("HTTP daemon client lock poisoned".into()))?;
        if slot.is_none() {
            *slot = Some(
                connect_or_start_system(
                    None,
                    None,
                    true,
                    Duration::from_secs(10),
                    &self.daemon_executable,
                )
                .map_err(|error| DaemonError::Unavailable(error.to_string()))?,
            );
        }
        let client = slot.as_mut().expect("client initialized");
        let args = args.as_object().cloned().unwrap_or_default();
        client.call(command, args).map_err(map_client_error)
    }
}

fn map_client_error(error: ClientError) -> DaemonError {
    match error {
        ClientError::Unavailable(message) => DaemonError::Unavailable(message),
        ClientError::Io(error) => DaemonError::Transport(error.to_string()),
        ClientError::Dunk(DunkError::Unavailable(message)) => DaemonError::Unavailable(message),
        ClientError::Dunk(error) => DaemonError::Failed(error.to_string()),
        ClientError::Protocol(message) => DaemonError::Failed(message),
    }
}

struct TmuxHttpBackend(Mutex<TmuxBackend>);

impl Backend for TmuxHttpBackend {
    fn name(&self) -> String {
        "tmux".into()
    }
    fn capabilities(&self) -> std::collections::BTreeSet<String> {
        ["resize".into()].into()
    }

    fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
        let Ok(terminal) = self.0.lock() else {
            return Vec::new();
        };
        let mut panes = terminal.list_panes();
        panes.sort_by_key(|pane| (pane.workspace_index, pane.window_index));
        panes.iter().map(http_pane).collect()
    }

    fn resize(
        &self,
        pane_id: &str,
        cols: &serde_json::Value,
        rows: &serde_json::Value,
    ) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let cols = python_int(cols)?;
        let rows = python_int(rows)?;
        if !(1..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
            return Err("cols and rows must be between 1 and 1000".into());
        }
        let mut terminal = self
            .0
            .lock()
            .map_err(|_| "terminal backend lock poisoned".to_owned())?;
        let resized = terminal
            .resize(pane_id, cols, rows)
            .map_err(|error| error.message)?;
        let resized_panes = resized
            .resized_panes
            .into_iter()
            .map(|pane| {
                serde_json::json!({
                    "pane_id":pane.pane_id,"cols":pane.cols,"rows":pane.rows,
                    "cols_before":pane.cols_before,"rows_before":pane.rows_before
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({
            "pane_id":resized.pane_id,
            "requested":{"cols":resized.requested_cols,"rows":resized.requested_rows},
            "cols":resized.cols,"rows":resized.rows,
            "window_pane_count":resized.window_pane_count,
            "resized_panes":resized_panes,
            "clients_attached":resized.clients_attached,
            "overridable_by_client":resized.overridable_by_client
        })
        .as_object()
        .expect("JSON resize result object")
        .clone())
    }
}

fn http_pane(pane: &Pane) -> serde_json::Value {
    let agent = ds_core::terminal::parse::agent_kind(&pane.command, &pane.title);
    serde_json::json!({
        "pane_id":pane.pane_id,"terminal_id":pane.pane_id,"tab_id":pane.window_id,
        "workspace_id":pane.session_id,"cwd":pane.cwd,"foreground_cwd":pane.cwd,
        "agent":agent,"agent_status":"unknown","agent_session":null,
        "focused":pane.active,"label":pane.label,"terminal_title_stripped":pane.label,
        "terminal_title":pane.title,"pane_pid":pane.pane_pid,
        "cols":pane.width,"rows":pane.height,"managed":false,
        "tab_label":pane.tab,"workspace_label":pane.workspace,
        "tab_number":pane.window_index,"workspace_number":pane.workspace_index
    })
}

fn python_int(value: &serde_json::Value) -> Result<i64, String> {
    let parsed: i128 = match value {
        serde_json::Value::Bool(value) => {
            if *value {
                1
            } else {
                0
            }
        }
        serde_json::Value::Number(number) if number.is_i64() => number.as_i64().unwrap() as i128,
        serde_json::Value::Number(number) if number.is_u64() => number.as_u64().unwrap() as i128,
        serde_json::Value::Number(number) => {
            let Some(value) = number.as_f64() else {
                return Err("cols and rows must be integers".into());
            };
            if !value.is_finite() {
                return Err("cols and rows must be integers".into());
            }
            if value.trunc() >= i128::MAX as f64 {
                return Ok(i64::MAX);
            }
            if value.trunc() <= i128::MIN as f64 {
                return Ok(i64::MIN);
            }
            value.trunc() as i128
        }
        serde_json::Value::String(value) => match value.trim().parse::<i128>() {
            Ok(value) => value,
            Err(_)
                if value
                    .trim()
                    .trim_start_matches(['+', '-'])
                    .bytes()
                    .all(|b| b.is_ascii_digit()) =>
            {
                if value.trim_start().starts_with('-') {
                    i128::MIN
                } else {
                    i128::MAX
                }
            }
            Err(_) => return Err("cols and rows must be integers".into()),
        },
        _ => return Err("cols and rows must be integers".into()),
    };
    if parsed < i64::MIN as i128 {
        Ok(i64::MIN)
    } else if parsed > i64::MAX as i128 {
        Ok(i64::MAX)
    } else {
        Ok(parsed as i64)
    }
}

#[derive(Clone)]
struct HttpState {
    daemon: Arc<Mutex<Box<dyn Daemon + Send>>>,
    backend: Arc<Mutex<Box<dyn Backend + Send>>>,
}

/// Construct the in-process HTTP adapter around the already reconciled route
/// helpers. `daemon` and `backend` are injected so the router tests never open
/// a socket, bind a port, or contact the configured terminal provider.
pub fn router<D, B>(daemon: D, backend: B) -> Router
where
    D: Daemon + Send + 'static,
    B: Backend + Send + 'static,
{
    let state = HttpState {
        daemon: Arc::new(Mutex::new(Box::new(daemon))),
        backend: Arc::new(Mutex::new(Box::new(backend))),
    };
    Router::new()
        .fallback(any(dispatch))
        .layer(Extension(state))
}

async fn dispatch(
    Extension(state): Extension<HttpState>,
    request: Request<Body>,
) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    if std::env::var("DUNKINGSHEEP_API_VERBOSE").as_deref() == Ok("1") {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.to_string())
            .unwrap_or_else(|| "-".into());
        eprintln!("{peer} - - \"{method} {uri} HTTP/1.1\"");
    }
    match method {
        Method::GET => {
            let query = parse_query(uri.query().unwrap_or_default());
            let result = match (state.backend.lock(), state.daemon.lock()) {
                (Ok(backend), Ok(daemon)) => {
                    ds_core::http::route(uri.path(), &query, backend.as_ref(), daemon.as_ref())
                }
                _ => Err(ApiError::new(
                    500,
                    "internal error: HTTP adapter lock poisoned",
                )),
            };
            match result {
                Ok(payload) => json_response(200, payload.into(), true),
                Err(error) => api_error_response(error),
            }
        }
        Method::POST => post(state, uri, request).await,
        _ => json_response(
            405,
            Value::from(serde_json::json!({"error":"Method Not Allowed"})),
            true,
        ),
    }
}

async fn post(state: HttpState, uri: Uri, request: Request<Body>) -> Response<Body> {
    let python_unbounded_read = request.extensions().get::<PythonNegativeLength>().is_some();
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > ds_core::http::MAX_POST_BYTES)
    {
        return api_error_response(ApiError::new(
            413,
            format!("body larger than {} bytes", ds_core::http::MAX_POST_BYTES),
        ));
    }
    let body_limit = if python_unbounded_read {
        usize::MAX
    } else {
        ds_core::http::MAX_POST_BYTES + 1
    };
    let body = match to_bytes(request.into_body(), body_limit).await {
        Ok(body) => body,
        Err(_) => {
            return api_error_response(ApiError::new(400, "body must be JSON"));
        }
    };
    if !python_unbounded_read && body.len() > ds_core::http::MAX_POST_BYTES {
        return api_error_response(ApiError::new(
            413,
            format!("body larger than {} bytes", ds_core::http::MAX_POST_BYTES),
        ));
    }
    let parsed = if body.is_empty() {
        Value::Object(Vec::new())
    } else {
        match request_json(&body) {
            Ok(payload) => payload,
            Err(_) => return api_error_response(ApiError::new(400, "body must be JSON")),
        }
    };
    let payload = parsed.resize_fields();
    let result = match state.backend.lock() {
        Ok(backend) => ds_core::http::route_post(
            uri.path(),
            &payload,
            backend.as_ref(),
            &ds_core::http::now_iso(),
        ),
        Err(_) => Err(ApiError::new(
            500,
            "internal error: HTTP adapter lock poisoned",
        )),
    };
    match result {
        Ok(payload) => json_response(200, payload.into(), true),
        Err(error) if error.context.contains_key("requested") => {
            resize_error_response(error, &parsed)
        }
        Err(error) => api_error_response(error),
    }
}

fn resize_error_response(error: ApiError, requested: &Value) -> Response<Body> {
    let mut fields = vec![(
        python_json::PythonString::from_str("error"),
        Value::String(python_json::PythonString::from_str(&error.message)),
    )];
    for key in ["name", "pane_id"] {
        if let Some(value) = error.context.get(key) {
            fields.push((
                python_json::PythonString::from_str(key),
                Value::from(value.clone()),
            ));
        }
    }
    let requested_fields = ["cols", "rows"]
        .into_iter()
        .filter_map(|name| {
            requested
                .resize_value(name)
                .cloned()
                .map(|value| (python_json::PythonString::from_str(name), value))
        })
        .collect();
    fields.push((
        python_json::PythonString::from_str("requested"),
        Value::Object(requested_fields),
    ));
    json_response(error.status, Value::Object(fields), true)
}

fn api_error_response(error: ApiError) -> Response<Body> {
    let status = error.status;
    json_response(status, Value::from(ds_core::http::error_body(&error)), true)
}

fn json_response(status: u16, payload: Value, pretty: bool) -> Response<Body> {
    EncodedResponse::json(status, &payload, pretty)
        .into_axum()
        .expect("route status is a valid HTTP status")
}

fn parse_query(raw: &str) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();
    for part in raw.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        let key = percent_decode(key);
        let value = percent_decode(value);
        if !value.is_empty() {
            result.entry(key).or_insert_with(Vec::new).push(value);
        }
    }
    result
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => {
                decoded.push(b' ');
                at += 1;
            }
            b'%' if at + 2 < bytes.len() => {
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                if let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                    decoded.push((high << 4) | low);
                    at += 3;
                } else {
                    decoded.push(b'%');
                    at += 1;
                }
            }
            byte => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::{body::to_bytes, http::header, http::Request};
    use serde_json::{json, Map};
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use tower::ServiceExt;

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker: &Waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeDaemon(Mutex<Vec<(String, serde_json::Value)>>);

    impl Daemon for FakeDaemon {
        fn call(
            &self,
            command: &str,
            args: &serde_json::Value,
        ) -> Result<serde_json::Value, ds_core::http::DaemonError> {
            self.0.lock().unwrap().push((command.into(), args.clone()));
            Ok(json!({"ok":true,"command":command}))
        }
    }

    #[derive(Default)]
    pub(crate) struct FakeBackend(Mutex<Vec<(String, serde_json::Value, serde_json::Value)>>);

    impl Backend for FakeBackend {
        fn name(&self) -> String {
            "fake".into()
        }
        fn capabilities(&self) -> std::collections::BTreeSet<String> {
            ["resize".into()].into()
        }
        fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
            vec![json!({"pane_id":"w1:p1","label":"test","agent":"codex","cols":80,"rows":24})]
        }
        fn resize(
            &self,
            pane_id: &str,
            cols: &serde_json::Value,
            rows: &serde_json::Value,
        ) -> Result<Map<String, serde_json::Value>, String> {
            let cols_int = python_int(cols)?;
            let rows_int = python_int(rows)?;
            if !(1..=1000).contains(&cols_int) || !(1..=1000).contains(&rows_int) {
                return Err("cols and rows must be between 1 and 1000".into());
            }
            self.0
                .lock()
                .unwrap()
                .push((pane_id.into(), cols.clone(), rows.clone()));
            Ok(json!({"cols":cols,"rows":rows,"resized_panes":[]} )
                .as_object()
                .unwrap()
                .clone())
        }
    }

    fn request(method: Method, uri: &str, body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "localhost")
            .body(body.into())
            .unwrap()
    }

    fn test_router() -> Router {
        router(FakeDaemon::default(), FakeBackend::default())
    }

    #[test]
    fn response_uses_python_bytes_for_status_and_content_length() {
        let payload = request_json(br#"{"snow":"\u2603","bad":"\ud800"}"#).unwrap();
        let encoded = EncodedResponse::json(400, &payload, true);
        let expected = python_json::response_bytes(&payload, Some(2));
        assert_eq!(encoded.status, 400);
        assert_eq!(encoded.body, expected);
        assert_eq!(encoded.content_length(), expected.len());

        let response = encoded.into_axum().unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
            expected.len().to_string()
        );
    }

    #[test]
    fn request_boundary_does_not_reframe_or_silently_stringify() {
        let body = br#"{"cols":80,"rows":24,"unknown":"\ud800"}"#;
        let parsed = request_json(body).unwrap();
        assert_eq!(
            parsed.resize_fields(),
            serde_json::json!({"cols":80,"rows":24})
        );
        assert!(request_json(b"{bad").is_err());
    }

    #[test]
    fn in_process_get_reuses_route_and_emits_python_pretty_json_headers() {
        let app = test_router();
        let response =
            block_on(app.oneshot(request(Method::GET, "/api/roster", Body::empty()))).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let expected = b"{\n  \"ok\": true,\n  \"command\": \"roster\"\n}";
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
            expected.len().to_string()
        );
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(bytes.as_ref(), expected);
    }

    #[test]
    fn in_process_post_keeps_surrogate_extensions_and_exact_body_limit() {
        let app = test_router();
        let mut body = br#"{"unknown\ud800":"\udfff","cols":80.0,"rows":24}"#.to_vec();
        body.resize(ds_core::http::MAX_POST_BYTES, b' ');
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/terminals/test/resize",
            Body::from(body),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let declared_length = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(declared_length, bytes.len());
        let mut oversized = br#"{"cols":80,"rows":24}"#.to_vec();
        oversized.resize(ds_core::http::MAX_POST_BYTES + 1, b' ');
        let response = block_on(app.oneshot(request(
            Method::POST,
            "/api/terminals/test/resize",
            Body::from(oversized),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            bytes.as_ref(),
            b"{\n  \"error\": \"body larger than 4096 bytes\"\n}"
        );
    }

    #[test]
    fn in_process_post_malformed_body_maps_to_compact_python_error_shape() {
        let response = block_on(test_router().oneshot(request(
            Method::POST,
            "/api/terminals/test/resize",
            Body::from(b"{bad".as_slice()),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(bytes.as_ref(), b"{\n  \"error\": \"body must be JSON\"\n}");
    }

    #[test]
    fn post_error_echo_retains_surrogates_huge_integers_lists_and_objects() {
        for (body, expected) in [
            (
                br#"{"cols":"\ud800","rows":24}"#.as_slice(),
                "{\n  \"error\": \"cols and rows must be integers\",\n  \"name\": \"test\",\n  \"pane_id\": \"w1:p1\",\n  \"requested\": {\n    \"cols\": \"\\ud800\",\n    \"rows\": 24\n  }\n}",
            ),
            (
                br#"{"cols":999999999999999999999999999999999999999999999999,"rows":24}"#.as_slice(),
                "{\n  \"error\": \"cols and rows must be between 1 and 1000\",\n  \"name\": \"test\",\n  \"pane_id\": \"w1:p1\",\n  \"requested\": {\n    \"cols\": 999999999999999999999999999999999999999999999999,\n    \"rows\": 24\n  }\n}",
            ),
            (
                br#"{"cols":[1,2],"rows":24}"#.as_slice(),
                "{\n  \"error\": \"cols and rows must be integers\",\n  \"name\": \"test\",\n  \"pane_id\": \"w1:p1\",\n  \"requested\": {\n    \"cols\": [\n      1,\n      2\n    ],\n    \"rows\": 24\n  }\n}",
            ),
            (
                br#"{"cols":{"nested":1},"rows":24}"#.as_slice(),
                "{\n  \"error\": \"cols and rows must be integers\",\n  \"name\": \"test\",\n  \"pane_id\": \"w1:p1\",\n  \"requested\": {\n    \"cols\": {\n      \"nested\": 1\n    },\n    \"rows\": 24\n  }\n}",
            ),
        ] {
            let response = block_on(test_router().oneshot(request(
                Method::POST,
                "/api/terminals/test/resize",
                Body::from(body),
            )))
            .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
            assert_eq!(bytes.as_ref(), expected.as_bytes());
        }
    }

    #[test]
    fn bind_defaults_and_environment_overrides_match_python_settings() {
        assert_eq!(
            api_bind_config_from(|_| None).unwrap(),
            ApiBindConfig {
                host: "127.0.0.1".into(),
                port: 8792
            }
        );
        assert_eq!(
            api_bind_config_from(|name| match name {
                "DUNKINGSHEEP_API_HOST" => Some("127.0.0.2".into()),
                "DUNKINGSHEEP_API_PORT" => Some("0".into()),
                _ => None,
            })
            .unwrap(),
            ApiBindConfig {
                host: "127.0.0.2".into(),
                port: 0
            }
        );
    }

    #[tokio::test]
    async fn ephemeral_listener_serves_until_shutdown_without_production_port() {
        let listener = bind_listener("127.0.0.1", 0).await.unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), DEFAULT_API_PORT);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve_with_shutdown(listener, test_router(), async move {
            let _ = shutdown_rx.await;
        }));
        shutdown_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
