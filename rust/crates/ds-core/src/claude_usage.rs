//! Claude subscription usage, using the same OAuth credential file and endpoint as the existing
//! configured provider. Tokens and HTTP response bodies are never included in errors.
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const MAX_RESPONSE: usize = 1024 * 1024;

fn error(now: f64, message: &str) -> Value {
    json!({"provider":"claude", "status":"error", "meaning":"unknown", "measured_at":now, "error":message})
}

/// Read the configured local OAuth credential and query the existing subscription endpoint.
/// No environment API key or alternate credential source is used.
pub async fn subscription_usage(home: &Path, now: f64) -> Value {
    subscription_usage_at(home, now, ENDPOINT, Duration::from_secs(20)).await
}

async fn subscription_usage_at(home: &Path, now: f64, endpoint: &str, timeout: Duration) -> Value {
    let path = home.join(".claude/.credentials.json");
    let mut contents = Vec::new();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return error(now, "cannot read Claude credentials"),
    };
    if file
        .take((MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut contents)
        .is_err()
        || contents.len() > MAX_RESPONSE
    {
        return error(now, "cannot read Claude credentials");
    }
    let credentials: Value = match serde_json::from_slice(&contents) {
        Ok(value) => value,
        Err(_) => return error(now, "cannot parse Claude credentials"),
    };
    let oauth = &credentials["claudeAiOauth"];
    let token = match oauth["accessToken"].as_str().filter(|s| !s.is_empty()) {
        Some(token) => token,
        None => return error(now, "no Claude OAuth accessToken in credentials"),
    };
    if !oauth["expiresAt"].is_null() {
        let expiry = oauth["expiresAt"]
            .as_f64()
            .or_else(|| oauth["expiresAt"].as_str().and_then(|s| s.parse().ok()));
        let Some(mut expiry) = expiry.filter(|v| v.is_finite()) else {
            return error(now, "invalid Claude OAuth expiry");
        };
        if expiry > 1e12 {
            expiry /= 1000.0;
        }
        if expiry <= now + 60.0 {
            return error(now, "OAuth access token expired; run claude to refresh it");
        }
    }
    let client = match reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return error(now, "cannot initialize Claude usage client"),
    };
    let mut response = match client
        .get(endpoint)
        .bearer_auth(token)
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "dunkingsheep-usage/1")
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) if e.is_timeout() => return error(now, "Claude usage request timed out"),
        Err(_) => return error(now, "Claude usage request failed"),
    };
    if !response.status().is_success() {
        return error(
            now,
            &format!("Claude usage endpoint HTTP {}", response.status().as_u16()),
        );
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len().saturating_add(chunk.len()) <= MAX_RESPONSE => {
                body.extend_from_slice(&chunk)
            }
            Ok(Some(_)) => return error(now, "Claude usage response exceeds size limit"),
            Ok(None) => break,
            Err(_) => return error(now, "Claude usage response failed"),
        }
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error(now, "Claude usage response is invalid JSON"),
    };
    let mut windows = Vec::new();
    for (name, key) in [("five_hour", "five_hour"), ("weekly_all", "seven_day")] {
        if let Some(used) = payload[key]["utilization"]
            .as_f64()
            .filter(|n| n.is_finite())
        {
            windows.push(json!({"name":name, "percent_used":used,
                "percent_remaining":((100.0-used)*10.0).round()/10.0, "resets_at":payload[key]["resets_at"]}));
        }
    }
    if windows.is_empty() {
        return error(now, "usage endpoint returned no utilization windows");
    }
    let extra = payload
        .get("extra_usage")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut notes = Vec::new();
    if !extra["utilization"].is_null() {
        fn display(v: &Value) -> String {
            if v.is_null() {
                "None".to_owned()
            } else if let Some(s) = v.as_str() {
                s.to_owned()
            } else {
                v.to_string()
            }
        }
        notes.push(format!(
            "extra_usage credits {} of {} used ({}% used)",
            display(&extra["used_credits"]),
            display(&extra["monthly_limit"]),
            display(&extra["utilization"])
        ));
    }
    let mut result = json!({"provider":"claude", "status":"ok", "meaning":"percent_used", "measured_at":now,
        "source":ENDPOINT, "subscription":oauth["subscriptionType"], "rate_limit_tier":oauth["rateLimitTier"],
        "windows":windows, "extra_usage":extra, "notes":notes});
    redact_token(&mut result, token);
    result
}

fn redact_token(value: &mut Value, token: &str) {
    match value {
        Value::String(text) => *text = text.replace(token, "[redacted]"),
        Value::Number(number) if number.to_string().contains(token) => {
            *value = Value::String(number.to_string().replace(token, "[redacted]"));
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact_token(item, token)),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                redact_token(&mut item, token);
                map.insert(key.replace(token, "[redacted]"), item);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn credentials(home: &Path, value: &str) {
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".claude/.credentials.json"), value).unwrap();
    }
    fn fake_server(
        status: &str,
        body: &str,
        delay_ms: u64,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        let body = body.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let count = socket.read(&mut request).unwrap();
            std::thread::sleep(Duration::from_millis(delay_ms));
            let _ = write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            String::from_utf8_lossy(&request[..count]).to_string()
        });
        (format!("http://{address}/usage"), handle)
    }
    #[tokio::test]
    async fn missing_malformed_and_expired_credentials_fail_without_network() {
        let home = tempfile::tempdir().unwrap();
        let call = || {
            subscription_usage_at(
                home.path(),
                1000.0,
                "http://127.0.0.1:1",
                Duration::from_secs(1),
            )
        };
        assert_eq!(call().await["error"], "cannot read Claude credentials");
        credentials(home.path(), "not json");
        assert_eq!(call().await["error"], "cannot parse Claude credentials");
        credentials(home.path(), r#"{"claudeAiOauth":{}}"#);
        assert_eq!(
            call().await["error"],
            "no Claude OAuth accessToken in credentials"
        );
        credentials(
            home.path(),
            r#"{"claudeAiOauth":{"accessToken":"dummy-secret","expiresAt":1000}}"#,
        );
        let value = call().await;
        assert!(value["error"].as_str().unwrap().contains("expired"));
        assert!(!value.to_string().contains("dummy-secret"));
    }
    #[tokio::test]
    async fn success_uses_oauth_and_preserves_subscription_windows() {
        let home = tempfile::tempdir().unwrap();
        credentials(
            home.path(),
            r#"{"claudeAiOauth":{"accessToken":"dummy-secret","expiresAt":9999999,"subscriptionType":"pro","rateLimitTier":"dummy-secret"}}"#,
        );
        let (url, server) = fake_server(
            "200 OK",
            r#"{"five_hour":{"utilization":12.0,"resets_at":"dummy-secret"},"seven_day":{"utilization":50.0},"extra_usage":{"used_credits":1,"monthly_limit":4,"utilization":25,"nested":{"dummy-secret":"dummy-secret"}}}"#,
            0,
        );
        let value = subscription_usage_at(home.path(), 1000.0, &url, Duration::from_secs(1)).await;
        let request = server.join().unwrap();
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer dummy-secret"));
        assert_eq!(value["status"], "ok");
        assert_eq!(value["subscription"], "pro");
        assert_eq!(value["windows"][0]["percent_remaining"], 88.0);
        assert_eq!(value["windows"][1]["name"], "weekly_all");
        assert!(!value.to_string().contains("dummy-secret"));
    }
    #[tokio::test]
    async fn cross_origin_redirect_never_receives_a_request() {
        let home = tempfile::tempdir().unwrap();
        credentials(
            home.path(),
            r#"{"claudeAiOauth":{"accessToken":"dummy-secret"}}"#,
        );
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let location = destination.local_addr().unwrap();
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = source.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = source.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).unwrap();
            write!(socket, "HTTP/1.1 302 Found\r\nLocation: http://{location}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let value = subscription_usage_at(
            home.path(),
            1000.0,
            &format!("http://{address}/usage"),
            Duration::from_secs(1),
        )
        .await;
        server.join().unwrap();
        assert!(value["error"].as_str().unwrap().contains("HTTP 302"));
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[tokio::test]
    async fn errors_are_bounded_sanitized_and_redirects_are_not_followed() {
        let home = tempfile::tempdir().unwrap();
        credentials(
            home.path(),
            r#"{"claudeAiOauth":{"accessToken":"dummy-secret"}}"#,
        );
        for (status, body, expected) in [
            ("429 Too Many Requests", "dummy-secret", "HTTP 429"),
            ("302 Found", "dummy-secret", "HTTP 302"),
            ("200 OK", "not json dummy-secret", "invalid JSON"),
            ("200 OK", "{}", "no utilization"),
        ] {
            let (url, server) = fake_server(status, body, 0);
            let value =
                subscription_usage_at(home.path(), 1000.0, &url, Duration::from_secs(1)).await;
            server.join().unwrap();
            assert!(value["error"].as_str().unwrap().contains(expected));
            assert!(!value.to_string().contains("dummy-secret"));
        }
        let oversized = "x".repeat(MAX_RESPONSE + 1);
        let (url, server) = fake_server("200 OK", &oversized, 0);
        let value = subscription_usage_at(home.path(), 1000.0, &url, Duration::from_secs(3)).await;
        server.join().unwrap();
        assert!(value["error"].as_str().unwrap().contains("size limit"));
        let (url, server) = fake_server("200 OK", "{}", 100);
        let value =
            subscription_usage_at(home.path(), 1000.0, &url, Duration::from_millis(10)).await;
        server.join().unwrap();
        assert!(value["error"].as_str().unwrap().contains("timed out"));
    }
}
