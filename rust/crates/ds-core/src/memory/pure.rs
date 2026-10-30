//! `memory` (T51, first slice): the PURE core - redaction, chunking, FTS5 tokens
//! and queries, snippets.
//!
//! `dunk_memory.py` is 1,210 lines and 35 methods; the recall/ingest halves need the
//! golden corpus (`rust/fixtures/memory/corpus.json`, PR #190) as their judge and the
//! write convention (`db::SyncWriter`, #187). These five functions need neither, and
//! they are the part where a byte difference is a real defect: the redaction patterns
//! are the leak-prevention layer, and the chunk boundaries decide what bm25 ever sees.

use regex::Regex;
use std::sync::OnceLock;

/// `SECRET_PATTERNS`, VERBATIM from `dunk_memory.py`: (pattern, replacement), applied
/// in order. A test compares this list to the Python's own by running both over the
/// same inputs - the markers and the boundaries have to match exactly, because the
/// redaction is what keeps a credential out of the memory store.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    (
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        "[REDACTED:private-key]",
    ),
    (
        r"\b(sk|rk|pk|ak)-[A-Za-z0-9_\-]{16,}\b",
        "[REDACTED:api-key]",
    ),
    (r"\bgh[pousr]_[A-Za-z0-9]{20,}\b", "[REDACTED:github-token]"),
    (
        r"\bgithub_pat_[A-Za-z0-9_]{20,}\b",
        "[REDACTED:github-token]",
    ),
    (
        r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
        "[REDACTED:slack-token]",
    ),
    (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED:aws-key]"),
    (r"\bAIza[0-9A-Za-z_\-]{30,}\b", "[REDACTED:google-key]"),
    (
        r"(?i)\b(bearer)\s+[A-Za-z0-9._\-]{16,}",
        "${1} [REDACTED:bearer]",
    ),
    (
        r#"(?i)\b(api[_-]?key|secret|token|password|passwd|authorization)\b\s*[:=]\s*['"]?([^\s'"]{6,})"#,
        "${1}=[REDACTED]",
    ),
];

/// The compiled patterns, built once. `(?s)` makes `.` match newlines for the
/// private-key pattern, which the Python gets from `re.S`.
fn patterns() -> &'static Vec<(Regex, String)> {
    static COMPILED: OnceLock<Vec<(Regex, String)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .map(|(pattern, replacement)| {
                let pattern = if pattern.starts_with("-----BEGIN") {
                    format!("(?s){pattern}")
                } else {
                    (*pattern).to_string()
                };
                (
                    Regex::new(&pattern).expect("a SECRET_PATTERN must compile"),
                    (*replacement).to_string(),
                )
            })
            .collect()
    })
}

/// `redact`: replace credential-shaped substrings with a stable marker.
pub fn redact(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (pattern, replacement) in patterns() {
        // Rust's regex crate has no backreference-preserving replace for `${1}`
        // in the same shape as Python's, but it does expand `${name}` groups.
        out = pattern.replace_all(&out, replacement.as_str()).into_owned();
    }
    out
}

/// Default chunk size and overlap (`DEFAULT_CHUNK_CHARS` / `DEFAULT_OVERLAP_CHARS`).
pub const DEFAULT_CHUNK_CHARS: usize = 2_000;
pub const DEFAULT_OVERLAP_CHARS: usize = 200;

/// `chunk_text`: split into overlapping chunks at whitespace boundaries.
///
/// Character offsets, not bytes: the Python indexes `str`, and a multi-byte
/// character must not be split mid-sequence (that would be invalid UTF-8 in Rust
/// where Python would just carry the character).
pub fn chunk_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max_chars {
        return vec![trimmed.to_string()];
    }
    let length = chars.len();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < length {
        let mut end = std::cmp::min(length, start + max_chars);
        if end < length {
            // the last newline or space in [start, end), if it is past the halfway
            // point of the window
            let window = &chars[start..end];
            let boundary = window
                .iter()
                .enumerate()
                .filter(|(_, c)| **c == '\n' || **c == ' ')
                .map(|(index, _)| start + index)
                .max();
            if let Some(boundary) = boundary {
                if boundary > start + max_chars / 2 {
                    end = boundary;
                }
            }
        }
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim().to_string();
        if !piece.is_empty() {
            chunks.push(piece);
        }
        if end >= length {
            break;
        }
        start = std::cmp::max(end.saturating_sub(overlap_chars), start + 1);
    }
    chunks
}

/// `fts_tokens`: word tokens of two or more characters, at most sixteen.
pub fn fts_tokens(text: &str) -> Vec<String> {
    static WORD: OnceLock<Regex> = OnceLock::new();
    let word = WORD.get_or_init(|| Regex::new(r"[A-Za-z0-9_]+").expect("word regex"));
    word.find_iter(text)
        .map(|found| found.as_str().to_string())
        .filter(|token| token.len() >= 2)
        .take(16)
        .collect()
}

/// `fts_query`: a safe FTS5 query - quoted tokens joined by `operator`.
pub fn fts_query(text: &str, operator: &str) -> Option<String> {
    let tokens = fts_tokens(text);
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() == 1 {
        return Some(format!("\"{}\"", tokens[0]));
    }
    let joined: Vec<String> = tokens.iter().map(|token| format!("\"{token}\"")).collect();
    Some(joined.join(&format!(" {operator} ")))
}

/// `make_snippet`: a cheap window around the first matching token.
///
/// The Python does not use FTS5's `snippet()`, which scans every match; this picks
/// the earliest token position and takes a window that starts a third of the way in.
pub fn make_snippet(text: &str, tokens: &[String], max_chars: usize) -> String {
    let lowered = text.to_lowercase();
    let mut position: Option<usize> = None;
    for token in tokens {
        if let Some(found) = lowered.find(&token.to_lowercase()) {
            position = Some(match position {
                Some(current) => current.min(found),
                None => found,
            });
        }
    }
    let Some(position) = position else {
        return text.chars().take(max_chars).collect();
    };
    let start = position.saturating_sub(max_chars / 3);
    text.chars().skip(start).take(max_chars).collect()
}

#[cfg(test)]
#[path = "pure_tests.rs"]
mod pure_tests;
