//! Ported from tests/test_dunk_core.py (test_bright_text_ignores_truecolor_subparameters,
//! DialogScopeTests live/answered), tests/test_dunk_delivery.py (test_the_hint_grammar),
//! tests/test_model_command_suffix.py (old picker footer) and the docstring incidents of
//! `_banner_marker` / `_status_marker`. The fixtures are tests/fakes.py's, with ONE
//! simplification: the answered-dialog frame ends in a plain idle box rather than the full
//! `_codex_frame_ansi` styling (the reader strips ANSI first, so the verdict is the same).

use super::*;

fn approval_dialog() -> String {
    [
        "  \u{2514} Run the command: inspect pc \u{2026}",
        "  \u{2514}   1. Yes, proceed (y)",
        "  \u{2514}   2. No, and tell Codex what to do differently (esc)",
        "",
        "      Press Enter to confirm or Esc to cancel",
        "",
        "  gpt-6-sol high \u{b7} ~/workspace",
    ]
    .join("\n")
        + "\n"
}

fn answered_dialog_in_scrollback() -> String {
    let mut lines = vec![
        "  \u{2514} Run the command: inspect pc \u{2026}".to_string(),
        "  \u{2514}   1. Yes, proceed (y)".to_string(),
        "  \u{2514}   2. No, and tell Codex what to do differently (esc)".to_string(),
        "      Press Enter to confirm or Esc to cancel".to_string(),
    ];
    lines.extend((1..=20).map(|n| format!("  \u{2514} output line {n}")));
    lines.join("\n") + "\n\u{203a} Ask Codex to do anything\n  gpt-6-sol high \u{b7} ~/workspace\n"
}

#[test]
fn test_bright_text_ignores_truecolor_subparameters() {
    // "38;2;r;g;b" carries a literal 2 that must not read as dim.
    let line = "\x1b[38;2;255;255;255mhello\x1b[0m \x1b[2mdim\x1b[22m bright";
    assert_eq!("hello  bright", bright_text(line));
}

#[test]
fn test_a_live_dialog_is_recognised() {
    assert!(modal_open(&approval_dialog()));
}

#[test]
fn test_an_answered_dialog_in_the_scrollback_is_not() {
    let screen = answered_dialog_in_scrollback();
    assert!(
        screen.contains("Press Enter to confirm"),
        "the history is still there"
    );
    assert!(
        !modal_open(&screen),
        "only the bottom of the screen is the live interface"
    );
}

#[test]
fn test_old_picker_footer_above_current_composer_is_transcript() {
    let screen = "Select Model and Effort\nenter select \u{b7} esc back\n\u{203a} new human draft\nGPT-6-Astra high \u{b7} ~/work\n";
    assert!(!modal_open(screen));
    // ...but the same footer as the LIVE bottom row is a picker (t405).
    assert!(modal_open(
        "Select Model and Effort\n  enter   select \u{b7} esc back\n"
    ));
    assert!(modal_open(
        "x\nenter default \u{b7} s session \u{b7} esc back"
    ));
    assert!(!modal_open(""));
}

#[test]
fn test_the_hint_grammar() {
    assert!(is_placeholder("new task? /clear to save 497.4k tokens"));
    assert!(is_placeholder("  new task? /clear to save 1.2M tokens  "));
    assert!(!is_placeholder("Context limit reached"));
    assert!(is_placeholder("type your request"));
    // The hint is drawn INSIDE the composer: border and sigil come off first.
    assert!(is_placeholder("\u{2502} \u{203a} Ask Codex to do anything"));
    assert!(is_placeholder("\x1b[1mtab\x1b[0m to queue message"));
}

#[test]
fn test_a_banner_must_start_its_own_line() {
    let dead = "\x1b[31m\u{25a0} Server connection could not be restored\x1b[0m\n\u{276f} \n";
    assert_eq!(
        banner_marker(dead, &DEAD_SESSION_MARKERS).as_deref(),
        Some("server connection could not be restored")
    );
    // A live pane that merely wrote those words inside a sentence is not dead.
    let prose = "I wrote: the server connection could not be restored, then retried\n";
    assert_eq!(banner_marker(prose, &DEAD_SESSION_MARKERS), None);
    assert_eq!(banner_marker("", &DEAD_SESSION_MARKERS), None);
}

#[test]
fn test_a_dim_hint_is_a_placeholder_not_a_state() {
    // Claude draws "new task? /clear to save ..." dim; reading it as 'context full' made a
    // healthy lane permanently unreachable.
    let dim = "\x1b[2mnew task? /clear to save 497.4k tokens\x1b[22m\n";
    assert_eq!(
        status_marker(dim, &CONTEXT_FULL_MARKERS, 3, true, false),
        None
    );
    // The same words in BRIGHT text are caught unless the placeholder grammar claims them.
    let bright = "new task? /clear to save 497.4k tokens\n";
    assert_eq!(
        status_marker(bright, &CONTEXT_FULL_MARKERS, 3, false, false).as_deref(),
        Some("/clear to save")
    );
    assert_eq!(
        status_marker(bright, &CONTEXT_FULL_MARKERS, 3, true, false),
        None
    );
    // dim_ok sees codex's grey "Working" indicator.
    let working = "\x1b[2m\u{2022} Working (55m \u{b7} esc to interrupt)\x1b[0m\n";
    assert_eq!(
        status_marker(working, &WORKING_MARKERS, 3, true, false),
        None
    );
    assert_eq!(
        status_marker(working, &WORKING_MARKERS, 3, true, true).as_deref(),
        Some("esc to interrupt")
    );
}

#[test]
fn test_only_the_last_non_blank_rows_count_for_a_status() {
    let mut screen = String::from("goal paused (/goal resume)\n");
    screen.push_str("a\n\nb\n\nc\n\nd\n");
    assert_eq!(
        status_marker(&screen, &PAUSED_GOAL_MARKERS, 3, false, false),
        None
    );
    assert_eq!(
        status_marker(&screen, &PAUSED_GOAL_MARKERS, 5, false, false).as_deref(),
        Some("goal paused")
    );
}

#[test]
fn test_delivery_rows_and_tail_and_splitlines_match_python() {
    assert_eq!(
        delivery_rows("\x1b[31m a \x1b[0m  \n\n   \nb\n"),
        vec![" a", "b"]
    );
    assert_eq!(delivery_tail("A\n\nB\nC\n", 2), "b\nc");
    // Python's splitlines also breaks on \r, \x0c and  , and does not emit a final empty row.
    assert_eq!(
        splitlines("a\rb\r\nc\x0cd\u{2028}e\n"),
        vec!["a", "b", "c", "d", "e"]
    );
    assert_eq!(splitlines(""), Vec::<&str>::new());
    assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
    assert_eq!(collapse("  a \n b\t c "), "a b c");
}
