use std::{
    future::Future,
    net::SocketAddr,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Waker},
};

use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Extension},
    http::{header, Request},
};
use ds_web::{
    router_with_scheme,
    routes::auth::{AuthBackend, AuthError, AuthFuture, Identity},
    RequestScheme,
};
use serde_json::{json, Value};
use tower::ServiceExt;

struct UnusedAuthBackend;

impl AuthBackend for UnusedAuthBackend {
    fn login<'a>(
        &'a self,
        _address: &'a str,
        _password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("backend must not be called".into())) })
    }

    fn identity<'a>(
        &'a self,
        _bearer: Option<&'a str>,
        _cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("backend must not be called".into())) })
    }

    fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("backend must not be called".into())) })
    }

    fn change_password<'a>(
        &'a self,
        _current: &'a str,
        _new: &'a str,
        _keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("backend must not be called".into())) })
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

#[test]
fn auth_login_origin_refusal_matches_python_fixture() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/login_origin_refused.json"))
        .expect("valid shared Python/Rust fixture");
    let request_fixture = &fixture["request"];
    let body = serde_json::to_vec(&request_fixture["json"]).expect("request JSON");
    let backend: Arc<dyn AuthBackend> = Arc::new(UnusedAuthBackend);
    let app = router_with_scheme(RequestScheme::Http).layer(Extension(backend));

    let mut request = Request::builder()
        .method(request_fixture["method"].as_str().unwrap())
        .uri(request_fixture["path"].as_str().unwrap())
        .header(
            header::HOST,
            request_fixture["headers"]["host"].as_str().unwrap(),
        )
        .header(
            header::ORIGIN,
            request_fixture["headers"]["origin"].as_str().unwrap(),
        )
        .header(
            header::CONTENT_TYPE,
            request_fixture["headers"]["content-type"].as_str().unwrap(),
        )
        .body(Body::from(body))
        .expect("valid fixture request");
    request
        .extensions_mut()
        .insert(ConnectInfo("127.0.0.1:4000".parse::<SocketAddr>().unwrap()));

    let response = block_on(app.oneshot(request)).expect("in-process router response");
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
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|value| value.to_str().unwrap());
    assert_eq!(set_cookie, expected["set_cookie"].as_str());

    let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).expect("response body");
    let raw = String::from_utf8(bytes.to_vec()).expect("UTF-8 JSON body");
    assert_eq!(raw, expected["body"].as_str().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap(),
        expected["json"]
    );
    assert_eq!(
        json!({"detail": "Cross-origin request refused"}),
        expected["json"]
    );
}
