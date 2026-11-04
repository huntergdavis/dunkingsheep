//! Operations board deferral and refresh routes from `web/app.py`.
//!
//! Daemon calls and the board cache stay behind an injected backend so these
//! handlers can be exercised without a live daemon or operations database.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, RequestScheme};
use super::messages_write::authenticate;
use super::response_error::WebResponseError;

pub type OpsBoardFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The two operations needed by this route group.
pub trait OpsBoardBackend: Send + Sync + 'static {
    fn defer_question<'a>(
        &'a self,
        task_id: &'a str,
    ) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>>;

    fn refresh<'a>(&'a self) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpsBoardError {
    Failure(String),
}

/// Mount the deferral and refresh routes with injected authentication and board services.
pub fn router(
    backend: Arc<dyn OpsBoardBackend>,
    auth_backend: Arc<dyn AuthBackend>,
    request_scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/ops-board/not-now", post(not_now_owner_question))
        .route("/api/ops-board/refresh", post(refresh_operations_board))
        .layer(Extension(backend))
        .layer(Extension(auth_backend))
        .layer(Extension(request_scheme))
}

async fn not_now_owner_question(
    Extension(backend): Extension<Arc<dyn OpsBoardBackend>>,
    Extension(auth_backend): Extension<Arc<dyn AuthBackend>>,
    Extension(request_scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    // FastAPI decodes JSON before dependencies, then validates the dict after auth.
    let payload = match request_payload(request).await {
        Ok(payload) => payload,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authenticate(auth_backend.as_ref(), &headers, request_scheme).await {
        return response.into_response();
    }
    let payload = match payload {
        None => return body_validation("missing", "Field required", Value::Null),
        Some(RequestPayload::Json(value)) => value,
        Some(RequestPayload::Raw(bytes)) => {
            // FastAPI validates raw bytes after dependencies; its error encoder is strict UTF-8.
            return match String::from_utf8(bytes) {
                Ok(text) => body_validation(
                    "dict_type",
                    "Input should be a valid dictionary",
                    Value::String(text),
                ),
                Err(_) => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
                }
            };
        }
    };
    let Some(payload) = payload.as_object() else {
        return body_validation("dict_type", "Input should be a valid dictionary", payload);
    };
    let task_value = payload.get("task_id");
    let task_id = if python_truthy(task_value) {
        ds_core::api::python_value_string(task_value.expect("truthy present value"))
            .trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
            .to_owned()
    } else {
        String::new()
    };
    if task_id.is_empty() {
        return detail_response(StatusCode::BAD_REQUEST, "task_id is required");
    }
    match backend.defer_question(&task_id).await {
        Ok(task) => {
            Json(json!({"task_id": task_id, "priority": "low", "task": task})).into_response()
        }
        Err(error) => detail_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Could not lower {task_id}: {}", error.kind()),
        ),
    }
}

async fn refresh_operations_board(
    Extension(backend): Extension<Arc<dyn OpsBoardBackend>>,
    Extension(auth_backend): Extension<Arc<dyn AuthBackend>>,
    Extension(request_scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticate(auth_backend.as_ref(), &headers, request_scheme).await {
        return response.into_response();
    }
    match backend.refresh().await {
        Ok(board) => Json(board).into_response(),
        Err(error) => detail_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Operations board unavailable: {}", error.kind()),
        ),
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

enum RequestPayload {
    Json(Value),
    Raw(Vec<u8>),
}

async fn request_payload(request: Request) -> Result<Option<RequestPayload>, WebResponseError> {
    use super::game_state::{
        decode_python_json_bytes, python_json_decode_error,
        python_json_integer_exceeds_digit_limit, request_json_content_type,
    };
    let is_json = request_json_content_type(request.headers());
    let bytes = to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| {
            detail_response(
                StatusCode::BAD_REQUEST,
                "There was an error parsing the body",
            )
        })?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if !is_json {
        return Ok(Some(RequestPayload::Raw(bytes.to_vec())));
    }
    let bytes = decode_python_json_bytes(&bytes).map_err(|_| {
        detail_response(
            StatusCode::BAD_REQUEST,
            "There was an error parsing the body",
        )
    })?;
    if python_json_integer_exceeds_digit_limit(&bytes) {
        return Err(detail_response(
            StatusCode::BAD_REQUEST,
            "There was an error parsing the body",
        )
        .into());
    }
    let text = std::str::from_utf8(&bytes).expect("decoded JSON is UTF-8");
    ds_core::wire_json::from_str(text)
        .map(|value| Some(RequestPayload::Json(value)))
        .map_err(|error| python_json_decode_error(&bytes, &error).into())
}

fn body_validation(kind: &str, message: &str, input: Value) -> Response {
    // Starlette's strict JSONResponse rejects nonfinite validation-error inputs.
    if has_nonfinite_float(&input) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
    }
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": kind, "loc": ["body"], "msg": message, "input": input
        }]})),
    )
        .into_response()
}

fn has_nonfinite_float(value: &Value) -> bool {
    match value {
        Value::Number(number) => {
            let raw = number.to_string();
            (raw.contains(['.', 'e', 'E'])
                || matches!(raw.as_str(), "NaN" | "Infinity" | "-Infinity"))
                && raw.parse::<f64>().is_ok_and(|number| !number.is_finite())
        }
        Value::Array(values) => values.iter().any(has_nonfinite_float),
        Value::Object(values) => values.values().any(has_nonfinite_float),
        _ => false,
    }
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(value)) => value
            .to_string()
            .parse::<f64>()
            .is_ok_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

impl OpsBoardError {
    fn kind(&self) -> &str {
        match self {
            Self::Failure(kind) => kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::auth::{AuthError, Identity};
    use std::{
        future::Future,
        pin::pin,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;

    #[derive(Default)]
    struct SeenAuth {
        bearer: Mutex<Option<String>>,
        cookie: Mutex<Option<String>>,
    }

    struct FakeAuth {
        fail_identity: bool,
        identity_calls: Arc<AtomicUsize>,
        seen: Arc<SeenAuth>,
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }
        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            self.identity_calls.fetch_add(1, Ordering::SeqCst);
            *self.seen.bearer.lock().unwrap() = bearer.map(str::to_owned);
            *self.seen.cookie.lock().unwrap() = cookie.map(str::to_owned);
            let result = if self.fail_identity {
                Err(AuthError::SignInRequired)
            } else {
                Ok(Identity::Owner)
            };
            Box::pin(async move { result })
        }
        fn logout<'a>(
            &'a self,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Default)]
    struct FakeBoard {
        deferred: Mutex<Vec<String>>,
        refresh_calls: AtomicUsize,
        fail_defer: bool,
        fail_refresh: bool,
    }

    impl OpsBoardBackend for FakeBoard {
        fn defer_question<'a>(
            &'a self,
            task_id: &'a str,
        ) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
            Box::pin(async move {
                if self.fail_defer {
                    return Err(OpsBoardError::Failure("RuntimeError".into()));
                }
                self.deferred.lock().unwrap().push(task_id.to_owned());
                Ok(json!({"id": task_id, "priority": "low"}))
            })
        }

        fn refresh<'a>(&'a self) -> OpsBoardFuture<'a, Result<Value, OpsBoardError>> {
            Box::pin(async move {
                self.refresh_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_refresh {
                    return Err(OpsBoardError::Failure("RuntimeError".into()));
                }
                Ok(json!({"generated_at": "fixture", "items": []}))
            })
        }
    }

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

    fn app(board: Arc<FakeBoard>) -> Router {
        let board: Arc<dyn OpsBoardBackend> = board;
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth {
            fail_identity: false,
            identity_calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::default(),
        });
        router(board, auth, RequestScheme::Http)
    }

    #[test]
    fn routes_reject_cross_origin_and_unauthenticated_requests_before_backend_calls() {
        let board = Arc::new(FakeBoard::default());
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/refresh")
            .header(header::HOST, "desk.test")
            .header(header::ORIGIN, "https://other.test")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(board.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"detail": "Cross-origin request refused"})
        );
        assert_eq!(board.refresh_calls.load(Ordering::SeqCst), 0);

        let board = Arc::new(FakeBoard::default());
        let board_backend: Arc<dyn OpsBoardBackend> = board.clone();
        let auth_backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth {
            fail_identity: true,
            identity_calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::default(),
        });
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/refresh")
            .body(Body::empty())
            .unwrap();
        let response =
            block_on(router(board_backend, auth_backend, RequestScheme::Http).oneshot(request))
                .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"detail": "Sign in required"})
        );
        assert_eq!(board.refresh_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ops_board_reuses_latin1_and_last_cookie_auth_semantics() {
        let board = Arc::new(FakeBoard::default());
        let identity_calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(SeenAuth::default());
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth {
            fail_identity: false,
            identity_calls: identity_calls.clone(),
            seen: seen.clone(),
        });
        let backend: Arc<dyn OpsBoardBackend> = board.clone();
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/not-now")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, "Bearer owner-token")
            .header(header::COOKIE, "vp_session=first")
            .header(header::COOKIE, r#"vp_session="\101""#)
            .body(Body::from(r#"{"task_id":"t9"}"#))
            .unwrap();
        let response =
            block_on(router(backend, auth, RequestScheme::Http).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*seen.bearer.lock().unwrap(), Some("owner-token".to_owned()));
        assert_eq!(*seen.cookie.lock().unwrap(), Some("A".to_owned()));

        let board = Arc::new(FakeBoard::default());
        let identity_calls = Arc::new(AtomicUsize::new(0));
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth {
            fail_identity: false,
            identity_calls: identity_calls.clone(),
            seen: Arc::default(),
        });
        let backend: Arc<dyn OpsBoardBackend> = board.clone();
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/not-now")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "desk.test")
            .header(
                header::ORIGIN,
                axum::http::HeaderValue::from_bytes(b"http://d\xe9sk.test").unwrap(),
            )
            .body(Body::from(r#"{"task_id":"t9"}"#))
            .unwrap();
        let response =
            block_on(router(backend, auth, RequestScheme::Http).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(identity_calls.load(Ordering::SeqCst), 0);
        assert!(board.deferred.lock().unwrap().is_empty());
    }

    #[test]
    fn not_now_matches_actual_fastapi_raw_payload_and_validation_order() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ops_board_request_matrix.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let board = Arc::new(FakeBoard::default());
            let identity_calls = Arc::new(AtomicUsize::new(0));
            let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth {
                fail_identity: case["fail_auth"].as_bool().unwrap(),
                identity_calls: identity_calls.clone(),
                seen: Arc::default(),
            });
            let backend: Arc<dyn OpsBoardBackend> = board.clone();
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/ops-board/not-now");
            if let Some(content_type) = case["content_type"].as_str() {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            let request = request
                .body(Body::from(
                    case["body_hex"]
                        .as_str()
                        .unwrap()
                        .as_bytes()
                        .chunks_exact(2)
                        .map(|pair| {
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
                        })
                        .collect::<Vec<_>>(),
                ))
                .unwrap();
            let response =
                block_on(router(backend, auth, RequestScheme::Http).oneshot(request)).unwrap();
            assert_eq!(
                response.status().as_u16(),
                case["status"].as_u64().unwrap() as u16,
                "{case}"
            );
            let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes)
                    .unwrap_or_else(|_| Value::String(String::from_utf8(bytes.to_vec()).unwrap())),
                case["json"],
                "{case}"
            );
            let calls = case["calls"].as_array().unwrap();
            assert_eq!(
                identity_calls.load(Ordering::SeqCst),
                usize::from(calls.iter().any(|call| call == "auth")),
                "{case}"
            );
            let deferred: Vec<String> = calls
                .iter()
                .filter_map(|call| call.as_array())
                .map(|call| call[1].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(*board.deferred.lock().unwrap(), deferred, "{case}");
        }
    }

    #[test]
    fn not_now_owner_question_lowers_priority_and_returns_task() {
        let board = Arc::new(FakeBoard::default());
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/not-now")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"task_id":"t9"}"#))
            .unwrap();
        let response = block_on(app(board.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({
                "task_id": "t9", "priority": "low", "task": {"id": "t9", "priority": "low"}
            })
        );
        assert_eq!(*board.deferred.lock().unwrap(), ["t9"]);
    }

    #[test]
    fn not_now_owner_question_validates_and_maps_backend_errors() {
        let board = Arc::new(FakeBoard {
            fail_defer: true,
            ..FakeBoard::default()
        });
        for (body, status, expected) in [
            (
                r#"{}"#,
                StatusCode::BAD_REQUEST,
                json!({"detail":"task_id is required"}),
            ),
            (
                r#"{"task_id":"t9"}"#,
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"detail":"Could not lower t9: RuntimeError"}),
            ),
        ] {
            let request = Request::builder()
                .method("POST")
                .uri("/api/ops-board/not-now")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap();
            let response = block_on(app(board.clone()).oneshot(request)).unwrap();
            assert_eq!(response.status(), status);
            let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn refresh_operations_board_returns_board_and_maps_errors() {
        let board = Arc::new(FakeBoard::default());
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/refresh")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(board.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"generated_at":"fixture","items":[]})
        );
        assert_eq!(board.refresh_calls.load(Ordering::SeqCst), 1);

        let board = Arc::new(FakeBoard {
            fail_refresh: true,
            ..FakeBoard::default()
        });
        let request = Request::builder()
            .method("POST")
            .uri("/api/ops-board/refresh")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(board).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"detail":"Operations board unavailable: RuntimeError"})
        );
    }
}
