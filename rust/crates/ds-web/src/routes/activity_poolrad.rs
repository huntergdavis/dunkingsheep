//! Pane activity and Poolrad progress endpoints from `web/app.py`.
//!
//! Pane reads, dashboard loading, accounting, and cache state are injected so the
//! HTTP handlers can be exercised without contacting a terminal or dashboard checkout.

use std::sync::Arc;

use axum::{
    extract::{Extension, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, RequestScheme};

const PANES_ACTIVITY_TTL_S: u64 = 4;
const POOLRAD_PROGRESS_TTL_S: u64 = 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityPoolradError {
    /// The underlying Python route surfaces a stable HTTP error for this failure.
    Http { status: StatusCode, detail: String },
    /// Unexpected exceptions are returned as a generic 500.
    Failure,
}

impl ActivityPoolradError {
    fn into_response(self) -> Response {
        match self {
            Self::Http { status, detail } => detail_response(status, detail),
            Self::Failure => internal_error(),
        }
    }
}

/// Operations supplied by the application runtime. Pane activity returns `None` when
/// the shared refresh has failed; channel activity returns `None` when no pane matches.
pub trait ActivityPoolradBackend: Send + Sync + 'static {
    fn panes_activity(&self) -> Result<Option<Value>, ActivityPoolradError>;
    fn activity(&self, channel: &str) -> Result<Option<Value>, ActivityPoolradError>;
    fn poolrad_progress(&self) -> Result<Option<Value>, ActivityPoolradError>;
}

pub fn router(
    backend: Arc<dyn ActivityPoolradBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/panes/activity", get(panes_activity))
        .route("/api/activity", get(activity))
        .route(
            "/api/poolrad-dashboard/progress",
            get(poolrad_dashboard_progress),
        )
        .layer(Extension(scheme))
        .layer(Extension(auth))
        .layer(Extension(backend))
}

async fn panes_activity(
    Extension(backend): Extension<Arc<dyn ActivityPoolradBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return with_no_store(response);
    }
    let data = match backend.panes_activity() {
        Ok(Some(mut data)) => {
            if let Some(object) = data.as_object_mut() {
                object.insert("refresh_after_seconds".into(), json!(PANES_ACTIVITY_TTL_S));
            }
            data
        }
        Ok(None) | Err(_) => {
            return with_no_store(detail_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Herdr panes unavailable",
            ));
        }
    };
    with_no_store(Json(data).into_response())
}

async fn activity(
    Extension(backend): Extension<Arc<dyn ActivityPoolradBackend>>,
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
    let Some(channel) = query.get("channel") else {
        return validation_error(json!({
            "type": "missing", "loc": ["query", "channel"],
            "msg": "Field required", "input": null
        }));
    };
    let mut result = match backend.activity(channel) {
        Ok(Some(result)) => result,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Pane unavailable"),
        Err(error) => return error.into_response(),
    };
    let Some(object) = result.as_object_mut() else {
        return internal_error();
    };
    object.insert("channel".into(), Value::String(channel.clone()));
    Json(result).into_response()
}

async fn poolrad_dashboard_progress(
    Extension(backend): Extension<Arc<dyn ActivityPoolradBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let mut view = match backend.poolrad_progress() {
        Ok(Some(view)) => view,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Poolrad dashboard unavailable"),
        Err(ActivityPoolradError::Http { status, detail }) => {
            return detail_response(status, detail);
        }
        Err(ActivityPoolradError::Failure) => {
            return detail_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Poolrad dashboard progress unavailable",
            );
        }
    };
    let Some(object) = view.as_object_mut() else {
        return internal_error();
    };
    object.insert(
        "refresh_after_seconds".into(),
        json!(POOLRAD_PROGRESS_TTL_S),
    );
    Json(view).into_response()
}

fn with_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<(), Response> {
    if let Some(value) = headers.get(header::ORIGIN) {
        let origin = value.to_str().map_err(|_| cross_origin())?;
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let expected = match scheme {
            RequestScheme::Http => "http",
            RequestScheme::Https => "https",
        };
        if !origin.is_empty() && origin != format!("{expected}://{host}") {
            return Err(cross_origin());
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value.split(';').find_map(|part| {
                let (name, value) = part.trim().split_once('=')?;
                (name.trim() == "vp_session").then(|| value.trim())
            })
        });
    auth.identity(bearer, cookie)
        .await
        .map(|_| ())
        .map_err(auth_error_response)
}

fn parse_query(raw: Option<&str>) -> Result<std::collections::BTreeMap<String, String>, ()> {
    fn decode(text: &str) -> Result<String, ()> {
        let bytes = text.as_bytes();
        let mut result = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    result.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = |byte: u8| match byte {
                        b'0'..=b'9' => Some(byte - b'0'),
                        b'a'..=b'f' => Some(byte - b'a' + 10),
                        b'A'..=b'F' => Some(byte - b'A' + 10),
                        _ => None,
                    };
                    result.push(
                        hex(bytes[index + 1]).ok_or(())? * 16 + hex(bytes[index + 2]).ok_or(())?,
                    );
                    index += 3;
                }
                b'%' => return Err(()),
                byte => {
                    result.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8(result).map_err(|_| ())
    }
    let mut values = std::collections::BTreeMap::new();
    for pair in raw.unwrap_or("").split('&').filter(|part| !part.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        values.insert(decode(key)?, decode(value)?);
    }
    Ok(values)
}

fn cross_origin() -> Response {
    detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused")
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
        AuthError::Internal(_) => internal_error(),
    }
}

fn validation_error(error: Value) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [error]})),
    )
        .into_response()
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;

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

    #[derive(Default)]
    struct FakeAuth;

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            let accepted = bearer.or(cookie) == Some("owner-token");
            Box::pin(async move {
                if accepted {
                    Ok(super::super::auth::Identity::Owner)
                } else {
                    Err(AuthError::SignInRequired)
                }
            })
        }
        fn logout<'a>(
            &'a self,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<String>>,
        panes: Option<Result<Option<Value>, ActivityPoolradError>>,
        channel: Option<Result<Option<Value>, ActivityPoolradError>>,
        progress: Option<Result<Option<Value>, ActivityPoolradError>>,
    }

    impl ActivityPoolradBackend for FakeBackend {
        fn panes_activity(&self) -> Result<Option<Value>, ActivityPoolradError> {
            self.calls.lock().unwrap().push("panes".into());
            self.panes
                .clone()
                .unwrap_or_else(|| Ok(Some(json!({"panes":[], "checked_at":"now"}))))
        }
        fn activity(&self, channel: &str) -> Result<Option<Value>, ActivityPoolradError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("activity:{channel}"));
            self.channel.clone().unwrap_or_else(|| {
                Ok(Some(
                    json!({"pane_id":"%1", "status":"working", "text":"output"}),
                ))
            })
        }
        fn poolrad_progress(&self) -> Result<Option<Value>, ActivityPoolradError> {
            self.calls.lock().unwrap().push("progress".into());
            self.progress.clone().unwrap_or_else(|| {
                Ok(Some(
                    json!({"percent": 42, "replacements":{"state":"ok","count":3}}),
                ))
            })
        }
    }

    fn app(backend: Arc<FakeBackend>) -> Router {
        router(backend, Arc::new(FakeAuth), RequestScheme::Http)
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .body(Body::empty())
            .unwrap()
    }

    fn json_response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn panes_activity_returns_snapshot_with_ttl_and_no_store() {
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(app(backend).oneshot(request("/api/panes/activity"))).unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"panes":[], "checked_at":"now", "refresh_after_seconds":4})
        );
    }

    #[test]
    fn pane_activity_failure_is_generic_503_and_not_cached() {
        let backend = Arc::new(FakeBackend {
            panes: Some(Err(ActivityPoolradError::Failure)),
            ..FakeBackend::default()
        });
        let response = block_on(app(backend).oneshot(request("/api/panes/activity"))).unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, json!({"detail":"Herdr panes unavailable"}));
    }

    #[test]
    fn channel_activity_requires_channel_and_merges_channel_into_result() {
        let backend = Arc::new(FakeBackend::default());
        let missing = block_on(app(backend.clone()).oneshot(request("/api/activity"))).unwrap();
        assert_eq!(json_response(missing).0, StatusCode::UNPROCESSABLE_ENTITY);
        let response =
            block_on(app(backend.clone()).oneshot(request("/api/activity?channel=worker%2D1")))
                .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"channel":"worker-1", "pane_id":"%1", "status":"working", "text":"output"})
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["activity:worker-1"]);
    }

    #[test]
    fn channel_activity_maps_missing_pane_to_404_and_backend_failure_to_500() {
        let missing = Arc::new(FakeBackend {
            channel: Some(Ok(None)),
            ..FakeBackend::default()
        });
        let missing_response =
            block_on(app(missing).oneshot(request("/api/activity?channel=off"))).unwrap();
        assert_eq!(
            json_response(missing_response),
            (StatusCode::NOT_FOUND, json!({"detail":"Pane unavailable"}))
        );
        let failed = Arc::new(FakeBackend {
            channel: Some(Err(ActivityPoolradError::Failure)),
            ..FakeBackend::default()
        });
        assert_eq!(
            block_on(app(failed).oneshot(request("/api/activity?channel=off")))
                .unwrap()
                .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn poolrad_progress_sets_refresh_interval_and_maps_unavailable_states() {
        let backend = Arc::new(FakeBackend::default());
        let response =
            block_on(app(backend).oneshot(request("/api/poolrad-dashboard/progress"))).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"percent":42,"replacements":{"state":"ok","count":3},"refresh_after_seconds":60})
        );

        let absent = Arc::new(FakeBackend {
            progress: Some(Ok(None)),
            ..FakeBackend::default()
        });
        let response =
            block_on(app(absent).oneshot(request("/api/poolrad-dashboard/progress"))).unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail":"Poolrad dashboard unavailable"})
            )
        );
        let failed = Arc::new(FakeBackend {
            progress: Some(Err(ActivityPoolradError::Failure)),
            ..FakeBackend::default()
        });
        let response =
            block_on(app(failed).oneshot(request("/api/poolrad-dashboard/progress"))).unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"detail":"Poolrad dashboard progress unavailable"})
            )
        );
    }

    #[test]
    fn activity_routes_require_authentication() {
        let backend = Arc::new(FakeBackend::default());
        let request = Request::builder()
            .method("GET")
            .uri("/api/activity?channel=worker")
            .header("host", "desk.test")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::UNAUTHORIZED,
                json!({"detail":"Sign in required"})
            )
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
