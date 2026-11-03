//! Public system routes ported from `web/app.py`.
//!
//! The outer application supplies the environment-derived Herdr URL/label and
//! the built UI directory. Keeping those inputs explicit makes the handlers
//! deterministic and leaves production wiring to the runtime integration.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

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

/// Mount the public system routes with explicit environment and build inputs.
pub fn router(config: SystemConfig) -> Router {
    Router::new()
        .route("/api/herdr-webui", get(herdr_webui))
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .with_state(config)
}

async fn herdr_webui(
    State(config): State<SystemConfig>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let configured = config.herdr_webui_url.as_deref().unwrap_or_default().trim();
    let hostname = request_hostname(&headers);
    let url = host_agnostic_url(configured, hostname.as_deref());
    Json(json!({
        "url": url,
        "available": !url.is_empty(),
        "label": config.herdr_webui_label.as_deref().unwrap_or("Herdr web UI").trim(),
    }))
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
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request, StatusCode},
    };
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::{router, SystemConfig};

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
    fn herdr_webui_rewrites_host_and_preserves_configured_parts() {
        let temp = TempDir::new();
        let app = router(config(
            &temp.0,
            " https://configured.example:9999/ui/panes?mode=full#discard ",
            "  Herdr web UI  ",
        ));
        let response =
            block_on(app.oneshot(request("/api/herdr-webui", Some("Owner.Example:8443"))))
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
        let empty = router(config(&temp.0, "   ", "   "));
        let response = block_on(empty.oneshot(request("/api/herdr-webui", Some("desk.test"))))
            .expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"url": "", "available": false, "label": ""}));

        let invalid = router(config(
            &temp.0,
            "https://configured.example:99999/path#fragment",
            "label",
        ));
        let response = block_on(invalid.oneshot(request("/api/herdr-webui", Some("desk.test"))))
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
        let app = router(config(&temp.0, "http://configured.example:8080", "ui"));
        let response =
            block_on(app.oneshot(request("/api/herdr-webui", Some("[2001:db8::1]:9000"))))
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
        let app = router(config(&temp.0, "not a URL#fragment", "ui"));
        let response =
            block_on(app.oneshot(request("/api/herdr-webui", None))).expect("route response");
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
        let app = router(config(&temp.0, "", "Herdr web UI"));
        let response = block_on(app.oneshot(request("/api/health", None))).expect("route response");
        let (status, content_type, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(body, json!({"ok": true}));
    }

    #[test]
    fn version_missing_index_returns_unbuilt() {
        let temp = TempDir::new();
        let app = router(config(&temp.0, "", "Herdr web UI"));
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
        let app = router(config(&temp.0, "", "Herdr web UI"));
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
        let app = router(config(&temp.0, "", "Herdr web UI"));
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
        let app = router(config(&temp.0, "", "Herdr web UI"));
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
        let app = router(SystemConfig::new(None, None, &temp.0));
        let response =
            block_on(app.oneshot(request("/api/herdr-webui", None))).expect("route response");
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
        let app = router(config(&temp.0, configured, "ui"));
        let response = block_on(app.oneshot(request("/api/herdr-webui", Some("desk.test"))))
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
