//! Game save-state HTTP handlers ported from `web/app.py`.
//!
//! The runtime owns DOSBox state and file operations behind `GameStateBackend`;
//! this module owns request validation, auth, and Python-compatible responses.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

pub type GameStateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, GameStateError>> + Send + 'a>>;

/// Runtime operations used by the three game-state endpoints.
///
/// A concrete adapter can call the game session/client; route tests inject a
/// fake so they never contact a running emulator or write save-state files.
pub trait GameStateBackend: Send + Sync + 'static {
    fn list_states(&self) -> GameStateFuture<'_>;
    fn savestate(&self, tag: String) -> GameStateFuture<'_>;
    fn loadstate(&self, slot: i64) -> GameStateFuture<'_>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GameStateError {
    Game(String),
    Failure(String),
}

/// Mount the three game-state routes using the default HTTP scheme.
pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

/// Mount game-state handlers; the containing application supplies auth and
/// game backends as extensions and chooses the externally visible scheme.
pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/game/states", get(game_states))
        .route("/api/game/savestate", post(game_savestate))
        .route("/api/game/loadstate", post(game_loadstate))
        .layer(Extension(scheme))
}

async fn game_states(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    match game.list_states().await {
        Ok(states) => Json(states).into_response(),
        Err(_) => internal_error(),
    }
}

async fn game_savestate(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let body = match request_json(request).await {
        Ok(Some(Value::Object(body))) => body,
        Ok(Some(input)) => return model_error(input),
        Ok(None) => return missing_body_error(),
        Err(response) => return response,
    };
    let tag = match body.get("tag") {
        None => String::new(),
        Some(Value::String(tag)) if tag.chars().count() <= 80 => tag.clone(),
        Some(Value::String(tag)) => {
            return validation_error(
                "string_too_long",
                vec![json!("body"), json!("tag")],
                "String should have at most 80 characters",
                json!(tag),
                Some(json!({"max_length": 80})),
            );
        }
        Some(input) => {
            return validation_error(
                "string_type",
                vec![json!("body"), json!("tag")],
                "Input should be a valid string",
                input.clone(),
                None,
            );
        }
    };

    match game.savestate(tag).await {
        Ok(state) => Json(state).into_response(),
        Err(GameStateError::Game(detail)) => detail_response(StatusCode::BAD_REQUEST, detail),
        Err(GameStateError::Failure(_)) => internal_error(),
    }
}

async fn game_loadstate(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let body = match request_json(request).await {
        Ok(Some(Value::Object(body))) => body,
        Ok(Some(input)) => return model_error(input),
        Ok(None) => return missing_body_error(),
        Err(response) => return response,
    };
    let Some(input) = body.get("slot") else {
        return validation_error(
            "missing",
            vec![json!("body"), json!("slot")],
            "Field required",
            Value::Object(body),
            None,
        );
    };
    let Some(slot) = python_integer(input) else {
        let (kind, message) = match input {
            Value::String(_) => (
                "int_parsing",
                "Input should be a valid integer, unable to parse string as an integer",
            ),
            Value::Number(number) if number.as_f64().is_some_and(|value| value.fract() != 0.0) => (
                "int_from_float",
                "Input should be a valid integer, got a number with a fractional part",
            ),
            _ => ("int_type", "Input should be a valid integer"),
        };
        return validation_error(
            kind,
            vec![json!("body"), json!("slot")],
            message,
            input.clone(),
            None,
        );
    };
    if slot < 1 {
        return validation_error(
            "greater_than_equal",
            vec![json!("body"), json!("slot")],
            "Input should be greater than or equal to 1",
            input.clone(),
            Some(json!({"ge": 1})),
        );
    }
    if slot > 99 {
        return validation_error(
            "less_than_equal",
            vec![json!("body"), json!("slot")],
            "Input should be less than or equal to 99",
            input.clone(),
            Some(json!({"le": 99})),
        );
    }

    match game.loadstate(slot).await {
        Ok(state) => Json(state).into_response(),
        Err(GameStateError::Game(detail)) => detail_response(StatusCode::BAD_REQUEST, detail),
        Err(GameStateError::Failure(_)) => internal_error(),
    }
}

async fn request_json(request: Request) -> Result<Option<Value>, Response> {
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
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        let offset = error.column().saturating_sub(1);
        validation_error(
            "json_invalid",
            vec![json!("body"), json!(offset)],
            "JSON decode error",
            json!({}),
            Some(json!({"error": python_json_error(&error.to_string())})),
        )
    })
}

fn python_json_error(error: &str) -> &'static str {
    if error.starts_with("key must be a string") {
        "Expecting property name enclosed in double quotes"
    } else if error.starts_with("EOF while parsing") || error.starts_with("expected value") {
        "Expecting value"
    } else if error.starts_with("expected `:`") {
        "Expecting ':' delimiter"
    } else if error.starts_with("expected `,`") {
        "Expecting ',' delimiter"
    } else {
        "Invalid JSON"
    }
}

fn python_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|value| value.fract() == 0.0)
                    .and_then(|value| i64::try_from(value as i128).ok())
            }),
        Value::String(value) => value.trim().parse().ok(),
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn model_error(input: Value) -> Response {
    validation_error(
        "model_attributes_type",
        vec![json!("body")],
        "Input should be a valid dictionary or object to extract fields from",
        input,
        None,
    )
}

fn missing_body_error() -> Response {
    validation_error(
        "missing",
        vec![json!("body")],
        "Field required",
        Value::Null,
        None,
    )
}

fn validation_error(
    kind: &str,
    location: Vec<Value>,
    message: &str,
    input: Value,
    context: Option<Value>,
) -> Response {
    let mut issue = serde_json::Map::new();
    issue.insert("type".into(), json!(kind));
    issue.insert("loc".into(), Value::Array(location));
    issue.insert("msg".into(), json!(message));
    issue.insert("input".into(), input);
    if let Some(context) = context {
        issue.insert("ctx".into(), context);
    }
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [Value::Object(issue)]})),
    )
        .into_response()
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
        AuthError::Internal(_) => internal_error(),
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request as HttpRequest,
    };
    use std::{
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };
    use tower::ServiceExt;

    struct FakeGame {
        calls: Mutex<Vec<String>>,
        result: Mutex<Result<Value, GameStateError>>,
    }

    impl FakeGame {
        fn new(result: Value) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Ok(result)),
            }
        }

        fn fail(&self, error: GameStateError) {
            *self.result.lock().unwrap() = Err(error);
        }
    }

    impl GameStateBackend for FakeGame {
        fn list_states(&self) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push("list_states".into());
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn savestate(&self, tag: String) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push(format!("savestate:{tag}"));
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn loadstate(&self, slot: i64) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push(format!("loadstate:{slot}"));
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }
    }

    struct FakeAuth {
        result: Result<Identity, AuthError>,
    }

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
            let result = self.result.clone();
            Box::pin(async move { result })
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

    fn app(game: Arc<dyn GameStateBackend>, auth: Arc<dyn AuthBackend>) -> Router {
        router().layer(Extension(auth)).layer(Extension(game))
    }

    fn request(method: &str, path: &str, body: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "desk.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
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

    fn authorized() -> Arc<dyn AuthBackend> {
        Arc::new(FakeAuth {
            result: Ok(Identity::Api),
        })
    }

    fn response(app: Router, request: HttpRequest<Body>) -> (StatusCode, String, Value) {
        let response = block_on(app.oneshot(request)).unwrap();
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, body, json)
    }

    #[test]
    fn game_states_returns_backend_json() {
        let game = Arc::new(FakeGame::new(json!({"run_name": null, "states": []})));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("GET", "/api/game/states", ""),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"run_name":null,"states":[]}"#, body);
        assert_eq!(json!({"run_name": null, "states": []}), value);
        assert_eq!(vec!["list_states"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_savestate_defaults_tag_and_returns_backend_json() {
        let game = Arc::new(FakeGame::new(json!({"slot": 1, "tag": "slot 1"})));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", "{}"),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"slot":1,"tag":"slot 1"}"#, body);
        assert_eq!(json!({"slot": 1, "tag": "slot 1"}), value);
        assert_eq!(vec!["savestate:"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_savestate_maps_game_error_to_python_400_detail() {
        let game = Arc::new(FakeGame::new(Value::Null));
        game.fail(GameStateError::Game(
            "All save-state slots are in use".into(),
        ));
        let (status, body, value) = response(
            app(game, authorized()),
            request("POST", "/api/game/savestate", r#"{"tag":"quick"}"#),
        );
        assert_eq!(StatusCode::BAD_REQUEST, status);
        assert_eq!(r#"{"detail":"All save-state slots are in use"}"#, body);
        assert_eq!(json!({"detail": "All save-state slots are in use"}), value);
    }

    #[test]
    fn game_loadstate_passes_validated_slot_and_returns_backend_json() {
        let game = Arc::new(FakeGame::new(
            json!({"slot": 8, "loaded": true, "verified": true}),
        ));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":8}"#),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"loaded":true,"slot":8,"verified":true}"#, body);
        assert_eq!(json!({"slot": 8, "loaded": true, "verified": true}), value);
        assert_eq!(vec!["loadstate:8"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_loadstate_maps_game_error_to_python_400_detail() {
        let game = Arc::new(FakeGame::new(Value::Null));
        game.fail(GameStateError::Game(
            "That save state is not available".into(),
        ));
        let (status, body, value) = response(
            app(game, authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":4}"#),
        );
        assert_eq!(StatusCode::BAD_REQUEST, status);
        assert_eq!(r#"{"detail":"That save state is not available"}"#, body);
        assert_eq!(json!({"detail": "That save state is not available"}), value);
    }

    #[test]
    fn game_loadstate_rejects_out_of_range_slot_without_calling_backend() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":0}"#),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"greater_than_equal","loc":["body","slot"],
                "msg":"Input should be greater than or equal to 1","input":0,"ctx":{"ge":1}}]}),
            value
        );
        assert!(body.starts_with(r#"{"detail":[{"ctx":{"ge":1},"input":0,"loc":["body","slot"],"msg":"Input should be greater than or equal to 1","type":"greater_than_equal"}]}"#));
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_loadstate_requires_slot_with_python_validation_shape() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", "{}"),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"missing","loc":["body","slot"],
                "msg":"Field required","input":{}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_rejects_long_tag_with_python_validation_shape() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let long_tag = "x".repeat(81);
        let body = serde_json::to_string(&json!({"tag": long_tag})).unwrap();
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", &body),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!("string_too_long", value["detail"][0]["type"]);
        assert_eq!(80, value["detail"][0]["ctx"]["max_length"]);
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_states_requires_authentication_and_preserves_json_error_body() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let auth = Arc::new(FakeAuth {
            result: Err(AuthError::SignInRequired),
        });
        let (status, body, value) = response(
            app(game.clone(), auth),
            request("GET", "/api/game/states", ""),
        );
        assert_eq!(StatusCode::UNAUTHORIZED, status);
        assert_eq!(r#"{"detail":"Sign in required"}"#, body);
        assert_eq!(json!({"detail": "Sign in required"}), value);
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_states_rejects_cross_origin_before_auth_or_backend() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let mut request = request("GET", "/api/game/states", "");
        request
            .headers_mut()
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let (status, body, value) = response(app(game.clone(), authorized()), request);
        assert_eq!(StatusCode::FORBIDDEN, status);
        assert_eq!(r#"{"detail":"Cross-origin request refused"}"#, body);
        assert_eq!(json!({"detail": "Cross-origin request refused"}), value);
        assert!(game.calls.lock().unwrap().is_empty());
    }
}
