//! `memory`: the session-memory store, ported from `dunk_memory.py` (T51).
//!
//! First slice: [`pure`] - redaction, chunking, FTS5 tokens/queries and snippets.
//! The recall and ingest halves are judged by `rust/fixtures/memory/corpus.json`
//! (PR #190) and need the write convention (`db::SyncWriter`, #187), so they are
//! not here yet.

pub mod pure;
pub mod recall;

pub use pure::{
    chunk_text, fts_query, fts_tokens, make_snippet, redact, DEFAULT_CHUNK_CHARS,
    DEFAULT_OVERLAP_CHARS,
};
