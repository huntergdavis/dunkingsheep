use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    response::Response,
    Router,
};
use ds_web::routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    game_media::{
        router, GameMediaBackend, GameMediaError, GameMediaFuture, GameMediaJson, GameStreamKind,
    },
};
use serde_json::{json, Value};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("fixtures/game_media.json");

struct AllowAuth;

impl AuthBackend for AllowAuth {
    fn login<'a>(
        &'a self,
        _address: &'a str,
        _password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unexpected login".into())) })
    }

    fn identity<'a>(
        &'a self,
        _bearer: Option<&'a str>,
        _cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Ok(Identity::Owner) })
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

struct DenyAuth;

impl AuthBackend for DenyAuth {
    fn login<'a>(
        &'a self,
        _address: &'a str,
        _password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unexpected login".into())) })
    }

    fn identity<'a>(
        &'a self,
        _bearer: Option<&'a str>,
        _cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::SignInRequired) })
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

struct FakeGame {
    templates: Result<Vec<Value>, GameMediaError>,
    recognition: Result<GameMediaJson, GameMediaError>,
    status: Result<Value, GameMediaError>,
    stream_body: String,
    calls: Mutex<Vec<String>>,
    recognized_templates: Mutex<Vec<Value>>,
}

impl GameMediaBackend for FakeGame {
    fn templates_cached(&self) -> GameMediaFuture<'_, Result<Vec<Value>, GameMediaError>> {
        self.calls.lock().unwrap().push("templates".into());
        let result = self.templates.clone();
        Box::pin(async move { result })
    }

    fn recognize_codeword<'a>(
        &'a self,
        templates: Vec<Value>,
    ) -> GameMediaFuture<'a, Result<GameMediaJson, GameMediaError>> {
        self.calls.lock().unwrap().push("recognize".into());
        *self.recognized_templates.lock().unwrap() = templates;
        let result = self.recognition.clone();
        Box::pin(async move { result })
    }

    fn status(&self) -> GameMediaFuture<'_, Result<Value, GameMediaError>> {
        self.calls.lock().unwrap().push("status".into());
        let result = self.status.clone();
        Box::pin(async move { result })
    }

    fn stream<'a>(
        &'a self,
        kind: GameStreamKind,
    ) -> GameMediaFuture<'a, Result<Body, GameMediaError>> {
        self.calls.lock().unwrap().push(match kind {
            GameStreamKind::Video => "video-stream".into(),
            GameStreamKind::Audio => "audio-stream".into(),
        });
        let body = self.stream_body.clone();
        Box::pin(async move { Ok(Body::from(body)) })
    }
}

fn game_for_case(case: &Value) -> Arc<FakeGame> {
    let backend = &case["backend"];
    let templates = match backend.get("template_error") {
        Some(error) => Err(GameMediaError::Template(error.as_str().unwrap().into())),
        None => Ok(backend
            .get("templates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()),
    };
    let recognition = match backend.get("game_error") {
        Some(error) => Err(GameMediaError::Game(error.as_str().unwrap().into())),
        None => Ok(GameMediaJson::new(
            backend
                .get("result_fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|pair| (pair[0].as_str().unwrap().to_owned(), pair[1].clone()))
                .collect(),
        )),
    };
    Arc::new(FakeGame {
        templates,
        recognition,
        status: Ok(backend.get("status").cloned().unwrap_or_else(|| json!({}))),
        stream_body: backend
            .get("stream_body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        calls: Mutex::new(Vec::new()),
        recognized_templates: Mutex::new(Vec::new()),
    })
}

fn fixture_router(game: Arc<FakeGame>) -> Router {
    let backend: Arc<dyn GameMediaBackend> = game;
    let auth: Arc<dyn AuthBackend> = Arc::new(AllowAuth);
    router(backend, auth, RequestScheme::Http)
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

fn fixture_cases() -> Value {
    serde_json::from_str(FIXTURE).expect("valid Python/Rust game media fixture")
}

fn case_named<'a>(fixture: &'a Value, name: &str) -> &'a Value {
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .expect("fixture case exists")
}

fn fixture_request(case: &Value) -> Request<Body> {
    let request = &case["request"];
    let mut builder = Request::builder()
        .method(request["method"].as_str().unwrap())
        .uri(request["path"].as_str().unwrap());
    for (name, value) in request["headers"].as_object().unwrap() {
        builder = builder.header(name, value.as_str().unwrap());
    }
    builder.body(Body::empty()).expect("valid fixture request")
}

fn assert_fixture_case(name: &str) -> Arc<FakeGame> {
    let fixture = fixture_cases();
    let case = case_named(&fixture, name);
    let game = game_for_case(case);
    let response = block_on(fixture_router(game.clone()).oneshot(fixture_request(case)))
        .expect("in-process Axum router response");
    assert_fixture_response(response, &case["response"]);
    game
}

fn assert_fixture_response(response: Response, expected: &Value) {
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
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|value| value.to_str().unwrap());
    assert_eq!(set_cookie, expected["set_cookie"].as_str());
    let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
    let body = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(body, expected["body"].as_str().unwrap());
    if !expected["json"].is_null() {
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            expected["json"]
        );
    }
}

#[test]
fn game_autocodeword_matches_source_fixture() {
    let game = assert_fixture_case("autocodeword_success");
    assert_eq!(
        game.calls.lock().unwrap().as_slice(),
        ["templates", "recognize"]
    );
    assert_eq!(
        game.recognized_templates.lock().unwrap().as_slice(),
        [json!({"glyph": "K"})]
    );
}

#[test]
fn game_autocodeword_template_failure_uses_empty_templates_like_python() {
    let game = assert_fixture_case("autocodeword_template_failure_uses_empty_templates");
    assert_eq!(
        game.calls.lock().unwrap().as_slice(),
        ["templates", "recognize"]
    );
    assert!(game.recognized_templates.lock().unwrap().is_empty());
}

#[test]
fn game_autocodeword_preserves_game_error_response() {
    let game = assert_fixture_case("autocodeword_game_error");
    assert_eq!(
        game.calls.lock().unwrap().as_slice(),
        ["templates", "recognize"]
    );
}

#[test]
fn game_video_matches_stream_and_not_running_fixtures() {
    let stream = assert_fixture_case("video_stream");
    assert_eq!(
        stream.calls.lock().unwrap().as_slice(),
        ["status", "video-stream"]
    );
    let stopped = assert_fixture_case("video_not_running");
    assert_eq!(stopped.calls.lock().unwrap().as_slice(), ["status"]);
}

#[test]
fn game_audio_matches_stream_and_unavailable_fixtures() {
    let stream = assert_fixture_case("audio_stream");
    assert_eq!(
        stream.calls.lock().unwrap().as_slice(),
        ["status", "audio-stream"]
    );
    let unavailable = assert_fixture_case("audio_unavailable");
    assert_eq!(unavailable.calls.lock().unwrap().as_slice(), ["status"]);
}

#[test]
fn every_game_media_route_requires_auth_before_backend_calls() {
    let fixture = fixture_cases();
    for case in fixture["cases"].as_array().unwrap() {
        let game = game_for_case(case);
        let backend: Arc<dyn GameMediaBackend> = game.clone();
        let auth: Arc<dyn AuthBackend> = Arc::new(DenyAuth);
        let response =
            block_on(router(backend, auth, RequestScheme::Http).oneshot(fixture_request(case)))
                .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", case["name"]);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(game.calls.lock().unwrap().is_empty(), "{}", case["name"]);
    }
}

#[test]
fn cross_origin_refusal_precedes_auth_and_game_backend_calls() {
    let fixture = fixture_cases();
    let case = case_named(&fixture, "video_stream");
    let game = game_for_case(case);
    let backend: Arc<dyn GameMediaBackend> = game.clone();
    let auth: Arc<dyn AuthBackend> = Arc::new(DenyAuth);
    let mut request = fixture_request(case);
    request
        .headers_mut()
        .insert(header::ORIGIN, "https://other.test".parse().unwrap());

    let response = block_on(router(backend, auth, RequestScheme::Http).oneshot(request)).unwrap();
    let (status, body) = response_json(response);
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
    assert!(game.calls.lock().unwrap().is_empty());
}

fn response_json(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
