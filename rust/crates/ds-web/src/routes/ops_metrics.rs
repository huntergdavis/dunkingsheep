//! Operations metrics routes ported from `web/app.py`.
//!
//! Collection, database access, and the periodically refreshed board stay behind a
//! narrow backend boundary. The handlers own authentication, query validation, and
//! Python-compatible HTTP error shaping.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

const HISTORY_WINDOW_ERROR: &str = "History window must be 1 to 168 hours";

pub type OpsMetricsFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The three read/refresh operations exposed by `web/app.py`.
///
/// The backend owns database connection lifetimes and the board cache. It reports the
/// Python exception class name so the route can preserve the public error text without
/// exposing exception details.
pub trait OpsMetricsBackend: Send + Sync + 'static {
    fn history<'a>(&'a self, hours: u16) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>>;
    fn refresh<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>>;
    fn board_snapshot<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpsMetricsError {
    Database { exception_type: String },
    Other { exception_type: String },
}

impl OpsMetricsError {
    pub fn database(exception_type: impl Into<String>) -> Self {
        Self::Database {
            exception_type: exception_type.into(),
        }
    }

    pub fn other(exception_type: impl Into<String>) -> Self {
        Self::Other {
            exception_type: exception_type.into(),
        }
    }

    fn exception_type(&self) -> &str {
        match self {
            Self::Database { exception_type } | Self::Other { exception_type } => exception_type,
        }
    }

    fn database_error_response(&self, detail: String) -> Response {
        match self {
            Self::Database { .. } => detail_response(StatusCode::SERVICE_UNAVAILABLE, detail),
            Self::Other { .. } => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
            }
        }
    }
}

pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/ops-metrics/history", get(operations_metrics_history))
        .route("/api/ops-metrics/refresh", post(refresh_operations_metrics))
        .route("/api/ops-board", get(operations_board))
        .layer(Extension(scheme))
}

async fn operations_metrics_history(
    Extension(backend): Extension<Arc<dyn OpsMetricsBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query"),
    };
    let hours_raw = query.get("hours").map(String::as_str);
    let hours = match parse_hours(hours_raw) {
        Ok(hours) => hours,
        Err(response) => return response,
    };
    match backend.history(hours).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.database_error_response(format!(
            "Operations metrics history unavailable: {}",
            error.exception_type()
        )),
    }
}

async fn refresh_operations_metrics(
    Extension(backend): Extension<Arc<dyn OpsMetricsBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    match backend.refresh().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.database_error_response(format!(
            "Operations metrics unavailable: {}",
            error.exception_type()
        )),
    }
}

async fn operations_board(
    Extension(backend): Extension<Arc<dyn OpsMetricsBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    match backend.board_snapshot().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => detail_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Operations board unavailable: {}", error.exception_type()),
        ),
    }
}

fn parse_hours(raw: Option<&str>) -> Result<u16, Response> {
    let Some(raw) = raw else { return Ok(24) };
    let parsed = raw.trim().parse::<i128>();
    let hours = match parsed {
        Ok(hours) => hours,
        Err(_) => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": [{
                    "type": "int_parsing",
                    "loc": ["query", "hours"],
                    "msg": "Input should be a valid integer, unable to parse string as an integer",
                    "input": raw
                }]})),
            )
                .into_response());
        }
    };
    if !(1..=168).contains(&hours) {
        return Err(detail_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            HISTORY_WINDOW_ERROR,
        ));
    }
    Ok(hours as u16)
}

async fn authenticate(
    backend: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<Identity, Response> {
    if let Some(response) = same_origin_refusal(headers, scheme) {
        return Err(response);
    }
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers, "vp_session");
    backend
        .identity(bearer.as_deref(), cookie.as_deref())
        .await
        .map_err(auth_error_response)
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

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
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
        AuthError::Internal(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn parse_query(raw: Option<&str>) -> Result<std::collections::HashMap<String, String>, ()> {
    fn decode(component: &str) -> Result<String, ()> {
        let bytes = component.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).map_err(|_| ())?;
                    decoded.push(u8::from_str_radix(hex, 16).map_err(|_| ())?);
                    index += 3;
                }
                b'%' => return Err(()),
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8(decoded).map_err(|_| ())
    }

    let mut values = std::collections::HashMap::new();
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values.insert(decode(key)?, decode(value)?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request},
    };
    use tower::ServiceExt;

    use super::super::auth::AuthFuture;
    use super::*;

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

    struct FakeAuth {
        identity: Mutex<Result<Identity, AuthError>>,
        seen: Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl FakeAuth {
        fn accepting() -> Self {
            Self {
                identity: Mutex::new(Ok(Identity::Owner)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected login".into())) })
        }
        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            let bearer = bearer.map(str::to_owned);
            let cookie = cookie.map(str::to_owned);
            Box::pin(async move {
                self.seen.lock().unwrap().push((bearer, cookie));
                self.identity.lock().unwrap().clone()
            })
        }
        fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected logout".into())) })
        }
        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected password change".into())) })
        }
    }

    #[derive(Default)]
    struct FakeOpsMetrics {
        calls: Mutex<Vec<(String, Option<u16>)>>,
        history_result: Mutex<Option<Result<Value, OpsMetricsError>>>,
        refresh_result: Mutex<Option<Result<Value, OpsMetricsError>>>,
        board_result: Mutex<Option<Result<Value, OpsMetricsError>>>,
    }

    impl OpsMetricsBackend for FakeOpsMetrics {
        fn history<'a>(
            &'a self,
            hours: u16,
        ) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
            self.calls
                .lock()
                .unwrap()
                .push(("history".into(), Some(hours)));
            let result = self
                .history_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(json!({"window_hours": hours})));
            Box::pin(async move { result })
        }
        fn refresh<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
            self.calls.lock().unwrap().push(("refresh".into(), None));
            let result = self
                .refresh_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(json!({"state": "refreshed"})));
            Box::pin(async move { result })
        }
        fn board_snapshot<'a>(&'a self) -> OpsMetricsFuture<'a, Result<Value, OpsMetricsError>> {
            self.calls.lock().unwrap().push(("board".into(), None));
            let result = self
                .board_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(json!({"state": "ok"})));
            Box::pin(async move { result })
        }
    }

    fn app(backend: Arc<FakeOpsMetrics>, auth: Arc<FakeAuth>) -> Router {
        router()
            .layer(Extension(backend as Arc<dyn OpsMetricsBackend>))
            .layer(Extension(auth as Arc<dyn AuthBackend>))
    }

    fn request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "desk.test")
            .body(Body::empty())
            .unwrap()
    }

    async fn response_json(response: Response) -> (u16, Value) {
        let status = response.status().as_u16();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn operations_metrics_history_defaults_and_forwards_valid_window() {
        let backend = Arc::new(FakeOpsMetrics::default());
        let auth = Arc::new(FakeAuth::accepting());
        let app = app(backend.clone(), auth);
        let response = block_on(
            app.clone()
                .oneshot(request("GET", "/api/ops-metrics/history")),
        )
        .unwrap();
        assert_eq!(
            (200, json!({"window_hours": 24})),
            block_on(response_json(response))
        );
        let response =
            block_on(app.oneshot(request("GET", "/api/ops-metrics/history?hours=168"))).unwrap();
        assert_eq!(
            (200, json!({"window_hours": 168})),
            block_on(response_json(response))
        );
        assert_eq!(
            vec![("history".into(), Some(24)), ("history".into(), Some(168))],
            *backend.calls.lock().unwrap()
        );
    }

    #[test]
    fn operations_metrics_history_rejects_invalid_windows_without_backend_call() {
        let backend = Arc::new(FakeOpsMetrics::default());
        let auth = Arc::new(FakeAuth::accepting());
        let app = app(backend.clone(), auth);
        let response = block_on(
            app.clone()
                .oneshot(request("GET", "/api/ops-metrics/history?hours=169")),
        )
        .unwrap();
        assert_eq!(
            (422, json!({"detail": HISTORY_WINDOW_ERROR})),
            block_on(response_json(response))
        );
        let response =
            block_on(app.oneshot(request("GET", "/api/ops-metrics/history?hours=nope"))).unwrap();
        assert_eq!(
            (
                422,
                json!({"detail": [{"type":"int_parsing","loc":["query","hours"],"msg":"Input should be a valid integer, unable to parse string as an integer","input":"nope"}]})
            ),
            block_on(response_json(response))
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn operations_metrics_refresh_maps_backend_failures_and_returns_data() {
        let backend = Arc::new(FakeOpsMetrics::default());
        *backend.refresh_result.lock().unwrap() =
            Some(Err(OpsMetricsError::database("OperationalError")));
        let auth = Arc::new(FakeAuth::accepting());
        let app = app(backend.clone(), auth);
        let response = block_on(
            app.clone()
                .oneshot(request("POST", "/api/ops-metrics/refresh")),
        )
        .unwrap();
        assert_eq!(
            (
                503,
                json!({"detail":"Operations metrics unavailable: OperationalError"})
            ),
            block_on(response_json(response))
        );
        *backend.refresh_result.lock().unwrap() = None;
        let response = block_on(app.oneshot(request("POST", "/api/ops-metrics/refresh"))).unwrap();
        assert_eq!(
            (200, json!({"state":"refreshed"})),
            block_on(response_json(response))
        );
        assert_eq!(
            vec![("refresh".into(), None), ("refresh".into(), None)],
            *backend.calls.lock().unwrap()
        );
    }

    #[test]
    fn operations_board_returns_cached_snapshot_and_hides_exception_details() {
        let backend = Arc::new(FakeOpsMetrics::default());
        *backend.board_result.lock().unwrap() = Some(Err(OpsMetricsError::other("RuntimeError")));
        let auth = Arc::new(FakeAuth::accepting());
        let app = app(backend.clone(), auth);
        let response = block_on(app.clone().oneshot(request("GET", "/api/ops-board"))).unwrap();
        assert_eq!(
            (
                503,
                json!({"detail":"Operations board unavailable: RuntimeError"})
            ),
            block_on(response_json(response))
        );
        *backend.board_result.lock().unwrap() = None;
        let response = block_on(app.oneshot(request("GET", "/api/ops-board"))).unwrap();
        assert_eq!(
            (200, json!({"state":"ok"})),
            block_on(response_json(response))
        );
        assert_eq!(
            vec![("board".into(), None), ("board".into(), None)],
            *backend.calls.lock().unwrap()
        );
    }
}
