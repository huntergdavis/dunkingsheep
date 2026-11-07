//! The bridge's failure kinds. The Python loop catches `(RuntimeError, ValueError, OSError)`,
//! prints only the TYPE NAME (Telegram URLs carry the bot token, so no message text is printed
//! from a network failure), sleeps five seconds and retries. Each variant keeps that name.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// Python `RuntimeError`: Telegram refusals and network failures, daemon refusals, state errors.
    Runtime(String),
    /// Python `ValueError`: routing or config values that cannot be used.
    Value(String),
    /// Python `OSError`: file, directory and lock failures.
    Os(String),
}

impl BridgeError {
    /// The name the Python loop prints: `Telegram bridge retrying after RuntimeError`.
    pub fn python_type(&self) -> &'static str {
        match self {
            BridgeError::Runtime(_) => "RuntimeError",
            BridgeError::Value(_) => "ValueError",
            BridgeError::Os(_) => "OSError",
        }
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::Runtime(text) | BridgeError::Value(text) | BridgeError::Os(text) => {
                formatter.write_str(text)
            }
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<ds_core::error::DunkError> for BridgeError {
    fn from(error: ds_core::error::DunkError) -> Self {
        BridgeError::Runtime(error.to_string())
    }
}
