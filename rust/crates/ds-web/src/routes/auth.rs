//! Authentication routes ported from `web/app.py` (`login`, `logout`, and
//! `change_password`). Persistent credentials and sessions remain behind the
//! injected backend so handlers can be exercised without a live desk database.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    extract::{ConnectInfo, Extension, Json},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde::Deserialize;
use serde_json::json;

const SESSION_COOKIE: &str = "vp_session";
const SESSION_MAX_AGE: u64 = 7 * 24 * 60 * 60;

pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trusted external scheme for same-origin checks and login return URLs.
/// Configure this from the web application's transport or trusted proxy setup,
/// never directly from a client-supplied forwarding header.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RequestScheme {
    #[default]
    Http,
    Https,
}

impl RequestScheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// The route boundary for credential and session operations. The production
/// adapter owns hashing, persistence, rate limiting, and session expiry.
pub trait AuthBackend: Send + Sync + 'static {
    fn login<'a>(
        &'a self,
        address: &'a str,
        password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>>;
    fn identity<'a>(
        &'a self,
        bearer: Option<&'a str>,
        cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>>;
    fn logout<'a>(&'a self, cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>>;
    fn change_password<'a>(
        &'a self,
        current: &'a str,
        new: &'a str,
        keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Identity {
    #[default]
    Owner,
    Api,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    TooManyAttempts,
    IncorrectPassword,
    SignInRequired,
    Internal(String),
}

impl AuthError {
    fn response(&self) -> Response {
        let (status, detail) = match self {
            Self::TooManyAttempts => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts; retry in five minutes".to_owned(),
            ),
            Self::IncorrectPassword => (StatusCode::UNAUTHORIZED, "Incorrect password".to_owned()),
            Self::SignInRequired => (StatusCode::UNAUTHORIZED, "Sign in required".to_owned()),
            Self::Internal(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .into_response();
            }
        };
        (status, Json(json!({"detail": detail}))).into_response()
    }
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct PasswordBody {
    current_password: String,
    new_password: String,
}

/// Mount the auth routes. The outer web application supplies its backend as
/// an `Extension<Arc<dyn AuthBackend>>`, keeping persistence out of handlers.
pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

/// Mount auth routes with the externally visible request scheme. When TLS
/// terminates at a reverse proxy, the application must set this from its
/// trusted proxy configuration rather than accepting client forwarding headers.
pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .layer(Extension(scheme))
}

async fn login(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    ConnectInfo(address): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    let address = address.ip().to_string();
    match backend.login(&address, &body.password).await {
        Ok(token) => {
            let next = body
                .next
                .as_deref()
                .and_then(|next| safe_next(next, &headers, scheme));
            let mut response =
                Json(json!({"ok": true, "next": next.unwrap_or_else(|| "/dash/cos".to_owned())}))
                    .into_response();
            set_cookie(&mut response, &token);
            response
        }
        Err(error) => error.response(),
    }
}

async fn logout(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    let cookie = cookie_value(&headers, SESSION_COOKIE);
    let bearer = bearer_value(&headers);
    match backend.identity(bearer.as_deref(), cookie.as_deref()).await {
        Err(error) => error.response(),
        Ok(_) => match backend.logout(cookie.as_deref()).await {
            Ok(()) => {
                let mut response = Json(json!({"ok": true})).into_response();
                delete_cookie(&mut response);
                response
            }
            Err(error) => error.response(),
        },
    }
}

async fn change_password(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
    Json(body): Json<PasswordBody>,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    if !(12..=200).contains(&body.new_password.chars().count()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": "new_password must contain between 12 and 200 characters"})),
        )
            .into_response();
    }
    let cookie = cookie_value(&headers, SESSION_COOKIE);
    let bearer = bearer_value(&headers);
    match backend.identity(bearer.as_deref(), cookie.as_deref()).await {
        Err(error) => error.response(),
        Ok(Identity::Api) => (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "Owner sign-in required"})),
        )
            .into_response(),
        Ok(Identity::Owner) => match backend
            .change_password(
                &body.current_password,
                &body.new_password,
                cookie.as_deref(),
            )
            .await
        {
            Ok(()) => Json(json!({"ok": true})).into_response(),
            Err(AuthError::IncorrectPassword) => (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": "Current password is incorrect"})),
            )
                .into_response(),
            Err(error) => error.response(),
        },
    }
}

fn same_origin(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if origin != format!("{}://{host}", scheme.as_str()) {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": "Cross-origin request refused"})),
            )
                .into_response(),
        )
    } else {
        None
    }
}

fn safe_next(value: &str, headers: &HeaderMap, request_scheme: RequestScheme) -> Option<String> {
    let candidate = value.trim();
    if candidate.is_empty() {
        return None;
    }
    // Browsers treat backslashes as slashes in HTTP(S) URLs, so a path like
    // `/\\evil.example` can become a cross-origin URL after navigation.
    if candidate.contains('\\') {
        return None;
    }
    // The WHATWG URL parser removes tabs and newlines before navigation.
    if candidate
        .chars()
        .any(|character| character.is_ascii_control())
    {
        return None;
    }
    if candidate.starts_with('/') && !candidate.starts_with("//") {
        return Some(candidate.to_owned());
    }
    let (scheme, remainder) = candidate.split_once("://")?;
    let authority = remainder.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let request_host = headers.get(header::HOST)?.to_str().ok()?;
    let request_port = request_host.rsplit_once(':')?.1;
    let request_host = request_host.rsplit_once(':')?.0;
    if host.eq_ignore_ascii_case(request_host)
        && port == request_port
        && scheme == request_scheme.as_str()
        && matches!(scheme, "http" | "https")
    {
        Some(candidate.to_owned())
    } else {
        None
    }
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

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn set_cookie(response: &mut Response, token: &str) {
    let value = format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_MAX_AGE}; HttpOnly; SameSite=Strict"
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn delete_cookie(response: &mut Response) {
    response.headers_mut().append(header::SET_COOKIE, HeaderValue::from_static("vp_session=; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    struct FakeAuth {
        identity: Mutex<Result<Identity, AuthError>>,
        login_error: Mutex<Option<AuthError>>,
        changed_password: Mutex<Option<(String, String, Option<String>)>>,
        logged_out: Mutex<Option<Option<String>>>,
    }

    impl Default for FakeAuth {
        fn default() -> Self {
            Self {
                identity: Mutex::new(Ok(Identity::Owner)),
                login_error: Mutex::new(None),
                changed_password: Mutex::new(None),
                logged_out: Mutex::new(None),
            }
        }
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            password: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            let configured_error = self.login_error.lock().expect("login lock").clone();
            Box::pin(async move {
                if let Some(error) = configured_error {
                    Err(error)
                } else if password == "correct" {
                    Ok("session-token".into())
                } else {
                    Err(AuthError::IncorrectPassword)
                }
            })
        }

        fn identity<'a>(
            &'a self,
            _bearer: Option<&'a str>,
            _cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            let result = self.identity.lock().expect("identity lock").clone();
            Box::pin(async move { result })
        }

        fn logout<'a>(&'a self, cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            *self.logged_out.lock().expect("logout lock") = Some(cookie.map(str::to_owned));
            Box::pin(async { Ok(()) })
        }

        fn change_password<'a>(
            &'a self,
            current: &'a str,
            new: &'a str,
            keep_cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            *self.changed_password.lock().expect("password lock") =
                Some((current.into(), new.into(), keep_cookie.map(str::to_owned)));
            Box::pin(async { Ok(()) })
        }
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

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("desk.test"));
        headers
    }

    fn body_json(response: Response) -> (StatusCode, Value, HeaderMap) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("response body");
        (
            status,
            serde_json::from_slice(&body).expect("JSON response"),
            headers,
        )
    }

    fn body_text(response: Response) -> (StatusCode, String, HeaderMap) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("response body");
        (
            status,
            String::from_utf8(body.to_vec()).expect("text response"),
            headers,
        )
    }

    #[test]
    fn login_returns_safe_next_and_strict_session_cookie() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("/dash/tasks".into()),
            }),
        ));
        let (status, body, headers) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true, "next": "/dash/tasks"}));
        let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.contains("vp_session=session-token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=604800"));
    }

    #[test]
    fn https_origin_form_request_uses_configured_scheme() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::HOST, HeaderValue::from_static("desk.test:443"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://desk.test:443"),
        );
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Https),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("https://desk.test:443/dash/tasks".into()),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"ok": true, "next": "https://desk.test:443/dash/tasks"})
        );
    }

    #[test]
    fn login_rejects_backslash_safe_next() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("/\\evil.example".into()),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true, "next": "/dash/cos"}));
    }

    #[test]
    fn login_rejects_ascii_controls_in_safe_next() {
        for control in ['\t', '\n', '\r'] {
            let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
            let mut headers = headers();
            headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
            let response = block_on(login(
                Extension(backend),
                Extension(RequestScheme::Http),
                ConnectInfo("127.0.0.1:4000".parse().unwrap()),
                headers,
                Json(LoginBody {
                    password: "correct".into(),
                    next: Some(format!("/{control}/evil.example")),
                }),
            ));
            let (status, body, _) = body_json(response);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, json!({"ok": true, "next": "/dash/cos"}));
        }
    }

    #[test]
    fn login_rejects_cross_origin_with_python_error_shape() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.test"),
        );
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: None,
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
    }

    #[test]
    fn login_internal_error_does_not_leak_backend_details() {
        let fake = Arc::new(FakeAuth {
            login_error: Mutex::new(Some(AuthError::Internal(
                "credential database path and secret".into(),
            ))),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = fake;
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: None,
            }),
        ));
        let (status, body, response_headers) = body_text(response);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
        assert!(!body.contains("credential database path and secret"));
        assert_eq!(
            response_headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn logout_requires_auth_and_deletes_session_cookie() {
        let unauthorized = Arc::new(FakeAuth {
            identity: Mutex::new(Err(AuthError::SignInRequired)),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = unauthorized;
        let mut headers = headers();
        headers.insert(header::COOKIE, HeaderValue::from_static("vp_session=old"));
        let response = block_on(logout(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers.clone(),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));

        let authorized = Arc::new(FakeAuth::default());
        let backend: Arc<dyn AuthBackend> = authorized.clone();
        let response = block_on(logout(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers,
        ));
        let (status, body, response_headers) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
        assert!(response_headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        assert_eq!(
            *authorized.logged_out.lock().unwrap(),
            Some(Some("old".into()))
        );
    }

    #[test]
    fn password_change_is_owner_only_and_keeps_current_session() {
        let fake = Arc::new(FakeAuth::default());
        let backend: Arc<dyn AuthBackend> = fake.clone();
        let mut headers = headers();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("vp_session=owner-session"),
        );
        let response = block_on(change_password(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers.clone(),
            Json(PasswordBody {
                current_password: "old".into(),
                new_password: "a sufficiently long password".into(),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
        assert_eq!(
            *fake.changed_password.lock().unwrap(),
            Some((
                "old".into(),
                "a sufficiently long password".into(),
                Some("owner-session".into())
            ))
        );

        let api = Arc::new(FakeAuth {
            identity: Mutex::new(Ok(Identity::Api)),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = api.clone();
        let response = block_on(change_password(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers,
            Json(PasswordBody {
                current_password: "old".into(),
                new_password: "a sufficiently long password".into(),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Owner sign-in required"}));
        assert!(api.changed_password.lock().unwrap().is_none());
    }
}
