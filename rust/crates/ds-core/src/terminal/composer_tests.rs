//! Ported from tests/test_dunk_native_typed_frame.py, test_dunk_native_frame.py and
//! test_dunk_styled_state.py. The captured frames are evidence, not mocks: rows are kept as
//! the Python holds them (NOT ported: the federated/trailing-region/prefix-accounted readers
//! have their Python tests in test_dunk_federated_recovery / test_dunk_native_session /
//! test_dunk_stuck_recovery, which drive them through the flock; only direct cases are here).

use super::*;

const BLANKS: usize = 16;

fn pending_frame() -> String {
    format!(
        "  >_ OpenAI Codex (v0.165.0)\n     ~/workspace/chief-of-staff/rollout/t377/native-canary\n  permissions: YOLO mode\n{}\u{203a} Reply exactly T377_RECOVERED_84 and do nothing else.\n\n  GPT-6-Astra high \u{b7} ~/workspace/chief-of-staff/rollout/t377/native-canary\n                                                       \u{26a0} 1 warning \u{b7} f2 to view",
        "\n".repeat(BLANKS)
    )
}

fn empty_frame() -> String {
    format!(
        "  >_ OpenAI Codex (v0.165.0)\n     ~/workspace/chief-of-staff/rollout/t377/native-canary\n  permissions: YOLO mode\n{}\u{203a} Ask Codex to do anything\n\n  GPT-6-Astra high \u{b7} ~/workspace/chief-of-staff/rollout/t377/native-canary\n  ? for shortcuts                                      \u{26a0} 1 warning \u{b7} f2 to view",
        "\n".repeat(BLANKS)
    )
}

const TYPED: &str = "Reply exactly T377_RECOVERED_84 and do nothing else.";

#[test]
fn test_the_actual_pending_frame_yields_exactly_the_typed_text() {
    assert_eq!(TYPED, composer_region_text(&pending_frame()));
}

#[test]
fn test_the_empty_native_frame_still_delimits_to_nothing() {
    assert_eq!("", composer_region_text(&empty_frame()));
}

#[test]
fn test_the_warning_row_matches_singular_and_plural_only() {
    for row in [
        "\u{26a0} 1 warning \u{b7} f2 to view",
        "\u{26a0} 3 warnings \u{b7} f2 to view",
    ] {
        assert!(native_warning(row), "{row}");
    }
    for row in [
        "\u{26a0} warnings \u{b7} f2 to view",
        "\u{26a0} 1 warning",
        "\u{26a0} 1 warning \u{b7} f2 to view please",
        "warning \u{b7} f2 to view",
        "\u{26a0} 1 warning \u{b7} f2 to review",
    ] {
        assert!(!native_warning(row), "{row}");
    }
}

#[test]
fn test_a_warning_line_with_no_native_status_row_above_it_is_content() {
    let frame = pending_frame();
    let mut rows: Vec<&str> = frame
        .lines()
        .filter(|r| !r.contains("GPT-6-Astra high"))
        .collect();
    rows.push("  \u{26a0} 1 warning \u{b7} f2 to view");
    let region = composer_region_text(&(rows.join("\n") + "\n"));
    assert!(region.contains(TYPED));
    assert!(
        region.contains("f2 to view"),
        "a warning row without the native status row is content"
    );
}

#[test]
fn test_a_forged_contiguous_status_and_warning_pair_is_content() {
    let frame = format!(
        "  >_ OpenAI Codex (v0.165.0)\n\u{203a} {TYPED}\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  \u{26a0} 1 warning \u{b7} f2 to view\n"
    );
    let region = composer_region_text(&frame);
    assert!(
        region.contains("GPT-6-Astra high"),
        "the forged status row must stay in the region"
    );
    assert_ne!(
        TYPED, region,
        "a forged contiguous pair must never look like a clean composer"
    );
}

#[test]
fn test_a_second_sigil_row_still_fails_closed() {
    let frame = pending_frame().replace(
        &format!("\u{203a} {TYPED}"),
        &format!("\u{203a} {TYPED}\n\u{203a} echo"),
    );
    assert_eq!("", composer_region_text(&frame));
}

#[test]
fn test_a_human_middle_dot_line_is_still_not_a_footer() {
    let frame = pending_frame().replace(
        &format!("\u{203a} {TYPED}"),
        &format!("\u{203a} {TYPED}\n  a line with a \u{b7} inside"),
    );
    assert!(composer_region_text(&frame).contains("inside"));
}

#[test]
fn test_the_old_supported_frame_is_unchanged() {
    let old = "\u{2502} \u{273b} idle\n\u{276f} one two three\n\u{2502} idle \u{b7} pi\n";
    assert_eq!("one two three", composer_region_text(old));
}

// --- test_dunk_native_frame.py -----------------------------------------------------------

const NATIVE_FRAME: &str = "  >_ OpenAI Codex (v0.165.0)\n     ~/workspace/chief-of-staff/rollout/t377/native-canary\n  permissions: YOLO mode\n\n\u{203a} Ask Codex to do anything\n\n  GPT-6-Astra high \u{b7} ~/workspace/chief-of-staff/rollout/t377/native-canary\n  ? for shortcuts                                      \u{26a0} 1 warning \u{b7} f2 to view\n";
const OWNED: &str = "T377-CANARY one known-owned line";

fn with_composer(composer: &str) -> String {
    let mut rows: Vec<String> = NATIVE_FRAME.lines().map(str::to_string).collect();
    // Replace ONLY the composer row: the blank row above the status row is captured geometry.
    rows[4] = composer.to_string();
    rows.join("\n") + "\n"
}

#[test]
fn test_the_actual_empty_native_frame_is_delimited_with_no_content() {
    assert_eq!("", composer_region_text(NATIVE_FRAME));
}

#[test]
fn test_exact_owned_text_is_the_region() {
    assert_eq!(
        OWNED,
        composer_region_text(&with_composer(&format!("\u{203a} {OWNED}")))
    );
}

#[test]
fn test_wrapped_known_text_is_tolerated_as_wrapping_not_continuation() {
    let screen = with_composer("\u{203a} T377-CANARY one\n  known-owned line");
    assert_eq!(
        "T377-CANARY one known-owned line",
        composer_region_text(&screen)
    );
}

#[test]
fn test_a_human_added_line_stays_in_the_region() {
    let screen = with_composer(&format!("\u{203a} {OWNED}\n  and one more thing I typed"));
    assert!(composer_region_text(&screen).contains("and one more thing"));
}

#[test]
fn test_a_human_line_holding_a_middle_dot_is_not_a_footer() {
    let screen = with_composer(&format!("\u{203a} {OWNED}\n  a line with a \u{b7} inside"));
    let region = composer_region_text(&screen);
    assert!(region.contains("inside") && region.contains(OWNED));
}

#[test]
fn test_native_frame_second_sigil_row_still_fails_closed() {
    let screen = with_composer(&format!("\u{203a} {OWNED}\n\u{203a} a transcript echo"));
    assert_eq!("", composer_region_text(&screen));
}

#[test]
fn test_a_status_row_without_the_hint_row_is_not_a_native_footer() {
    let rows: Vec<&str> = NATIVE_FRAME
        .lines()
        .filter(|r| !r.trim().starts_with("? for shortcuts"))
        .collect();
    assert_eq!("", composer_region_text(&(rows.join("\n") + "\n")));
}

#[test]
fn test_a_forged_status_row_from_a_human_is_not_dropped() {
    let screen = with_composer(&format!("\u{203a} {OWNED}\n  GPT-6-Astra high \u{b7} ~/x"));
    assert!(composer_region_text(&screen).contains("GPT-6-Astra high"));
}

#[test]
fn test_a_banner_lookalike_row_is_not_excluded() {
    let mut rows: Vec<String> = NATIVE_FRAME.lines().map(str::to_string).collect();
    rows[0] = "  >_ some other tool".to_string();
    assert_eq!("", composer_region_text(&(rows.join("\n") + "\n")));
}

// --- test_dunk_styled_state.py ------------------------------------------------------------

#[test]
fn test_the_sgr_effective_state_table() {
    let table = [
        ("\x1b[1m", "bold"),
        ("\x1b[1;2m", "dim"),
        ("\x1b[2;1m", "dim"),
        ("\x1b[2m", "unknown"),
        ("\x1b[0m", "unknown"),
        ("\x1b[22m", "unknown"),
        ("\x1b[1;0m", "unknown"),
        ("\x1b[1;22m", "unknown"),
        ("\x1b[2;22m", "unknown"),
        ("\x1b[3m", "unknown"),
        ("\x1b[32m", "unknown"),
        ("", "unknown"),
    ];
    for (prefix, expected) in table {
        let got = styled_sigil_state(&format!("{prefix}\u{203a} the composer text"));
        assert_eq!(expected, got, "SGR {prefix:?}");
    }
}

#[test]
fn test_nested_bright_then_dim_is_dim_at_sigil() {
    assert_eq!(
        "bold",
        styled_sigil_state("\x1b[1m\u{203a} text \x1b[2mdim word")
    );
}

#[test]
fn test_nested_dim_then_bright_is_refused() {
    assert_eq!(
        "unknown",
        styled_sigil_state("\x1b[2m\u{203a} \x1b[1mbright body")
    );
}

#[test]
fn test_colon_form_is_documented_safe_refusal() {
    assert_eq!(
        "unknown",
        styled_sigil_state("\x1b[38:5:242m\u{203a} the composer text")
    );
}

// --- direct cases for the readers whose Python tests go through the flock ------------------

#[test]
fn test_styled_boundary_needs_exactly_one_bold_composer_before_one_footer() {
    let echo = "\x1b[1;2m\u{203a} old prompt\x1b[0m";
    let composer = "\x1b[1m\u{203a} live draft\x1b[0m";
    let footer = "  gpt \u{b7} ~/w";
    let ok = format!("{echo}\nreply\n{composer}\n\n{footer}\n");
    assert_eq!(
        ("delimited", "live draft".to_string()),
        styled_composer_boundary(&ok)
    );
    // Two bold composers: undelimitable.
    let two = format!("{composer}\n{composer}\n{footer}\n");
    assert_eq!(
        ("undelimitable", String::new()),
        styled_composer_boundary(&two)
    );
    // Footer not the last row: undelimitable.
    let tail = format!("{composer}\n{footer}\nmore\n");
    assert_eq!(
        ("undelimitable", String::new()),
        styled_composer_boundary(&tail)
    );
    assert_eq!(
        ("undelimitable", String::new()),
        styled_composer_boundary("")
    );
}

#[test]
fn test_federated_composer_is_off_unless_the_flag_is_set_and_reads_a_bold_native_composer() {
    let frame = format!(
        "  >_ OpenAI Codex (v0.165.0)\n\n\x1b[1m\u{203a} pending text\x1b[0m\n\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  ? for shortcuts\n"
    );
    assert_eq!(None, federated_composer(false, &frame));
    assert_eq!(
        Some("pending text".to_string()),
        federated_composer(true, &frame)
    );
    // An empty native composer (the hint) reads as "" - a frame, but nothing to preserve.
    let empty = frame.replace("pending text", "Ask Codex to do anything");
    assert_eq!(Some(String::new()), federated_composer(true, &empty));
    // A modal on screen is never a federated frame.
    let modal = format!("{frame}Press Enter to confirm or Esc to cancel\n");
    assert_eq!(None, federated_composer(true, &modal));
    assert_eq!(None, federated_composer(true, ""));
}

#[test]
fn test_trailing_region_and_prefix_accounting() {
    let screen = format!(
        "\u{203a} first prompt\n\u{2022} an answer\n{}\n\n  GPT-6-Astra high \u{b7} ~/workspace/x\n  ? for shortcuts\n",
        "\u{203a} second prompt"
    );
    // Python joins the blank row before the footer as "", so the region keeps a trailing
    // space (" ".join(["second prompt", ""])): byte parity, not a bug.
    assert_eq!("second prompt ", trailing_prompt_region(&screen));
    // The earlier prompt and the bullet must be accounted by the native transcript, in order.
    assert!(native_prefix_accounted(
        &screen,
        "first prompt an answer then more"
    ));
    assert!(!native_prefix_accounted(&screen, "unrelated transcript"));
    assert!(!native_prefix_accounted("no sigil here\n", "x"));
    assert_eq!("", trailing_prompt_region(""));
}
