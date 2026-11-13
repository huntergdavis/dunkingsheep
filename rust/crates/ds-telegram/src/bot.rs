//! The Telegram Bot API, as the bridge uses it: one JSON POST per method.
//!
//! `Bot` is the seam the tests replace with a fake, so no test touches the network or a token.
//! `HttpBot` is the real client. It never formats a `reqwest` error: those texts carry the request
//! URL, and the URL carries the bot token. Every failure is one of the fixed Python texts.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::error::BridgeError;
use crate::pyval::present;

/// The Python `Telegram.call` timeout, in seconds.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(40);
/// The Bot API download limit enforced by `telegram_bridge.py`.
pub const DOWNLOAD_LIMIT: usize = 20 * 1024 * 1024;
const DOWNLOAD_LIMIT_REPLY: &str =
    "Telegram Bot API cannot download files over 20 MB; use the VP web desk.";

pub trait Bot {
    /// `Telegram.call(method, **payload)`: payload keys whose value is null are dropped, and the
    /// `result` of an ok answer is returned.
    fn call(&self, method: &str, payload: Map<String, Value>) -> Result<Value, BridgeError>;

    /// `Telegram.download`: resolve and fetch a Bot API file, with the Python size guard.
    fn download(&self, file_id: &str) -> Result<Vec<u8>, BridgeError>;
}

/// The real client. Owns a current-thread runtime, so callers see a blocking method.
pub struct HttpBot {
    base: String,
    file_base: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl HttpBot {
    pub fn new(token: &str) -> Result<Self, BridgeError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| BridgeError::Os(format!("runtime: {}", error.kind())))?;
        let client = reqwest::Client::builder()
            .timeout(CALL_TIMEOUT)
            .build()
            .map_err(|_| BridgeError::Runtime("Telegram client unavailable".into()))?;
        Ok(HttpBot {
            base: format!("https://api.telegram.org/bot{token}/"),
            file_base: format!("https://api.telegram.org/file/bot{token}/"),
            client,
            runtime,
        })
    }

    async fn fetch_file(&self, path: &str) -> Result<Vec<u8>, BridgeError> {
        let url = format!("{}{}", self.file_base, quote_file_path(path));
        let mut response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|_| BridgeError::Runtime("Telegram file download failed".into()))?;
        if !response.status().is_success() {
            return Err(BridgeError::Runtime("Telegram file download failed".into()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > DOWNLOAD_LIMIT as u64)
        {
            return Err(BridgeError::Value(DOWNLOAD_LIMIT_REPLY.into()));
        }
        let mut content = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| BridgeError::Runtime("Telegram file download failed".into()))?
        {
            if content.len().saturating_add(chunk.len()) > DOWNLOAD_LIMIT {
                return Err(BridgeError::Value(DOWNLOAD_LIMIT_REPLY.into()));
            }
            content.extend_from_slice(&chunk);
        }
        Ok(content)
    }
}

/// `urllib.parse.quote(path, safe="/")`, preserving Telegram's path separators only.
fn quote_file_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            encoded.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

/// The `description` of a Telegram error body, or the fallback the Python code uses.
fn description_of(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| fallback.to_string())
}

impl Bot for HttpBot {
    fn call(&self, method: &str, payload: Map<String, Value>) -> Result<Value, BridgeError> {
        let body: Map<String, Value> = payload
            .into_iter()
            .filter(|(_, value)| present(Some(value)).is_some())
            .collect();
        let url = format!("{}{}", self.base, method);
        self.runtime.block_on(async {
            let response = match self.client.post(url.as_str()).json(&body).send().await {
                Ok(response) => response,
                Err(_) => {
                    return Err(BridgeError::Runtime(format!(
                        "Telegram {method}: network unavailable"
                    )))
                }
            };
            let status = response.status();
            let bytes = response.bytes().await.map_err(|_| {
                BridgeError::Runtime(format!("Telegram {method}: network unavailable"))
            })?;
            if !status.is_success() {
                let description = description_of(&bytes, "HTTP error");
                return Err(BridgeError::Runtime(format!(
                    "Telegram {method}: {description}"
                )));
            }
            let answer: Value = serde_json::from_slice(&bytes)
                .map_err(|error| BridgeError::Value(error.to_string()))?;
            if answer.get("ok") != Some(&Value::Bool(true)) {
                let description = answer
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                return Err(BridgeError::Runtime(format!(
                    "Telegram {method}: {description}"
                )));
            }
            Ok(answer.get("result").cloned().unwrap_or(Value::Null))
        })
    }

    fn download(&self, file_id: &str) -> Result<Vec<u8>, BridgeError> {
        let mut payload = Map::new();
        payload.insert("file_id".into(), Value::String(file_id.to_string()));
        let info = self.call("getFile", payload)?;
        if info
            .get("file_size")
            .and_then(Value::as_u64)
            .is_some_and(|size| size > DOWNLOAD_LIMIT as u64)
        {
            return Err(BridgeError::Value(DOWNLOAD_LIMIT_REPLY.into()));
        }
        let path = info
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::Runtime("Telegram file download failed".into()))?;
        self.runtime.block_on(self.fetch_file(path))
    }
}
