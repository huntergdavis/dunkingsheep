//! Poolrad game lifecycle routes from `web/app.py`.
//!
//! The game process and its status remain behind an injected backend so the
//! handlers can be exercised with deterministic state and without starting an
//! emulator. Runtime wiring stays with the web acceptance task.

use std::{fs, path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    extract::{Extension, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity};

/// Operations supplied by the runtime adapter for the private game session.
pub trait GameLifecycleBackend: Send + Sync + 'static {
    fn status(&self) -> Result<Value, String>;
    fn start(&self, fresh: bool) -> Result<Value, String>;
}

#[derive(Clone)]
pub struct GameLifecycleState {
    codewheel_dir: PathBuf,
    backend: Arc<dyn GameLifecycleBackend>,
}

impl GameLifecycleState {
    pub fn new(codewheel_dir: impl Into<PathBuf>, backend: Arc<dyn GameLifecycleBackend>) -> Self {
        Self {
            codewheel_dir: codewheel_dir.into(),
            backend,
        }
    }
}

/// Mount only the lifecycle routes. The caller supplies the auth backend;
/// production registration is intentionally left to the runtime task.
pub fn router(state: GameLifecycleState, auth_backend: Arc<dyn AuthBackend>) -> Router {
    Router::new()
        .route("/api/game/status", get(game_status))
        .route("/api/game/codewheel/{filename}", get(game_codewheel_image))
        .route("/api/game/start", post(game_start))
        .with_state(state)
        .layer(Extension(auth_backend))
}

async fn game_status(
    State(state): State<GameLifecycleState>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    headers: HeaderMap,
) -> Response {
    if let Err(error) = authenticate(&auth, &headers).await {
        return auth_error_response(error);
    }
    match state.backend.status() {
        Ok(status) => Json(status).into_response(),
        Err(_) => internal_error(),
    }
}

async fn game_codewheel_image(
    State(state): State<GameLifecycleState>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> Response {
    if let Err(error) = authenticate(&auth, &headers).await {
        return auth_error_response(error);
    }
    if !valid_codewheel_filename(&filename) {
        return route_error(StatusCode::NOT_FOUND, "Unknown rune template");
    }

    let path = state.codewheel_dir.join(&filename);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return route_error(StatusCode::NOT_FOUND, "Private rune template unavailable");
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return route_error(StatusCode::NOT_FOUND, "Private rune template unavailable");
    }
    let content_length = metadata.len();
    let Ok(contents) = fs::read(path) else {
        return route_error(StatusCode::NOT_FOUND, "Private rune template unavailable");
    };

    let mut response = Response::new(Body::from(contents));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/gif"));
    if let Ok(value) = HeaderValue::from_str(&content_length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    response
}

async fn game_start(
    State(state): State<GameLifecycleState>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(error) = authenticate(&auth, &headers).await {
        return auth_error_response(error);
    }
    let fresh = match body {
        Ok(Json(Value::Object(body))) => match body.get("fresh") {
            None => false,
            Some(value) => match game_start_bool(value) {
                Some(fresh) => fresh,
                None => {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"detail": [{
                            "type": if value.is_null() { "bool_type" } else { "bool_parsing" },
                            "loc": ["body", "fresh"],
                            "msg": if value.is_null() {
                                "Input should be a valid boolean"
                            } else {
                                "Input should be a valid boolean, unable to interpret input"
                            },
                            "input": value
                        }]})),
                    )
                        .into_response();
                }
            },
        },
        Ok(Json(value)) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": [{
                    "type": "model_type",
                    "loc": ["body"],
                    "msg": "Input should be a valid dictionary or instance of GameStart",
                    "input": value
                }]})),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"detail": "Invalid request body"})),
            )
                .into_response();
        }
    };
    match state.backend.start(fresh) {
        Ok(result) => Json(result).into_response(),
        Err(message) => route_error(StatusCode::SERVICE_UNAVAILABLE, &message),
    }
}

fn game_start_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) if value.as_i64() == Some(0) => Some(false),
        Value::Number(value) if value.as_i64() == Some(1) => Some(true),
        Value::String(value) => match value.to_ascii_lowercase().as_str() {
            "0" | "f" | "false" | "n" | "no" | "off" => Some(false),
            "1" | "t" | "true" | "y" | "yes" | "on" => Some(true),
            _ => None,
        },
        _ => None,
    }
}

fn valid_codewheel_filename(filename: &str) -> bool {
    let Some(stem) = filename.strip_suffix(".gif") else {
        return false;
    };
    let Some(number) = stem
        .strip_prefix("esp")
        .or_else(|| stem.strip_prefix("det"))
    else {
        return false;
    };
    if number.len() != 2 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    matches!(number.parse::<u8>(), Ok(1..=36))
}

async fn authenticate(
    backend: &Arc<dyn AuthBackend>,
    headers: &HeaderMap,
) -> Result<Identity, AuthError> {
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers);
    backend.identity(bearer.as_deref(), cookie.as_deref()).await
}

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let mut cookie = None;
    for value in headers.get_all(header::COOKIE).iter() {
        let value = value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>();
        for part in value.split(';') {
            let (key, value) = part.split_once('=').unwrap_or(("", part));
            if key.trim() == "vp_session" {
                cookie = Some(unquote_cookie_value(value.trim()));
            }
        }
    }
    cookie.filter(|value| !value.is_empty())
}

fn unquote_cookie_value(value: &str) -> String {
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let chars = value.chars().collect::<Vec<_>>();
    let mut unquoted = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() {
            if index + 3 < chars.len()
                && matches!(chars[index + 1], '0'..='3')
                && matches!(chars[index + 2], '0'..='7')
                && matches!(chars[index + 3], '0'..='7')
            {
                let octal = (chars[index + 1] as u32 - '0' as u32) * 64
                    + (chars[index + 2] as u32 - '0' as u32) * 8
                    + (chars[index + 3] as u32 - '0' as u32);
                unquoted.push(char::from(octal as u8));
                index += 4;
            } else {
                unquoted.push(chars[index + 1]);
                index += 2;
            }
        } else {
            unquoted.push(chars[index]);
            index += 1;
        }
    }
    unquoted
}

fn auth_error_response(error: AuthError) -> Response {
    let (status, detail) = match error {
        AuthError::TooManyAttempts => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; retry in five minutes",
        ),
        AuthError::IncorrectPassword => (StatusCode::UNAUTHORIZED, "Incorrect password"),
        AuthError::SignInRequired => (StatusCode::UNAUTHORIZED, "Sign in required"),
        AuthError::Internal(_) => return internal_error(),
    };
    (status, Json(json!({"detail": detail}))).into_response()
}

fn route_error(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{body::to_bytes, http::Request};
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
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async move {
                if bearer == Some("test-token") || cookie == Some("test-cookie") {
                    Ok(Identity::Owner)
                } else {
                    Err(AuthError::SignInRequired)
                }
            })
        }

        fn logout<'a>(
            &'a self,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct FakeGame {
        start_result: Result<Value, String>,
        fresh_values: Mutex<Vec<bool>>,
    }

    impl GameLifecycleBackend for FakeGame {
        fn status(&self) -> Result<Value, String> {
            Ok(json!({
                "running": false,
                "audio": false,
                "build": "original",
                "emulator": {"kind": "pinned", "scope": "next_start"},
                "state_archive": {"available": true},
                "run_name": null
            }))
        }

        fn start(&self, fresh: bool) -> Result<Value, String> {
            self.fresh_values.lock().unwrap().push(fresh);
            self.start_result.clone()
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

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir() // tempdir-ok: route tests need a real isolated filesystem root.
            .join(format!(
                "ds-web-game-lifecycle-{label}-{}-{nonce}",
                std::process::id()
            ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn app(start_result: Result<Value, String>) -> (Router, Arc<FakeGame>, PathBuf) {
        let directory = temp_dir("routes");
        let game = Arc::new(FakeGame {
            start_result,
            fresh_values: Mutex::new(Vec::new()),
        });
        let state = GameLifecycleState::new(directory.clone(), game.clone());
        let auth: Arc<dyn AuthBackend> = Arc::new(FakeAuth);
        (router(state, auth), game, directory)
    }

    fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    fn json_body(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn accepts_only_the_python_codewheel_filename_pattern() {
        for filename in ["esp01.gif", "esp36.gif", "det09.gif", "det30.gif"] {
            assert!(valid_codewheel_filename(filename), "{filename}");
        }
        for filename in [
            "esp00.gif",
            "esp37.gif",
            "det1.gif",
            "esp01.GIF",
            "../esp01.gif",
            "other01.gif",
        ] {
            assert!(!valid_codewheel_filename(filename), "{filename}");
        }
    }

    #[test]
    fn game_status_returns_backend_json_and_requires_authentication() {
        let (service, _, directory) = app(Ok(json!({"started": true})));
        let response = block_on(service.clone().oneshot(request(
            "GET",
            "/api/game/status",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).1,
            json!({
                "running": false,
                "audio": false,
                "build": "original",
                "emulator": {"kind": "pinned", "scope": "next_start"},
                "state_archive": {"available": true},
                "run_name": null
            })
        );

        let mut unauthorized = request("GET", "/api/game/status", Body::empty());
        unauthorized.headers_mut().remove(header::AUTHORIZATION);
        let response = block_on(service.oneshot(unauthorized)).unwrap();
        assert_eq!(
            json_body(response),
            (
                StatusCode::UNAUTHORIZED,
                json!({"detail": "Sign in required"})
            )
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn game_start_defaults_fresh_and_maps_game_errors_to_503() {
        let (service, game, directory) = app(Ok(json!({"running": true, "run_name": "vp-test"})));
        let response = block_on(service.clone().oneshot(request(
            "POST",
            "/api/game/start",
            Body::from("{}"),
        )))
        .unwrap();
        assert_eq!(
            json_body(response),
            (
                StatusCode::OK,
                json!({"running": true, "run_name": "vp-test"})
            )
        );
        let response = block_on(service.clone().oneshot(request(
            "POST",
            "/api/game/start",
            Body::from(r#"{"fresh":true}"#),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = block_on(service.clone().oneshot(request(
            "POST",
            "/api/game/start",
            Body::from(r#"{"fresh":"yes"}"#),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*game.fresh_values.lock().unwrap(), vec![false, true, true]);
        let response = block_on(service.clone().oneshot(request(
            "POST",
            "/api/game/start",
            Body::from(r#"{"fresh":"maybe"}"#),
        )))
        .unwrap();
        assert_eq!(
            json_body(response),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"detail": [{
                    "type": "bool_parsing",
                    "loc": ["body", "fresh"],
                    "msg": "Input should be a valid boolean, unable to interpret input",
                    "input": "maybe"
                }]})
            )
        );
        fs::remove_dir_all(directory).unwrap();

        let (service, _, directory) = app(Err("Poolrad checkout is unavailable".into()));
        let response =
            block_on(service.oneshot(request("POST", "/api/game/start", Body::from("{}"))))
                .unwrap();
        assert_eq!(
            json_body(response),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"detail": "Poolrad checkout is unavailable"})
            )
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn codewheel_returns_gif_headers_and_rejects_invalid_or_symlink_files() {
        let (service, _, directory) = app(Ok(json!({})));
        fs::write(directory.join("esp01.gif"), b"GIF89a-test").unwrap();
        let response = block_on(service.clone().oneshot(request(
            "GET",
            "/api/game/codewheel/esp01.gif",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/gif");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX))
                .unwrap()
                .as_ref(),
            b"GIF89a-test"
        );

        let response = block_on(service.clone().oneshot(request(
            "GET",
            "/api/game/codewheel/esp37.gif",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(
            json_body(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail": "Unknown rune template"})
            )
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("esp01.gif", directory.join("det01.gif")).unwrap();
            let response = block_on(service.oneshot(request(
                "GET",
                "/api/game/codewheel/det01.gif",
                Body::empty(),
            )))
            .unwrap();
            assert_eq!(
                json_body(response),
                (
                    StatusCode::NOT_FOUND,
                    json!({"detail": "Private rune template unavailable"})
                )
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
