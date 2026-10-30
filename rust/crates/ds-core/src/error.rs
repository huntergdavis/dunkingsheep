//! The error family (T02), ported from `dunk_core.py` 423-441 and `dunk_superusers`.
//!
//! Port rule 0.6: every fallible function returns `Result<T, DunkError>`, and the text
//! the client sees is `Display`. The Python classes are a hierarchy (`DunkError(ValueError)`
//! with `AmbiguousTarget` under it); the Rust port keeps the *distinction* in an enum and
//! the *messages* byte-identical, because the tests and the watchdog grep them.

use std::fmt;

/// A caller mistake (bad id, bad interval, ambiguous target). Safe to show.
///
/// The variants are the classes the Python raises as separate types; a variant keeps
/// the same text so a ported test asserts on the same string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DunkError {
    /// `DunkError` itself: a message the caller can act on.
    Message(String),
    /// `AmbiguousTarget`: a target matched more than one pane. Distinct from "no such
    /// pane", because a mailbox may be a name with no pane behind it, but it must never
    /// be a name that stands for several panes at once.
    AmbiguousTarget(String),
    /// `SuperuserError`: the allowlist/registry file cannot be read or is malformed.
    Store(String),
    /// A task/queue refusal that names its reason (`busy`, `owned_elsewhere`, ...).
    Refused(String),
}

impl DunkError {
    /// The stable reason code when the error is a refusal, else `None`.
    ///
    /// `claim_next` returns refusal reasons as strings (`busy`, `empty`,
    /// `owned_elsewhere`) and callers branch on them; a port that only carried the
    /// message would lose that.
    pub fn reason(&self) -> Option<&str> {
        match self {
            DunkError::Refused(reason) => Some(reason.as_str()),
            _ => None,
        }
    }

    /// True when the error is safe to show a caller verbatim.
    ///
    /// The Python docstring says "safe to show" of `DunkError`; a store error may carry a
    /// path, so it is included deliberately and the caller decides.
    pub fn is_caller_mistake(&self) -> bool {
        matches!(self, DunkError::Message(_) | DunkError::AmbiguousTarget(_))
    }
}

impl fmt::Display for DunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DunkError::Message(text)
            | DunkError::AmbiguousTarget(text)
            | DunkError::Store(text)
            | DunkError::Refused(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for DunkError {}

impl From<std::io::Error> for DunkError {
    fn from(error: std::io::Error) -> Self {
        DunkError::Store(error.to_string())
    }
}

impl From<serde_json::Error> for DunkError {
    fn from(error: serde_json::Error) -> Self {
        DunkError::Store(format!("invalid JSON: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_the_display_text() {
        let error = DunkError::Message("bad id".into());
        assert_eq!("bad id", error.to_string());
    }

    #[test]
    fn an_ambiguous_target_is_a_caller_mistake_but_a_store_error_is_not() {
        assert!(DunkError::AmbiguousTarget("x".into()).is_caller_mistake());
        assert!(!DunkError::Store("cannot read /x".into()).is_caller_mistake());
    }

    #[test]
    fn a_refusal_carries_its_reason_code() {
        assert_eq!(Some("busy"), DunkError::Refused("busy".into()).reason());
        assert_eq!(None, DunkError::Message("x".into()).reason());
    }
}
