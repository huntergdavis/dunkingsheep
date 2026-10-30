//! `DunkError`: the one error type, with the names the Python already uses.
//!
//! Ported from `dunk_core.py` (the `DunkError` family, RUST_PORT.md 4.3). The
//! text a caller sees is [`Display`], because clients and tests match on it: the
//! manifest lists 161 strings tests assert on, and they must stay byte-identical.

use std::fmt;

/// What went wrong, in the categories the wire protocol already distinguishes.
#[derive(Debug, thiserror::Error)]
pub enum DunkError {
    /// A named refusal (the Python `DunkError`): exit 2 in the CLI.
    #[error("{0}")]
    Refused(String),
    /// The Python `RetryableDunkError`: `retryable: true` on the wire.
    #[error("{0}")]
    Retryable(String),
    /// The Python `AmbiguousTarget`.
    #[error("{0}")]
    Ambiguous(String),
    /// Identity could not be proven (t602: no pane and no token is anonymous).
    #[error("identity refused: {0}")]
    Identity(String),
    /// The daemon is not reachable (client side only).
    #[error("daemon unavailable: {0}")]
    Unavailable(String),
    /// SQLite said busy/locked; the caller may retry the request.
    #[error("database busy; retry the request")]
    Busy,
}

impl DunkError {
    /// The CLI exit code for this error (RUST_PORT.md 4.3).
    ///
    /// 2 for a refusal, 1 for an unreachable daemon. The long-running "still
    /// working" state (75) is not an error and is handled by the caller.
    pub fn exit_code(&self) -> i32 {
        match self {
            DunkError::Refused(_) | DunkError::Ambiguous(_) | DunkError::Identity(_) => 2,
            DunkError::Unavailable(_) => 1,
            DunkError::Retryable(_) | DunkError::Busy => 1,
        }
    }

    /// Whether the wire response should carry `retryable: true`.
    pub fn retryable(&self) -> bool {
        matches!(self, DunkError::Retryable(_) | DunkError::Busy)
    }
}

impl From<std::io::Error> for DunkError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::TimedOut => DunkError::Retryable(error.to_string()),
            _ => DunkError::Refused(error.to_string()),
        }
    }
}

impl From<serde_json::Error> for DunkError {
    fn from(error: serde_json::Error) -> Self {
        DunkError::Refused(format!("invalid JSON: {error}"))
    }
}

/// A refusal whose text is built at the call site, the way `DunkError("...")` is.
impl From<String> for DunkError {
    fn from(text: String) -> Self {
        DunkError::Refused(text)
    }
}

impl From<&str> for DunkError {
    fn from(text: &str) -> Self {
        DunkError::Refused(text.to_string())
    }
}

/// The one helper every refusal goes through, so the message shape cannot drift.
pub fn refusal<T: fmt::Display>(text: T) -> DunkError {
    DunkError::Refused(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_is_the_python_text() {
        assert_eq!(DunkError::Busy.to_string(), "database busy; retry the request");
        assert_eq!(DunkError::Identity("no pane".into()).to_string(),
                   "identity refused: no pane");
        assert_eq!(DunkError::Refused("no task with id t9".into()).to_string(),
                   "no task with id t9");
    }

    #[test]
    fn exit_codes_match_the_contract() {
        assert_eq!(2, DunkError::Refused("x".into()).exit_code());
        assert_eq!(2, DunkError::Ambiguous("x".into()).exit_code());
        assert_eq!(2, DunkError::Identity("x".into()).exit_code());
        assert_eq!(1, DunkError::Unavailable("x".into()).exit_code());
    }

    #[test]
    fn only_retryable_categories_say_retryable() {
        assert!(DunkError::Retryable("x".into()).retryable());
        assert!(DunkError::Busy.retryable());
        assert!(!DunkError::Refused("x".into()).retryable());
    }

    #[test]
    fn a_timeout_io_error_is_retryable_and_other_io_is_a_refusal() {
        let timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer after 45s");
        assert!(DunkError::from(timeout).retryable());
        let missing = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert!(!DunkError::from(missing).retryable());
    }
}
