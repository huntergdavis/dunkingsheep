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

pub trait Bot {
    /// `Telegram.call(method, **payload)`: payload keys whose value is null are dropped, and the
    /// `result` of an ok answer is returned.
    fn call(&self, method: &str, payload: Map<String, Value>) -> Result<Value, BridgeError>;
}

/// The real client. Owns a current-thread runtime, so callers see a blocking method.
pub struct HttpBot {
    base: String,
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
            client,
            runtime,
        })
    }
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
}
