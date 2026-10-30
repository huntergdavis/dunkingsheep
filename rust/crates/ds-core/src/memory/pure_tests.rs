//! `memory::pure` tests (T51 first slice): the three Python tests ported 1:1
//! (`test_chunking_overlap`, `test_redaction`'s string half, `test_fts_query_is_safe`)
//! plus the marker-by-marker redaction cases, because the patterns are the layer that
//! keeps a credential out of the store.

use super::*;

#[test]
fn chunking_overlap() {
    let text = "word ".repeat(4_000);
    let text = text.trim();
    let chunks = chunk_text(text, 1_000, 100);
    assert!(chunks.len() > 3, "got {} chunks", chunks.len());
    assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 1_000));
    let tail: String = chunks[0]
        .chars()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(
        chunks[1].contains(tail.trim()),
        "the overlap must carry the previous tail: {tail:?}"
    );
}

#[test]
fn an_empty_or_short_text_is_one_chunk_or_none() {
    assert!(chunk_text("", 100, 10).is_empty());
    assert!(chunk_text("   ", 100, 10).is_empty());
    assert_eq!(vec!["hello".to_string()], chunk_text("  hello  ", 100, 10));
}

#[test]
fn chunk_boundaries_fall_on_whitespace_not_mid_word() {
    let text = format!("{} end", "a".repeat(60));
    let chunks = chunk_text(&text, 40, 5);
    assert!(chunks.len() >= 2);
    for chunk in &chunks {
        assert!(chunk.chars().count() <= 40);
        assert!(!chunk.starts_with(' ') && !chunk.ends_with(' '));
    }
}

#[test]
fn redaction() {
    let cleaned = redact("key sk-live-abcdef0123456789abcdef and password=hunter2");
    assert!(
        !cleaned.contains("sk-live-abcdef0123456789abcdef"),
        "{cleaned}"
    );
    assert!(!cleaned.contains("hunter2"), "{cleaned}");
    assert!(cleaned.contains("[REDACTED"), "{cleaned}");
}

#[test]
fn every_secret_shape_gets_its_own_marker() {
    let cases = [
        ("sk-live-abcdef0123456789abcdef", "[REDACTED:api-key]"),
        (
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "[REDACTED:github-token]",
        ),
        (
            "github_pat_abcdefghijklmnopqrstuvwxyz",
            "[REDACTED:github-token]",
        ),
        ("xoxb-1234567890", "[REDACTED:slack-token]"),
        ("AKIAIOSFODNN7EXAMPLE", "[REDACTED:aws-key]"),
        (
            "AIzaSyA1234567890abcdefghijklmnopqrstu",
            "[REDACTED:google-key]",
        ),
    ];
    for (secret, marker) in cases {
        let cleaned = redact(&format!("before {secret} after"));
        assert!(!cleaned.contains(secret), "{secret} survived: {cleaned}");
        assert!(
            cleaned.contains(marker),
            "{secret} -> {cleaned} (wanted {marker})"
        );
    }
    // A private key block, where `.` has to match newlines (Python's re.S).
    let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----";
    let cleaned = redact(key);
    assert_eq!("[REDACTED:private-key]", cleaned);
    // Order matters and is the Python's: the bearer pattern (8th) fires BEFORE the
    // label pattern (9th). Label-first would leave the token behind
    // ("Authorization=[REDACTED] abcdefghijklmnop.qrstuvwx"), so never reorder them.
    assert_eq!(
        "Authorization=[REDACTED] [REDACTED:bearer]",
        redact("Authorization: bearer abcdefghijklmnop.qrstuvwx")
    );
}

#[test]
fn ordinary_text_is_untouched() {
    let text = "the parser reads a 12-byte record and returns PASS";
    assert_eq!(text, redact(text));
}

#[test]
fn fts_query_is_safe() {
    assert_eq!(None, fts_query("***", "AND"));
    assert!(fts_query("flux capacitor!", "AND")
        .expect("query")
        .contains("\"flux\""));
    assert_eq!(Some("\"flux\"".to_string()), fts_query("flux", "AND"));
    assert_eq!(
        Some("\"flux\" OR \"capacitor\"".to_string()),
        fts_query("flux capacitor", "OR")
    );
    // One-character tokens are dropped, so this query has no tokens at all -
    // confirmed against the Python, which returns None for the same input.
    assert_eq!(None, fts_query("a b", "AND"));
}

#[test]
fn tokens_are_words_of_two_or_more_and_at_most_sixteen() {
    assert_eq!(
        vec!["flux".to_string(), "cap".to_string()],
        fts_tokens("flux a cap!")
    );
    let many: String = (0..20).map(|n| format!("t{n} ")).collect();
    assert_eq!(16, fts_tokens(&many).len());
}

#[test]
fn a_snippet_windows_around_the_first_match() {
    let text = format!("{} needle {}", "x".repeat(500), "y".repeat(500));
    let snippet = make_snippet(&text, &["needle".to_string()], 240);
    assert!(snippet.contains("needle"), "{snippet}");
    assert_eq!(240, snippet.chars().count());
    // No match at all: the head of the text.
    let head = make_snippet("plain text", &["absent".to_string()], 5);
    assert_eq!("plain", head);
}

#[test]
fn a_snippet_window_is_in_characters_after_a_multibyte_prefix() {
    let tokens = vec!["needle".to_string()];
    for prefix in ["\u{3c0}", "\u{e9}"] {
        let text = format!("{} needle {}", prefix.repeat(300), "y".repeat(500));
        assert!(
            make_snippet(&text, &tokens, 240).contains("needle"),
            "{prefix}"
        );
        let upper = text.replace("needle", "NEEDLE");
        assert!(
            make_snippet(&upper, &tokens, 240).contains("NEEDLE"),
            "{prefix}"
        );
    }
}

#[test]
fn chunking_strips_the_pythons_whitespace_set() {
    // str.strip() removes U+001C..U+001F; str::trim does not.
    assert_eq!(
        vec!["abc".to_string()],
        chunk_text("\u{1c}abc\u{1c}", 100, 10)
    );
}
