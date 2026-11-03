//! Game input routes ported from `web/app.py`.
//!
//! The outer runtime supplies the authenticated identity backend and the game
//! input adapter. Keeping both explicit leaves this module isolated from the
//! live emulator and makes route behavior testable without starting a game.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::Extension,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Map, Value};

use super::auth::{AuthBackend, AuthError, RequestScheme};

pub type GameInputFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Operations needed by the game input routes. A production adapter can call
/// the shared game controller; tests use a fake and never touch a display.
pub trait GameInputBackend: Send + Sync + 'static {
    fn stop<'a>(&'a self) -> GameInputFuture<'a, Result<Value, GameInputError>>;
    fn key<'a>(&'a self, key: &'a str) -> GameInputFuture<'a, Result<(), GameInputError>>;
    fn type_text<'a>(
        &'a self,
        text: &'a str,
        submit: bool,
    ) -> GameInputFuture<'a, Result<(), GameInputError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GameInputError(pub String);

/// Mount the three authenticated game input endpoints without wiring them
/// into the default application router.
pub fn router(
    backend: Arc<dyn GameInputBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/game/stop", post(game_stop))
        .route("/api/game/key", post(game_key))
        .route("/api/game/type", post(game_type))
        .layer(Extension(backend))
        .layer(Extension(auth))
        .layer(Extension(scheme))
}

async fn game_stop(
    Extension(backend): Extension<Arc<dyn GameInputBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: axum::extract::Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    match backend.stop().await {
        Ok(status) => Json(status).into_response(),
        // The Python handler does not catch failures from GAME.stop(); keep
        // the framework's generic 500 response rather than exposing details.
        Err(_) => internal_error(),
    }
}

async fn game_key(
    Extension(backend): Extension<Arc<dyn GameInputBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: axum::extract::Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let object = match model_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let key = match string_field(object, "key", &body, 1, 16) {
        Ok(key) => key,
        Err(response) => return response,
    };
    match backend.key(key).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => detail_response(StatusCode::BAD_REQUEST, error.0),
    }
}

async fn game_type(
    Extension(backend): Extension<Arc<dyn GameInputBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: axum::extract::Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let object = match model_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let text = match string_field(object, "text", &body, 1, 64) {
        Ok(text) => text,
        Err(response) => return response,
    };
    let submit = match bool_field(object, "submit") {
        Ok(submit) => submit,
        Err(response) => return response,
    };
    match backend.type_text(text, submit).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => detail_response(StatusCode::BAD_REQUEST, error.0),
    }
}

async fn request_json(request: axum::extract::Request) -> Result<Value, Response> {
    let bytes = to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| {
            detail_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "There was an error parsing the body".into(),
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        let input = String::from_utf8_lossy(&bytes);
        let message = python_json_error(&error);
        let position = json_error_position(&input, &error);
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "json_invalid",
                "loc": ["body", position],
                "msg": "JSON decode error",
                "input": {},
                "ctx": {"error": message}
            }]})),
        )
            .into_response()
    })
}

fn json_error_position(input: &str, error: &serde_json::Error) -> usize {
    let line_start = input
        .split_inclusive('\n')
        .take(error.line().saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    let byte_position = line_start
        .saturating_add(error.column().saturating_sub(1))
        .min(input.len());
    input[..byte_position].chars().count()
}

fn python_json_error(error: &serde_json::Error) -> String {
    let message = error.to_string();
    let message = message
        .split_once(" at line ")
        .map_or(message.as_str(), |(message, _)| message);
    match message {
        "expected value" => "Expecting value".to_owned(),
        "key must be a string" => "Expecting property name enclosed in double quotes".to_owned(),
        "expected `:`" | "expected ':'" => "Expecting ':' delimiter".to_owned(),
        "expected `,` or `}`"
        | "expected ',' or '}'"
        | "expected `,` or `]`"
        | "expected ',' or ']'" => "Expecting ',' delimiter".to_owned(),
        _ => message.to_owned(),
    }
}

fn model_object(body: &Value) -> Result<&Map<String, Value>, Response> {
    body.as_object().ok_or_else(|| {
        validation_error(json!({
            "type": "model_attributes_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "input": body
        }))
    })
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    body: &Value,
    min_length: usize,
    max_length: usize,
) -> Result<&'a str, Response> {
    let Some(value) = object.get(name) else {
        return Err(validation_error(json!({
            "type": "missing",
            "loc": ["body", name],
            "msg": "Field required",
            "input": body
        })));
    };
    let Some(value_str) = value.as_str() else {
        return Err(validation_error(json!({
            "type": "string_type",
            "loc": ["body", name],
            "msg": "Input should be a valid string",
            "input": value
        })));
    };
    let length = value_str.chars().count();
    if length < min_length {
        return Err(validation_error(json!({
            "type": "string_too_short",
            "loc": ["body", name],
            "msg": format!("String should have at least {min_length} character"),
            "input": value,
            "ctx": {"min_length": min_length}
        })));
    }
    if length > max_length {
        return Err(validation_error(json!({
            "type": "string_too_long",
            "loc": ["body", name],
            "msg": format!("String should have at most {max_length} characters"),
            "input": value,
            "ctx": {"max_length": max_length}
        })));
    }
    Ok(value_str)
}

fn bool_field(object: &Map<String, Value>, name: &str) -> Result<bool, Response> {
    let Some(value) = object.get(name) else {
        return Ok(true);
    };
    if let Some(value) = value.as_bool() {
        return Ok(value);
    }
    if value.is_f64() {
        if let Some(value) = value.as_f64() {
            if value == 0.0 || value == 1.0 {
                return Ok(value == 1.0);
            }
        }
    }
    if let Some(value) = value.as_i64() {
        if value == 0 || value == 1 {
            return Ok(value == 1);
        }
    }
    if let Some(value) = value.as_str() {
        return match value.to_ascii_lowercase().as_str() {
            "0" | "f" | "false" | "n" | "no" | "off" => Ok(false),
            "1" | "t" | "true" | "y" | "yes" | "on" => Ok(true),
            _ => Err(validation_error(json!({
                "type": "bool_parsing",
                "loc": ["body", name],
                "msg": "Input should be a valid boolean, unable to interpret input",
                "input": value
            }))),
        };
    }
    Err(validation_error(json!({
        "type": "bool_type",
        "loc": ["body", name],
        "msg": "Input should be a valid boolean",
        "input": value
    })))
}

fn validation_error(error: Value) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [error]})),
    )
        .into_response()
}

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<(), Response> {
    if let Some(response) = same_origin(headers, scheme) {
        return Err(response);
    }
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers);
    auth.identity(bearer.as_deref(), cookie.as_deref())
        .await
        .map(|_| ())
        .map_err(auth_error_response)
}

fn same_origin(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?;
    let origin = latin1(origin.as_bytes());
    if origin.is_empty() {
        return None;
    }
    let host = headers
        .get(header::HOST)
        .map(|value| latin1(value.as_bytes()))
        .unwrap_or_default();
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    if origin != format!("{scheme}://{host}") {
        Some(detail_response(
            StatusCode::FORBIDDEN,
            "Cross-origin request refused".to_owned(),
        ))
    } else {
        None
    }
}

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let mut cookie = None;
    for value in headers.get_all(header::COOKIE).iter() {
        for part in latin1(value.as_bytes()).split(';') {
            let (key, value) = part.split_once('=').unwrap_or(("", part));
            if key.trim() == "vp_session" {
                cookie = Some(unquote_cookie(value.trim()));
            }
        }
    }
    cookie.filter(|value| !value.is_empty())
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn unquote_cookie(value: &str) -> String {
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
    match error {
        AuthError::TooManyAttempts => detail_response(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; retry in five minutes".to_owned(),
        ),
        AuthError::IncorrectPassword => {
            detail_response(StatusCode::UNAUTHORIZED, "Incorrect password".to_owned())
        }
        AuthError::SignInRequired => {
            detail_response(StatusCode::UNAUTHORIZED, "Sign in required".to_owned())
        }
        AuthError::Internal(_) => internal_error(),
    }
}

fn detail_response(status: StatusCode, detail: String) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
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
        http::{header, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::super::auth::Identity;
    use super::{
        router, AuthBackend, AuthError, GameInputBackend, GameInputError, GameInputFuture,
        RequestScheme,
    };

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
    struct FakeAuth {
        seen: Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }

        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            self.seen
                .lock()
                .unwrap()
                .push((bearer.map(str::to_owned), cookie.map(str::to_owned)));
            let result = if bearer == Some("test-token") || cookie == Some("session-token") {
                Ok(Identity::Api)
            } else {
                Err(AuthError::SignInRequired)
            };
            Box::pin(async move { result })
        }

        fn logout<'a>(
            &'a self,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
    }

    #[derive(Default)]
    struct FakeGameInput {
        calls: Mutex<Vec<String>>,
        stop_error: Option<String>,
        key_error: Option<String>,
        type_error: Option<String>,
    }

    impl GameInputBackend for FakeGameInput {
        fn stop(&self) -> GameInputFuture<'_, Result<Value, GameInputError>> {
            self.calls.lock().unwrap().push("stop".to_owned());
            let result = self.stop_error.clone().map_or_else(
                || Ok(json!({"running": false, "audio": false, "build": "test"})),
                |message| Err(GameInputError(message)),
            );
            Box::pin(async move { result })
        }

        fn key<'a>(&'a self, key: &'a str) -> GameInputFuture<'a, Result<(), GameInputError>> {
            self.calls.lock().unwrap().push(format!("key:{key}"));
            let result = self
                .key_error
                .clone()
                .map_or(Ok(()), |message| Err(GameInputError(message)));
            Box::pin(async move { result })
        }

        fn type_text<'a>(
            &'a self,
            text: &'a str,
            submit: bool,
        ) -> GameInputFuture<'a, Result<(), GameInputError>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("type:{text}:{submit}"));
            let result = self
                .type_error
                .clone()
                .map_or(Ok(()), |message| Err(GameInputError(message)));
            Box::pin(async move { result })
        }
    }

    fn app(backend: Arc<FakeGameInput>, auth: Arc<FakeAuth>) -> axum::Router {
        router(backend, auth, RequestScheme::Http)
    }

    fn request(path: &str, body: &str, authenticated: bool) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, "desk.test")
            .header(header::ORIGIN, "http://desk.test")
            .header(header::CONTENT_TYPE, "application/json");
        if authenticated {
            request = request.header(header::AUTHORIZATION, "Bearer test-token");
        }
        request.body(Body::from(body.to_owned())).expect("request")
    }

    fn block_response(response: axum::response::Response) -> (StatusCode, String) {
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).expect("response body");
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("UTF-8 body"),
        )
    }

    #[test]
    fn game_stop_returns_game_status_json() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response =
            block_on(app.oneshot(request("/api/game/stop", "", true))).expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "running": false,
                "audio": false,
                "build": "test"
            })
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["stop"]);
    }

    #[test]
    fn game_stop_backend_failure_is_generic_internal_error() {
        let backend = Arc::new(FakeGameInput {
            stop_error: Some("private backend detail".into()),
            ..FakeGameInput::default()
        });
        let app = app(backend, Arc::new(FakeAuth::default()));
        let response =
            block_on(app.oneshot(request("/api/game/stop", "", true))).expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    #[test]
    fn game_key_forwards_key_and_returns_ok_json() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request("/api/game/key", r#"{"key":"Return"}"#, true)))
            .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"ok": true})
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["key:Return"]);
    }

    #[test]
    fn game_key_maps_game_error_to_bad_request() {
        let backend = Arc::new(FakeGameInput {
            key_error: Some("Unsupported game key".into()),
            ..FakeGameInput::default()
        });
        let app = app(backend, Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request("/api/game/key", r#"{"key":"F13"}"#, true)))
            .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "detail": "Unsupported game key"
            })
        );
    }

    #[test]
    fn game_key_preserves_pydantic_string_validation() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request("/api/game/key", r#"{"key":""}"#, true)))
            .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "detail": [{
                    "type": "string_too_short",
                    "loc": ["body", "key"],
                    "msg": "String should have at least 1 character",
                    "input": "",
                    "ctx": {"min_length": 1}
                }]
            })
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_type_forwards_text_and_default_submit() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request("/api/game/type", r#"{"text":"look"}"#, true)))
            .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"ok": true})
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["type:look:true"]);
    }

    #[test]
    fn game_type_forwards_explicit_false_submit_and_maps_game_error() {
        let backend = Arc::new(FakeGameInput {
            type_error: Some("Poolrad is not running".into()),
            ..FakeGameInput::default()
        });
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request(
            "/api/game/type",
            r#"{"text":"look","submit":false}"#,
            true,
        )))
        .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "detail": "Poolrad is not running"
            })
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["type:look:false"]);
    }

    #[test]
    fn game_type_accepts_integral_float_submit_values() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request(
            "/api/game/type",
            r#"{"text":"look","submit":1.0}"#,
            true,
        )))
        .expect("route response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"ok": true})
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["type:look:true"]);
    }

    #[test]
    fn game_type_matches_pydantic_boolean_string_whitespace_and_case() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.clone().oneshot(request(
            "/api/game/type",
            r#"{"text":"look","submit":"FALSE"}"#,
            true,
        )))
        .expect("case-insensitive boolean response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*backend.calls.lock().unwrap(), vec!["type:look:false"]);

        let response = block_on(app.oneshot(request(
            "/api/game/type",
            r#"{"text":"look","submit":" false "}"#,
            true,
        )))
        .expect("whitespace-padded boolean response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"detail": [{
                "type": "bool_parsing",
                "loc": ["body", "submit"],
                "msg": "Input should be a valid boolean, unable to interpret input",
                "input": " false "
            }]})
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["type:look:false"]);
    }

    #[test]
    fn game_type_malformed_json_matches_fastapi_decoder_error() {
        let backend = Arc::new(FakeGameInput::default());
        let app = app(backend.clone(), Arc::new(FakeAuth::default()));
        let response = block_on(app.oneshot(request("/api/game/type", r#"{"text":}"#, true)))
            .expect("malformed JSON response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"detail": [{
                "type": "json_invalid",
                "loc": ["body", 8],
                "msg": "JSON decode error",
                "input": {},
                "ctx": {"error": "Expecting value"}
            }]})
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_input_requires_auth_and_checks_origin_before_backend() {
        let backend = Arc::new(FakeGameInput::default());
        let auth = Arc::new(FakeAuth::default());
        let app = app(backend.clone(), auth.clone());
        let response = block_on(app.clone().oneshot(request(
            "/api/game/key",
            r#"{"key":"Return"}"#,
            false,
        )))
        .expect("unauthenticated response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut cross_origin = request("/api/game/key", r#"{"key":"Return"}"#, true);
        cross_origin.headers_mut().insert(
            header::ORIGIN,
            "http://evil.test".parse().expect("origin header"),
        );
        let response = block_on(app.oneshot(cross_origin)).expect("cross-origin response");
        let (status, body) = block_response(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "detail": "Cross-origin request refused"
            })
        );
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(auth.seen.lock().unwrap().len(), 1);
    }
}
