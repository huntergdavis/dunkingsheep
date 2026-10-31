//! `dunk_context.py`: the declarative context store (T50). Slice 1: the read side of
//! `Store` and the pure front-matter functions (slice 1); the Ledger, `log_access`,
//! `select_stores` and `digest_both` (slice 2). `add`/`supersede`/`retire`, `check`, the FTS
//! index and search, and `stats` are later slices.

pub mod digest;
pub mod ledger;
pub mod store;

/// `ContextError(ValueError)`: a named refusal for the context store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextError(pub String);

impl std::fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ContextError {}
