//! Public system routes ported from `web/app.py`.
//!
//! The outer application supplies the environment-derived Herdr URL/label and
//! the built UI directory. Keeping those inputs explicit makes the handlers
//! deterministic and leaves production wiring to the runtime integration.

use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Extension, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

use super::auth::{AuthBackend, AuthError, RequestScheme};

#[derive(Clone, Debug)]
pub struct SystemConfig {
    pub herdr_webui_url: Option<String>,
    pub herdr_webui_label: Option<String>,
    pub ui_dist: PathBuf,
}

impl SystemConfig {
    pub fn new(
        herdr_webui_url: Option<String>,
        herdr_webui_label: Option<String>,
        ui_dist: impl Into<PathBuf>,
    ) -> Self {
        Self {
            herdr_webui_url,
            herdr_webui_label,
            ui_dist: ui_dist.into(),
        }
    }
}

/// Mount the system routes with explicit auth, environment, and build inputs.
pub fn router(
    config: SystemConfig,
    auth_backend: Arc<dyn AuthBackend>,
    request_scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/herdr-webui", get(herdr_webui))
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .with_state(config)
        .layer(Extension(auth_backend))
        .layer(Extension(request_scheme))
}

async fn herdr_webui(
    State(config): State<SystemConfig>,
    Extension(auth_backend): Extension<Arc<dyn AuthBackend>>,
    Extension(request_scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = same_origin(&headers, request_scheme) {
        return response;
    }
    let bearer = bearer_value(&headers);
    let cookie = cookie_value(&headers);
    if let Err(error) = auth_backend
        .identity(bearer.as_deref(), cookie.as_deref())
        .await
    {
        return auth_error_response(error);
    }

    let configured = config.herdr_webui_url.as_deref().unwrap_or_default().trim();
    let hostname = request_hostname(&headers);
    let url = host_agnostic_url(configured, hostname.as_deref());
    Json(json!({
        "url": url,
        "available": !url.is_empty(),
        "label": config.herdr_webui_label.as_deref().unwrap_or("Herdr web UI").trim(),
    }))
    .into_response()
}

fn same_origin(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    if origin != format!("{scheme}://{host}") {
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

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == "vp_session").then(|| value.to_owned())
        })
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

async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn version(State(config): State<SystemConfig>) -> Response {
    let index = config.ui_dist.join("index.html");
    match fs::metadata(&index) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Json(json!({"version": "unbuilt"})).into_response();
        }
        Err(_) => return internal_error(),
        Ok(_) => {}
    }

    let html = match fs::read_to_string(&index) {
        Ok(html) => html,
        Err(_) => return internal_error(),
    };
    if let Some(src) = first_script_src(&html) {
        return Json(json!({"version": src})).into_response();
    }

    match fs::metadata(&index).and_then(|metadata| metadata.modified()) {
        Ok(modified) => Json(json!({"version": system_time_ns(modified)})).into_response(),
        Err(_) => internal_error(),
    }
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn request_hostname(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    if let Some(bracketed) = host.strip_prefix('[') {
        let end = bracketed.find(']')?;
        return Some(bracketed[..end].to_ascii_lowercase());
    }
    let hostname = match host.rsplit_once(':') {
        Some((hostname, _)) if !hostname.contains(':') => hostname,
        _ => host,
    };
    if hostname.is_empty() {
        None
    } else {
        Some(hostname.to_ascii_lowercase())
    }
}

fn host_agnostic_url(configured: &str, hostname: Option<&str>) -> String {
    let Some(hostname) = hostname.filter(|hostname| !hostname.is_empty()) else {
        return configured.to_owned();
    };
    let Some((scheme, rest)) = configured.split_once("://") else {
        return configured.to_owned();
    };
    if scheme.is_empty()
        || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
    {
        return configured.to_owned();
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return configured.to_owned();
    }
    let authority_host = authority.rsplit('@').next().unwrap_or(authority);
    let port = match authority_host {
        value if value.starts_with('[') => {
            let Some(close) = value.find(']') else {
                return configured.to_owned();
            };
            if !valid_bracketed_host(&value[1..close]) {
                return configured.to_owned();
            }
            match &value[close + 1..] {
                "" => None,
                suffix if suffix.starts_with(':') => match parse_port(&suffix[1..]) {
                    Some(port) => Some(port),
                    None if suffix.len() == 1 => None,
                    None => return configured.to_owned(),
                },
                _ => return configured.to_owned(),
            }
        }
        value => match value.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => match parse_port(port) {
                Some(port) => Some(port),
                None if port.is_empty() => None,
                None => return configured.to_owned(),
            },
            Some(_) => return configured.to_owned(),
            None => None,
        },
    };

    let host = if hostname.contains(':') && !hostname.starts_with('[') {
        format!("[{hostname}]")
    } else {
        hostname.to_owned()
    };
    let netloc = match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    };
    let suffix = &rest[authority_end..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    let path = &suffix[..path_end];
    let query_and_fragment = &suffix[path_end..];
    let query = query_and_fragment
        .split_once('#')
        .map_or(query_and_fragment, |(before_fragment, _)| before_fragment);
    format!(
        "{scheme}://{netloc}{}{query}",
        if path.is_empty() { "/" } else { path }
    )
}

fn valid_bracketed_host(host: &str) -> bool {
    if host.starts_with('v') {
        return host
            .strip_prefix('v')
            .and_then(|future| future.split_once('.'))
            .is_some_and(|(version, address)| {
                !version.is_empty()
                    && version.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !address.is_empty()
            });
    }
    host.parse::<std::net::Ipv6Addr>().is_ok()
}

fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn first_script_src(html: &str) -> Option<&str> {
    let mut search_from = 0;
    while let Some(relative) = html[search_from..].find("<script") {
        let start = search_from + relative;
        let attrs_start = start + "<script".len();
        let boundary = html[attrs_start..]
            .find('>')
            .map_or(html.len(), |offset| attrs_start + offset);
        let attrs = &html[attrs_start..boundary];
        if attrs.len() > 1 {
            let mut candidate_end = attrs.len();
            while let Some(position) = attrs[..candidate_end].rfind("src=\"") {
                if position == 0 {
                    break;
                }
                let value_start = position + "src=\"".len();
                if let Some(relative_end) = attrs[value_start..].find('"') {
                    if relative_end > 0 {
                        return Some(&attrs[value_start..value_start + relative_end]);
                    }
                }
                candidate_end = position;
            }
        }
        search_from = attrs_start;
    }
    None
}

fn system_time_ns(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos().to_string(),
        Err(error) => format!("-{}", error.duration().as_nanos()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        pin::pin,
        sync::atomic::{AtomicU64, Ordering},
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::super::auth::{AuthFuture, Identity};
    use super::{router as build_router, AuthBackend, AuthError, RequestScheme, SystemConfig};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir() // tempdir-ok: ds-web has no tempfile dev dependency; Drop removes each fixture.
                .join(format!(
                    "ds-web-system-{}-{}",
                    std::process::id(),
                    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).expect("create temp directory");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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

    fn config(ui_dist: &std::path::Path, url: &str, label: &str) -> SystemConfig {
        SystemConfig::new(Some(url.to_owned()), Some(label.to_owned()), ui_dist)
    }

    #[derive(Default)]
    struct FakeAuth {
        identities: Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }

        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            self.identities
                .lock()
                .expect("identity lock")
                .push((bearer.map(str::to_owned), cookie.map(str::to_owned)));
            let identity = if bearer == Some("test-token") || cookie == Some("session-token") {
                Ok(Identity::Api)
            } else {
                Err(AuthError::SignInRequired)
            };
            Box::pin(async move { identity })
        }

        fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
    }

    fn app(config: SystemConfig) -> axum::Router {
        build_router(config, Arc::new(FakeAuth::default()), RequestScheme::Http)
    }

    fn bearer_request(path: &str, host: Option<&str>) -> Request<Body> {
        let mut request = request(path, host);
        if let Some(host) = host {
            request.headers_mut().insert(
                header::ORIGIN,
                format!("http://{host}").parse().expect("origin header"),
            );
        }
        request.headers_mut().insert(
            header::AUTHORIZATION,
            "Bearer test-token".parse().expect("authorization header"),
        );
        request
    }

    fn cookie_request(path: &str, host: &str) -> Request<Body> {
        let mut request = request(path, Some(host));
        request.headers_mut().insert(
            header::ORIGIN,
            format!("http://{host}").parse().expect("origin header"),
        );
        request.headers_mut().insert(
            header::COOKIE,
            "vp_session=session-token".parse().expect("cookie header"),
        );
        request
    }

    fn request(path: &str, host: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().uri(path);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        builder.body(Body::empty()).expect("request")
    }

    fn response_json(response: axum::response::Response) -> (StatusCode, String, Value) {
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content type")
            .to_str()
            .expect("content type string")
            .to_owned();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).expect("response body");
        let body = serde_json::from_slice(&bytes).expect("JSON response");
        (status, content_type, body)
    }

    #[test]
    fn herdr_webui_requires_valid_authentication() {
        let temp = TempDir::new();
        let app = app(config(&temp.0, "https://herdr.example", "Herdr web UI"));
        let response = block_on(app.oneshot(request("/api/herdr-webui", Some("desk.test"))))
            .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"detail": "Sign in required"}));
    }

    #[test]
    fn herdr_webui_rejects_cross_origin_before_authentication() {
        let temp = TempDir::new();
        let backend = Arc::new(FakeAuth::default());
        let app = build_router(
            config(&temp.0, "https://herdr.example", "Herdr web UI"),
            backend.clone(),
            RequestScheme::Http,
        );
        let mut request = bearer_request("/api/herdr-webui", Some("desk.test"));
        request.headers_mut().insert(
            header::ORIGIN,
            "http://evil.test".parse().expect("origin header"),
        );
        let response = block_on(app.oneshot(request)).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
        assert!(backend.identities.lock().unwrap().is_empty());
    }

    #[test]
    fn herdr_webui_accepts_bearer_and_session_authentication() {
        let temp = TempDir::new();
        let backend = Arc::new(FakeAuth::default());
        let app = build_router(
            config(&temp.0, "https://herdr.example", "Herdr web UI"),
            backend.clone(),
            RequestScheme::Http,
        );
        let bearer = block_on(
            app.clone()
                .oneshot(bearer_request("/api/herdr-webui", Some("desk.test"))),
        )
        .expect("bearer response");
        assert_eq!(bearer.status(), StatusCode::OK);
        let cookie = block_on(app.oneshot(cookie_request("/api/herdr-webui", "desk.test")))
            .expect("cookie response");
        assert_eq!(cookie.status(), StatusCode::OK);
        assert_eq!(
            *backend.identities.lock().unwrap(),
            vec![
                (Some("test-token".into()), None),
                (None, Some("session-token".into()))
            ]
        );
    }

    #[test]
    fn herdr_webui_rewrites_host_and_preserves_configured_parts() {
        let temp = TempDir::new();
        let app = app(config(
            &temp.0,
            " https://configured.example:9999/ui/panes?mode=full#discard ",
            "  Herdr web UI  ",
        ));
        let response = block_on(app.oneshot(bearer_request(
            "/api/herdr-webui",
            Some("Owner.Example:8443"),
        )))
        .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": "https://owner.example:9999/ui/panes?mode=full",
                "available": true,
                "label": "Herdr web UI"
            })
        );
    }

    #[test]
    fn herdr_webui_empty_and_invalid_config_preserves_source_response() {
        let temp = TempDir::new();
        let empty = app(config(&temp.0, "   ", "   "));
        let response =
            block_on(empty.oneshot(bearer_request("/api/herdr-webui", Some("desk.test"))))
                .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"url": "", "available": false, "label": ""}));

        let invalid = app(config(
            &temp.0,
            "https://configured.example:99999/path#fragment",
            "label",
        ));
        let response =
            block_on(invalid.oneshot(bearer_request("/api/herdr-webui", Some("desk.test"))))
                .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": "https://configured.example:99999/path#fragment",
                "available": true,
                "label": "label"
            })
        );
    }

    #[test]
    fn herdr_webui_brackets_ipv6_and_keeps_configured_port() {
        let temp = TempDir::new();
        let app = app(config(&temp.0, "http://configured.example:8080", "ui"));
        let response = block_on(app.oneshot(bearer_request(
            "/api/herdr-webui",
            Some("[2001:db8::1]:9000"),
        )))
        .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": "http://[2001:db8::1]:8080/",
                "available": true,
                "label": "ui"
            })
        );
    }

    #[test]
    fn herdr_webui_keeps_configured_url_when_host_is_missing() {
        let temp = TempDir::new();
        let app = app(config(&temp.0, "not a URL#fragment", "ui"));
        let response = block_on(app.oneshot(bearer_request("/api/herdr-webui", None)))
            .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": "not a URL#fragment",
                "available": true,
                "label": "ui"
            })
        );
    }

    #[test]
    fn health_returns_public_ok() {
        let temp = TempDir::new();
        let app = app(config(&temp.0, "", "Herdr web UI"));
        let response = block_on(app.oneshot(request("/api/health", None))).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"ok": true}));
    }

    #[test]
    fn version_missing_index_returns_unbuilt() {
        let temp = TempDir::new();
        let app = app(config(&temp.0, "", "Herdr web UI"));
        let response =
            block_on(app.oneshot(request("/api/version", None))).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"version": "unbuilt"}));
    }

    #[test]
    fn version_uses_first_script_source() {
        let temp = TempDir::new();
        fs::write(
            temp.0.join("index.html"),
            "<script type=\"module\" src=\"/assets/first.js\"></script><script defer src=\"/assets/second.js\"></script>",
        )
        .expect("write index fixture");
        let app = app(config(&temp.0, "", "Herdr web UI"));
        let response =
            block_on(app.oneshot(request("/api/version", None))).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"version": "/assets/first.js"}));
    }

    #[test]
    fn version_without_script_uses_mtime_ns_string() {
        let temp = TempDir::new();
        let index = temp.0.join("index.html");
        fs::write(&index, "<main>built</main>").expect("write index fixture");
        let duration = fs::metadata(&index)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let expected = (duration.as_secs() as u128 * 1_000_000_000
            + u128::from(duration.subsec_nanos()))
        .to_string();
        let app = app(config(&temp.0, "", "Herdr web UI"));
        let response =
            block_on(app.oneshot(request("/api/version", None))).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"version": expected}));
        assert!(body["version"].as_str().unwrap().parse::<i128>().is_ok());
    }

    #[test]
    fn version_read_error_returns_server_error_without_path_leakage() {
        let temp = TempDir::new();
        fs::create_dir(temp.0.join("index.html")).expect("create directory fixture");
        let app = app(config(&temp.0, "", "Herdr web UI"));
        let response =
            block_on(app.oneshot(request("/api/version", None))).expect("route response");
        let status = response.status();
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).expect("response body");
        let text = String::from_utf8(body.to_vec()).expect("UTF-8 error response");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(text, "Internal Server Error");
        assert!(!text.contains(temp.0.to_str().unwrap()));
    }

    #[test]
    fn herdr_webui_uses_source_defaults_for_missing_environment_values() {
        let temp = TempDir::new();
        let app = app(SystemConfig::new(None, None, &temp.0));
        let response = block_on(app.oneshot(bearer_request("/api/herdr-webui", None)))
            .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": "",
                "available": false,
                "label": "Herdr web UI"
            })
        );
    }

    #[test]
    fn herdr_webui_leaves_configured_url_when_bracketed_host_is_invalid() {
        let temp = TempDir::new();
        let configured = "https://[not-ipv6]:8080/ui#fragment";
        let app = app(config(&temp.0, configured, "ui"));
        let response = block_on(app.oneshot(bearer_request("/api/herdr-webui", Some("desk.test"))))
            .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            body,
            json!({
                "url": configured,
                "available": true,
                "label": "ui"
            })
        );
    }
}
