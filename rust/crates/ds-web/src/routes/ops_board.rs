//! Operations board deferral and refresh routes from `web/app.py`.
//!
//! Daemon calls and the board cache stay behind an injected backend so these
//! handlers can be exercised without a live daemon or operations database.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    extract::Extension,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

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
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = authenticate(auth_backend.as_ref(), &headers, request_scheme).await {
        return response;
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return detail_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid payload"),
    };
    let Some(payload) = payload.as_object() else {
        return detail_response(StatusCode::UNPROCESSABLE_ENTITY, "invalid payload");
    };
    let task_value = payload.get("task_id");
    let task_id = if python_truthy(task_value) {
        python_string(task_value).trim().to_owned()
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
        return response;
    }
    match backend.refresh().await {
        Ok(board) => Json(board).into_response(),
        Err(error) => detail_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Operations board unavailable: {}", error.kind()),
        ),
    }
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
        AuthError::IncorrectPassword | AuthError::SignInRequired => {
            detail_response(StatusCode::UNAUTHORIZED, "Sign in required")
        }
        AuthError::Internal(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn python_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.to_owned(),
        Some(value) => value.to_string(),
    }
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
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

    struct FakeAuth;

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
            _: Option<&'a str>,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async { Ok(Identity::Owner) })
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
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth);
        router(board, auth, RequestScheme::Http)
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
