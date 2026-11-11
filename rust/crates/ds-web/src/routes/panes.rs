//! Pane, channel, and browser viewport routes ported from `web/app.py`.
//!
//! The web boundary owns request validation, auth checks, response shaping,
//! and the short pane-text cache. The injected backend owns session storage,
//! daemon RPC, roster construction, and wall-clock formatting.

use super::response_error::WebResponseError;

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

const PANE_TEXT_TTL_S: f64 = 1.5;
const PANE_TEXT_READONLY_NOTE: &str =
    "read-only: typing goes through the delivery path, not this endpoint";

pub type PanesWebFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from the small session/daemon boundary used by these routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanesWebError {
    Http { status: StatusCode, detail: String },
    Failure(String),
}

impl PanesWebError {
    fn into_response(self) -> Response {
        match self {
            Self::Http { status, detail } => detail_response(status, detail),
            Self::Failure(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
    }
}

/// Required backend operations for the pane routes. `roster_rows` is async and
/// fallible: implementations resolve daemon pane authority off the executor and
/// must return roster errors rather than an empty roster.
pub trait PanesWebBackend: Send + Sync + 'static {
    fn rpc<'a>(
        &'a self,
        method: &'a str,
        args: Value,
    ) -> PanesWebFuture<'a, Result<Value, PanesWebError>>;

    fn update_viewport<'a>(
        &'a self,
        cookie: &'a str,
        sample: ViewportSample,
        now_seconds: f64,
    ) -> PanesWebFuture<'a, Result<bool, PanesWebError>>;

    fn viewports<'a>(&'a self) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>>;

    fn roster_rows<'a>(
        &'a self,
        panes: &'a [Value],
    ) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>>;

    fn now_seconds(&self) -> f64;

    fn checked_at(&self) -> String;
}

/// Per-router backend and Python-compatible pane-text TTL cache.
pub struct PanesWebState {
    backend: Arc<dyn PanesWebBackend>,
    pane_text_cache: Mutex<HashMap<String, (f64, String)>>,
}

impl PanesWebState {
    pub fn new(backend: Arc<dyn PanesWebBackend>) -> Self {
        Self {
            backend,
            pane_text_cache: Mutex::new(HashMap::new()),
        }
    }
}

/// Mount the routes. The host application supplies auth, scheme, and pane state
/// as Axum extensions, matching the other `ds-web` route groups.
pub fn router() -> Router {
    Router::new()
        .route("/api/viewport", post(report_viewport))
        .route("/api/channels", get(channels))
        .route("/api/pane-text", get(api_pane_text))
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewportSample {
    pub inner_width: i64,
    pub inner_height: i64,
    pub screen_width: i64,
    pub screen_height: i64,
    pub device_pixel_ratio: f64,
}

impl ViewportSample {
    fn is_valid(self) -> bool {
        (1..=32_768).contains(&self.inner_width)
            && (1..=32_768).contains(&self.inner_height)
            && (1..=32_768).contains(&self.screen_width)
            && (1..=32_768).contains(&self.screen_height)
            && self.device_pixel_ratio.is_finite()
            && self.device_pixel_ratio > 0.0
            && self.device_pixel_ratio <= 16.0
    }
}

async fn report_viewport(
    Extension(state): Extension<Arc<PanesWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response.into_response();
    }

    let body = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => return invalid_viewport_response(),
    };
    let sample: ViewportSample = match serde_json::from_slice::<ViewportSample>(&body) {
        Ok(sample) if sample.is_valid() => sample,
        _ => return invalid_viewport_response(),
    };
    let cookie = cookie_value(&headers, "vp_session").unwrap_or_default();
    match state
        .backend
        .update_viewport(&cookie, sample, state.backend.now_seconds())
        .await
    {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => detail_response(StatusCode::UNAUTHORIZED, "Browser session required"),
        Err(error) => error.into_response(),
    }
}

async fn channels(
    Extension(state): Extension<Arc<PanesWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response.into_response();
    }

    let (panes, daemon) = match state.backend.rpc("list_panes", json!({})).await {
        Ok(payload) => match payload.get("panes").and_then(Value::as_array) {
            Some(panes) => (panes.clone(), true),
            None => return internal_error_response(),
        },
        Err(PanesWebError::Http { .. }) => (Vec::new(), false),
        Err(PanesWebError::Failure(_)) => return internal_error_response(),
    };
    let channels = match state.backend.roster_rows(&panes).await {
        Ok(channels) => channels,
        Err(error) => return error.into_response(),
    };
    let viewports = match state.backend.viewports().await {
        Ok(viewports) => viewports,
        Err(error) => return error.into_response(),
    };
    Json(ChannelsResponse {
        channels,
        daemon,
        viewports,
        checked_at: state.backend.checked_at(),
    })
    .into_response()
}

#[derive(Default, Deserialize)]
struct PaneTextQuery {
    pane: Option<String>,
    lines: Option<String>,
}

#[derive(Serialize)]
struct ChannelsResponse {
    channels: Vec<Value>,
    daemon: bool,
    viewports: Vec<Value>,
    checked_at: String,
}

#[derive(Serialize)]
struct PaneTextResponse {
    pane: String,
    text: String,
    readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

#[derive(Serialize)]
struct PaneTextFailureResponse {
    pane: String,
    text: String,
    readonly: bool,
    reason: String,
}

#[derive(Serialize)]
struct QueryValidationError {
    detail: Vec<QueryValidationDetail>,
}

#[derive(Serialize)]
struct QueryValidationDetail {
    #[serde(rename = "type")]
    kind: &'static str,
    loc: Vec<&'static str>,
    msg: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
}

async fn api_pane_text(
    Extension(state): Extension<Arc<PanesWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response.into_response();
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query"),
    };
    let query: PaneTextQuery = match serde_json::from_value(Value::Object(
        query
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect(),
    )) {
        Ok(query) => query,
        Err(_) => return detail_response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid query"),
    };
    let Some(pane) = query.pane else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(QueryValidationError {
                detail: vec![QueryValidationDetail {
                    kind: "missing",
                    loc: vec!["query", "pane"],
                    msg: "Field required",
                    input: None,
                }],
            }),
        )
            .into_response();
    };
    let lines = match query.lines {
        Some(value) => match parse_pydantic_int(&value) {
            Some(lines) => lines,
            None => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(QueryValidationError {
                        detail: vec![QueryValidationDetail {
                            kind: "int_parsing",
                            loc: vec!["query", "lines"],
                            msg: "Input should be a valid integer, unable to parse string as an integer",
                            input: Some(value),
                        }],
                    }),
                )
                    .into_response();
            }
        },
        None => 40,
    }
    .clamp(1, 200);
    let pane = pane.trim().to_owned();
    if !pane.starts_with('%') {
        return detail_response(StatusCode::BAD_REQUEST, "a tmux pane id looks like %9");
    }

    let now = state.backend.now_seconds();
    if let Some((seen, text)) = state
        .pane_text_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&pane)
        .cloned()
    {
        if now - seen < PANE_TEXT_TTL_S {
            return Json(PaneTextResponse {
                pane,
                text,
                readonly: true,
                note: None,
            })
            .into_response();
        }
    }

    let result = match state
        .backend
        .rpc("read_pane", json!({"target": pane, "lines": lines}))
        .await
    {
        Ok(result) => result,
        Err(PanesWebError::Http { status, detail }) => return detail_response(status, detail),
        Err(PanesWebError::Failure(error)) => {
            return Json(PaneTextFailureResponse {
                pane,
                text: String::new(),
                readonly: true,
                reason: format!("the daemon could not read the pane: {error}"),
            })
            .into_response();
        }
    };
    let text = result
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    state
        .pane_text_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(pane.clone(), (now, text.clone()));
    Json(PaneTextResponse {
        pane,
        text,
        readonly: true,
        note: Some(PANE_TEXT_READONLY_NOTE),
    })
    .into_response()
}

/// Match Pydantic's string-to-int query conversion while keeping values outside
/// the route's 1..=200 range safe to clamp without overflowing a machine int.
fn parse_pydantic_int(raw: &str) -> Option<i64> {
    let value = raw.trim();
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty()
        || digits.len() > 8_600
        || digits.bytes().filter(u8::is_ascii_digit).count() > 4_300
    {
        return None;
    }

    fn valid_digit_sequence(value: &str) -> bool {
        let bytes = value.as_bytes();
        !bytes.is_empty()
            && bytes.iter().enumerate().all(|(index, byte)| match byte {
                b'0'..=b'9' => true,
                b'_' => {
                    index > 0
                        && index + 1 < bytes.len()
                        && bytes[index - 1].is_ascii_digit()
                        && bytes[index + 1].is_ascii_digit()
                }
                _ => false,
            })
    }

    let integer = digits.split_once('.');
    let integer_part = match integer {
        Some((whole, fraction)) => {
            if !valid_digit_sequence(whole)
                || !valid_digit_sequence(fraction)
                || fraction.bytes().any(|byte| byte != b'0')
            {
                return None;
            }
            whole
        }
        None => digits,
    };
    if !valid_digit_sequence(integer_part) {
        return None;
    }
    let magnitude = integer_part
        .bytes()
        .filter(|byte| *byte != b'_')
        .fold(0_i64, |value, byte| {
            value
                .saturating_mul(10)
                .saturating_add(i64::from(byte - b'0'))
                .min(201)
        });
    Some(if negative { -magnitude } else { magnitude })
}

async fn authenticate(
    backend: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<Identity, WebResponseError> {
    if let Some(response) = same_origin_refusal(headers, scheme) {
        return Err(response.into());
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = cookie_value(headers, "vp_session");
    backend
        .identity(bearer, cookie.as_deref())
        .await
        .map_err(auth_error_response)
        .map_err(WebResponseError::from)
}

fn same_origin_refusal(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    (origin != format!("{scheme}://{host}"))
        .then(|| detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused"))
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (cookie_name, value) = part.trim().split_once('=')?;
            (cookie_name.trim() == name).then(|| value.trim().to_owned())
        })
}

fn auth_error_response(error: AuthError) -> Response {
    match error {
        AuthError::TooManyAttempts => detail_response(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; retry in five minutes",
        ),
        AuthError::IncorrectPassword => {
            detail_response(StatusCode::UNAUTHORIZED, "Incorrect password")
        }
        AuthError::SignInRequired => detail_response(StatusCode::UNAUTHORIZED, "Sign in required"),
        AuthError::Internal(_) => internal_error_response(),
    }
}

fn invalid_viewport_response() -> Response {
    detail_response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid viewport sample")
}

fn internal_error_response() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn parse_query(raw: Option<&str>) -> Result<HashMap<String, String>, ()> {
    fn decode(component: &str) -> Result<String, ()> {
        fn hex_value(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }

        let bytes = component.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' => {
                    match (
                        bytes.get(index + 1).and_then(|byte| hex_value(*byte)),
                        bytes.get(index + 2).and_then(|byte| hex_value(*byte)),
                    ) {
                        (Some(high), Some(low)) => {
                            decoded.push((high << 4) | low);
                            index += 3;
                        }
                        _ => {
                            decoded.push(b'%');
                            index += 1;
                        }
                    }
                }
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        Ok(String::from_utf8_lossy(&decoded).into_owned())
    }

    let mut query = HashMap::new();
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(decode(key)?, decode(value)?);
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request as HttpRequest},
    };
    use std::task::{Context, Poll, Waker};
    use tower::ServiceExt;

    struct FakeAuth;

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }

        fn identity<'a>(
            &'a self,
            _bearer: Option<&'a str>,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async { Ok(Identity::Owner) })
        }

        fn logout<'a>(
            &'a self,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }
    }

    struct FakeBackend {
        rpc_result: Mutex<Result<Value, PanesWebError>>,
        viewport_result: Mutex<Result<bool, PanesWebError>>,
        viewports_result: Mutex<Result<Vec<Value>, PanesWebError>>,
        roster: Vec<Value>,
        roster_result: Mutex<Result<Vec<Value>, PanesWebError>>,
        calls: Mutex<Vec<(String, Value)>>,
        viewport_updates: Mutex<Vec<(String, ViewportSample, f64)>>,
        roster_inputs: Mutex<Vec<Vec<Value>>>,
    }

    impl FakeBackend {
        fn new(rpc_result: Result<Value, PanesWebError>) -> Arc<Self> {
            let roster = vec![json!({
                "id": "cos", "name": "Desk", "online": false,
                "status": "offline", "can_message": false
            })];
            Arc::new(Self {
                rpc_result: Mutex::new(rpc_result),
                viewport_result: Mutex::new(Ok(true)),
                viewports_result: Mutex::new(Ok(vec![json!({"innerWidth": 1280})])),
                roster_result: Mutex::new(Ok(roster.clone())),
                roster,
                calls: Mutex::new(Vec::new()),
                viewport_updates: Mutex::new(Vec::new()),
                roster_inputs: Mutex::new(Vec::new()),
            })
        }
    }

    impl PanesWebBackend for FakeBackend {
        fn rpc<'a>(
            &'a self,
            method: &'a str,
            args: Value,
        ) -> PanesWebFuture<'a, Result<Value, PanesWebError>> {
            self.calls.lock().unwrap().push((method.to_owned(), args));
            let result = self.rpc_result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn update_viewport<'a>(
            &'a self,
            cookie: &'a str,
            sample: ViewportSample,
            now_seconds: f64,
        ) -> PanesWebFuture<'a, Result<bool, PanesWebError>> {
            self.viewport_updates
                .lock()
                .unwrap()
                .push((cookie.to_owned(), sample, now_seconds));
            let result = self.viewport_result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn viewports<'a>(&'a self) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>> {
            let result = self.viewports_result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn roster_rows<'a>(
            &'a self,
            panes: &'a [Value],
        ) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>> {
            self.roster_inputs.lock().unwrap().push(panes.to_vec());
            let result = self.roster_result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn now_seconds(&self) -> f64 {
            100.0
        }

        fn checked_at(&self) -> String {
            "2026-11-03T00:00:00Z".into()
        }
    }

    fn app(backend: Arc<FakeBackend>) -> Router {
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth);
        router()
            .layer(Extension(RequestScheme::Http))
            .layer(Extension(auth))
            .layer(Extension(Arc::new(PanesWebState::new(backend))))
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker: &Waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    fn request(
        method: &str,
        uri: &str,
        body: Option<&str>,
        cookie: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "desk.test");
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        builder
            .body(
                body.map(|body| Body::from(body.to_owned()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap()
    }

    fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn report_viewport_validates_and_updates_the_browser_session() {
        let backend = FakeBackend::new(Ok(json!({"panes": []})));
        let app = app(Arc::clone(&backend));
        let invalid = request(
            "POST",
            "/api/viewport",
            Some(
                r#"{"innerWidth":0,"innerHeight":800,"screenWidth":1280,"screenHeight":800,"devicePixelRatio":1.0}"#,
            ),
            Some("vp_session=valid"),
        );
        let (status, body) = response_json(block_on(app.clone().oneshot(invalid)).unwrap());
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body, json!({"detail": "Invalid viewport sample"}));
        assert!(backend.viewport_updates.lock().unwrap().is_empty());

        let valid = request(
            "POST",
            "/api/viewport",
            Some(
                r#"{"innerWidth":1280,"innerHeight":800,"screenWidth":1920,"screenHeight":1080,"devicePixelRatio":1.5}"#,
            ),
            Some("other=x; vp_session=valid"),
        );
        let (status, body) = response_json(block_on(app.oneshot(valid)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
        assert_eq!(
            backend.viewport_updates.lock().unwrap().as_slice(),
            &[(
                "valid".into(),
                ViewportSample {
                    inner_width: 1280,
                    inner_height: 800,
                    screen_width: 1920,
                    screen_height: 1080,
                    device_pixel_ratio: 1.5,
                },
                100.0,
            )]
        );
    }

    #[test]
    fn channels_fall_back_when_the_daemon_returns_an_http_error() {
        let backend = FakeBackend::new(Err(PanesWebError::Http {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Dunking Sheep unavailable".into(),
        }));
        let request = request("GET", "/api/channels", None, None);
        let (status, body) =
            response_json(block_on(app(Arc::clone(&backend)).oneshot(request)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["daemon"], false);
        assert_eq!(body["channels"], json!(backend.roster));
        assert_eq!(body["viewports"], json!([{"innerWidth": 1280}]));
        assert_eq!(body["checked_at"], "2026-11-03T00:00:00Z");
        assert_eq!(
            backend.roster_inputs.lock().unwrap().as_slice(),
            &[Vec::<Value>::new()]
        );
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            &[("list_panes".into(), json!({}))]
        );
    }

    #[test]
    fn pane_text_clamps_lines_and_caches_the_read_only_response() {
        let backend = FakeBackend::new(Ok(json!({"text": "pane output"})));
        let app = app(Arc::clone(&backend));
        let first = request("GET", "/api/pane-text?pane=%25demo&lines=500", None, None);
        let (status, body) = response_json(block_on(app.clone().oneshot(first)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pane"], "%demo");
        assert_eq!(body["text"], "pane output");
        assert_eq!(body["readonly"], true);
        assert_eq!(body["note"], PANE_TEXT_READONLY_NOTE);

        let cached = request("GET", "/api/pane-text?pane=%25demo&lines=1", None, None);
        let (status, body) = response_json(block_on(app.oneshot(cached)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"pane": "%demo", "text": "pane output", "readonly": true})
        );
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            &[("read_pane".into(), json!({"target": "%demo", "lines": 200}))]
        );
    }

    #[test]
    fn pane_text_validates_ids_and_reports_daemon_read_failures() {
        let bad_id = FakeBackend::new(Ok(json!({"text": "unused"})));
        let bad_id_request = request("GET", "/api/pane-text?pane=desk", None, None);
        let (status, body) =
            response_json(block_on(app(Arc::clone(&bad_id)).oneshot(bad_id_request)).unwrap());
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, json!({"detail": "a tmux pane id looks like %9"}));
        assert!(bad_id.calls.lock().unwrap().is_empty());

        let failure = FakeBackend::new(Err(PanesWebError::Failure("daemon offline".into())));
        let failed_request = request("GET", "/api/pane-text?pane=%259", None, None);
        let (status, body) = response_json(block_on(app(failure).oneshot(failed_request)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["text"], "");
        assert_eq!(body["readonly"], true);
        assert_eq!(
            body["reason"],
            "the daemon could not read the pane: daemon offline"
        );
    }

    #[test]
    fn pane_text_validates_lines_before_returning_a_warm_cache_entry() {
        let backend = FakeBackend::new(Ok(json!({"text": "cached pane"})));
        let app = app(Arc::clone(&backend));
        let warm = request("GET", "/api/pane-text?pane=%259", None, None);
        let (status, _) = response_json(block_on(app.clone().oneshot(warm)).unwrap());
        assert_eq!(status, StatusCode::OK);

        let invalid = request("GET", "/api/pane-text?pane=%259&lines=nope", None, None);
        let (status, body) = response_json(block_on(app.oneshot(invalid)).unwrap());
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["type"], "int_parsing");
        assert_eq!(body["detail"][0]["loc"], json!(["query", "lines"]));
        assert_eq!(body["detail"][0]["input"], "nope");
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn pane_text_query_parsing_matches_python_percent_and_integer_behavior() {
        assert_eq!(
            parse_query(Some("pane=%259&noise=%ZZ")),
            Ok(HashMap::from([
                ("pane".into(), "%9".into()),
                ("noise".into(), "%ZZ".into()),
            ]))
        );
        assert_eq!(
            parse_query(Some("pane=%FF")),
            Ok(HashMap::from([("pane".into(), "�".into())]))
        );

        assert_eq!(parse_pydantic_int("  +1.00  "), Some(1));
        assert_eq!(parse_pydantic_int("1.0_0"), None);
        assert_eq!(parse_pydantic_int("1_0.00"), Some(10));
        assert_eq!(parse_pydantic_int("1_0"), Some(10));
        assert_eq!(parse_pydantic_int(&"9".repeat(4_300)), Some(201));
        assert_eq!(parse_pydantic_int(&"9".repeat(4_301)), None);
        assert_eq!(parse_pydantic_int("1.5"), None);
        assert_eq!(parse_pydantic_int("1__0"), None);
    }

    #[test]
    fn report_viewport_requires_a_browser_session_and_rejects_cross_origin_requests() {
        let backend = FakeBackend::new(Ok(json!({"panes": []})));
        *backend.viewport_result.lock().unwrap() = Ok(false);
        let app = app(Arc::clone(&backend));
        let valid_body = r#"{"innerWidth":1280,"innerHeight":800,"screenWidth":1920,"screenHeight":1080,"devicePixelRatio":1.5}"#;

        let mut cross_origin = request("POST", "/api/viewport", Some(valid_body), None);
        cross_origin
            .headers_mut()
            .insert(header::ORIGIN, "https://other.test".parse().unwrap());
        let (status, body) = response_json(block_on(app.clone().oneshot(cross_origin)).unwrap());
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
        assert!(backend.viewport_updates.lock().unwrap().is_empty());

        let no_session = request(
            "POST",
            "/api/viewport",
            Some(valid_body),
            Some("vp_session=old"),
        );
        let (status, body) = response_json(block_on(app.oneshot(no_session)).unwrap());
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Browser session required"}));
    }

    #[test]
    fn channels_returns_the_roster_and_marks_the_daemon_available() {
        let backend = FakeBackend::new(Ok(json!({
            "panes": [{"pane_id": "%9", "agent_status": "idle"}]
        })));
        let request = request("GET", "/api/channels", None, None);
        let (status, body) =
            response_json(block_on(app(Arc::clone(&backend)).oneshot(request)).unwrap());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["daemon"], true);
        assert_eq!(body["channels"], json!(backend.roster));
        assert_eq!(
            backend.roster_inputs.lock().unwrap().as_slice(),
            &[vec![json!({"pane_id": "%9", "agent_status": "idle"})]]
        );
    }

    #[test]
    fn channels_surfaces_roster_failures_instead_of_an_empty_roster() {
        let backend = FakeBackend::new(Ok(json!({"panes": [1]})));
        *backend.roster_result.lock().unwrap() =
            Err(PanesWebError::Failure("malformed pane".into()));
        let request = request("GET", "/api/channels", None, None);
        let response = block_on(app(Arc::clone(&backend)).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(body.as_ref(), b"Internal Server Error");
        assert_eq!(backend.roster_inputs.lock().unwrap().len(), 1);
    }

    #[test]
    fn pane_text_matches_missing_and_invalid_query_responses() {
        let backend = FakeBackend::new(Ok(json!({"text": "unused"})));
        let app = app(backend);
        let missing = request("GET", "/api/pane-text", None, None);
        let (status, body) = response_json(block_on(app.clone().oneshot(missing)).unwrap());
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            json!({"detail": [{
                "type": "missing", "loc": ["query", "pane"], "msg": "Field required"
            }]})
        );

        let invalid_lines = request("GET", "/api/pane-text?pane=%259&lines=nope", None, None);
        let (status, body) = response_json(block_on(app.oneshot(invalid_lines)).unwrap());
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["type"], "int_parsing");
        assert_eq!(body["detail"][0]["loc"], json!(["query", "lines"]));
        assert_eq!(body["detail"][0]["input"], "nope");
    }

    #[test]
    fn pane_text_propagates_http_errors_from_the_daemon_client() {
        let backend = FakeBackend::new(Err(PanesWebError::Http {
            status: StatusCode::SERVICE_UNAVAILABLE,
            detail: "Dunking Sheep unavailable".into(),
        }));
        let request = request("GET", "/api/pane-text?pane=%259", None, None);
        let (status, body) = response_json(block_on(app(backend).oneshot(request)).unwrap());
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({"detail": "Dunking Sheep unavailable"}));
    }

    #[test]
    fn pane_text_matches_python_http_parity_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/pane_text_success.json"))
                .expect("valid Python/Rust HTTP parity fixture");
        let request_fixture = &fixture["request"];
        let backend = FakeBackend::new(Ok(json!({"text": "fixture pane"})));
        let request = request(
            request_fixture["method"].as_str().unwrap(),
            request_fixture["path"].as_str().unwrap(),
            None,
            None,
        );
        let response = block_on(app(backend).oneshot(request)).unwrap();
        let expected = &fixture["response"];
        assert_eq!(
            response.status().as_u16(),
            expected["status"].as_u64().unwrap() as u16
        );
        for (name, value) in expected["headers"].as_object().unwrap() {
            assert_eq!(
                response.headers().get(name).unwrap().to_str().unwrap(),
                value.as_str().unwrap(),
                "header {name}"
            );
        }
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        let raw = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(raw, expected["body"].as_str().unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            expected["json"]
        );
    }
}
