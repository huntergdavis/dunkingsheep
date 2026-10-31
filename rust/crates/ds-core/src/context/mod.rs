//! `dunk_context.py`: the declarative context store (T50). Slice 1: the read side of
//! `Store` and the pure front-matter functions. `add`/`supersede`/`retire`, `check`, the
//! FTS index, the access log, `digest_both`, and the ledger are later slices.

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
