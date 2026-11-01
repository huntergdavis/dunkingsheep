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

// --- slice 2: box_text / typed_text / is_our_send ---------------------------------------
// Ported from tests/test_dunk_codex_composer.py, test_dunk_codex_drafts_vs_echo.py,
// test_dunk_codex_idle_footer.py and test_composer_boundary.py. The fixtures are the REAL
// captured frames (tests/fixtures/*.ansi, copied byte-for-byte). NOT ported: the
// classify_delivery assertion of test_dunk_codex_composer (classify_delivery is a later
// slice) and the 39 other box_text call sites in test_dunk_delivery & co.

const IDLE_FOOTER: &str = include_str!("../../tests/fixtures/screen/codex-idle-footer-pane17.ansi");
const TRANSCRIPT_PLUS_HINT: &str =
    include_str!("../../tests/fixtures/screen/codex-pane17-transcript-plus-hint.ansi");
const BUSY_FRAME: &str = include_str!("../../tests/fixtures/screen/codex-busy-frame-pane17.ansi");
const WORKING_DRAFT3: &str =
    include_str!("../../tests/fixtures/screen/codex-working-human-draft3.ansi");
const IDLE_DRAFT3: &str = include_str!("../../tests/fixtures/screen/codex-idle-human-draft3.ansi");

#[test]
fn test_the_captured_screen_reads_as_an_empty_box() {
    let (text, detail) = box_text(TRANSCRIPT_PLUS_HINT);
    assert_eq!("", text, "the transcript was read as a draft: {detail}");
    assert!(
        !detail.is_empty(),
        "the reader must say why the box is empty"
    );
    assert_eq!(Some(String::new()), typed_text(TRANSCRIPT_PLUS_HINT));
}

#[test]
fn test_both_codex_hints_count_as_empty() {
    for hint in [
        "Ask Codex to do anything",
        "tab to queue message",
        "tab to send",
    ] {
        assert!(is_placeholder(&format!("  \x1b[2m{hint}\x1b[0m")), "{hint}");
    }
}

#[test]
fn test_a_bordered_draft_is_unaffected() {
    let screen = format!(
        "\u{2502} \u{276f} check your inbox                     \u{2502}\n{}",
        "\u{2500}".repeat(30)
    );
    assert_eq!("check your inbox", box_text(&screen).0);
}

#[test]
fn test_the_idle_echo_frame_is_empty() {
    let (text, detail) = box_text(IDLE_FOOTER);
    assert_eq!("", text, "{detail}");
    assert_eq!(Some(String::new()), typed_text(IDLE_FOOTER));
}

#[test]
fn test_the_transcript_plus_hint_frame_is_empty() {
    let (text, detail) = box_text(TRANSCRIPT_PLUS_HINT);
    assert_eq!("", text, "{detail}");
}

#[test]
fn test_the_busy_frame_is_empty() {
    let (text, detail) = box_text(BUSY_FRAME);
    assert_eq!("", text, "{detail}");
}

#[test]
fn test_a_working_lane_with_a_three_line_draft_is_a_draft() {
    let (text, detail) = box_text(WORKING_DRAFT3);
    assert_eq!("my human draft line one", text, "{detail}");
}

#[test]
fn test_an_idle_lane_with_a_three_line_draft_is_a_draft() {
    let (text, detail) = box_text(IDLE_DRAFT3);
    assert_eq!("this is a half typed human draft", text, "{detail}");
}

#[test]
fn test_a_draft_is_also_foreign_text_so_the_guard_holds() {
    assert!(typed_text(WORKING_DRAFT3).is_some_and(|t| !t.is_empty()));
}

#[test]
fn test_a_hint_directly_under_a_draft_still_belongs_to_that_draft() {
    let screen = "\u{203a} half a sentence\n  \x1b[2mtab to queue message\x1b[0m";
    let (text, detail) = box_text(screen);
    assert_eq!("half a sentence", text, "{detail}");
}

fn bordered_screen(composer: &str) -> String {
    let border = "\u{2500}".repeat(64);
    [
        "\u{276f} already answered acceptance test",
        "\u{25cf} I replied to the test.",
        "\u{273b} Worked for 7s",
        &border,
        composer,
        &border,
        "  bypass permissions on",
    ]
    .join("\n")
}

#[test]
fn test_submitted_prompt_above_empty_composer_is_not_a_draft() {
    assert_eq!("", box_text(&bordered_screen("\u{276f} ")).0);
    assert_eq!(
        Some(String::new()),
        typed_text(&bordered_screen("\u{276f} "))
    );
}

#[test]
fn test_actual_draft_wins_over_submitted_transcript() {
    assert_eq!(
        "unfinished owner draft",
        box_text(&bordered_screen("\u{276f} unfinished owner draft")).0
    );
}

#[test]
fn test_multiline_draft_survives_trailing_empty_prompt() {
    let draft = "\u{2502} \u{276f} make test\n\u{2502} \u{276f} -k delivery\n\u{276f} ";
    let (text, detail) = box_text(&bordered_screen(draft));
    assert_eq!("make test", text);
    assert!(detail.contains("prompt line(s) below it"), "{detail}");
}

#[test]
fn test_ansi_borders_delimit_the_composer() {
    let border = "\u{2500}".repeat(64);
    let styled = bordered_screen("\u{276f} ").replace(&border, &format!("\x1b[2m{border}\x1b[0m"));
    assert_eq!("", box_text(&styled).0);
}

#[test]
fn test_no_border_keeps_ambiguous_draft() {
    assert_eq!(
        "preserve me",
        box_text("\u{2502} \u{276f} preserve me\n\u{276f} \n").0
    );
}

#[test]
fn test_a_bottom_border_alone_does_not_erase_draft() {
    let screen = format!(
        "\u{2502} \u{276f} preserve me\n\u{276f} \n{}",
        "\u{2500}".repeat(64)
    );
    assert_eq!("preserve me", box_text(&screen).0);
}

#[test]
fn test_prompt_shaped_footer_is_outside_composer() {
    let screen = format!("{}\n\u{203a} footer label", bordered_screen("\u{276f} "));
    assert_eq!("", box_text(&screen).0);
}

#[test]
fn test_long_bordered_draft_keeps_its_first_line() {
    let draft = format!(
        "\u{276f} first line\n{}\n\u{276f} ",
        vec!["\u{2502} \u{276f} continuation"; 16].join("\n")
    );
    assert_eq!("first line", box_text(&bordered_screen(&draft)).0);
}

#[test]
fn test_unreadable_and_boxless_screens() {
    assert_eq!(
        ("".to_string(), "the pane could not be read".to_string()),
        box_text("")
    );
    // No input box at all (a bare shell): inconclusive, not empty.
    assert_eq!(
        None,
        typed_text("hunter@cell:~$ ls\nfoo bar\nhunter@cell:~$ \n")
    );
    assert_eq!(None, typed_text(""));
    assert!(box_text("hello\n").1.starts_with("no input box found"));
}

#[test]
fn test_a_draft_whose_text_begins_with_the_sigil_still_reads() {
    let screen = "❯ ❯ ls\n❯\u{00a0}\n─────────────────────────────────────────────\n";
    let (text, detail) = box_text(screen);
    assert_eq!("❯ ls", text);
    assert!(detail.contains("text starts on row 1"), "{detail}");
}

#[test]
fn test_an_empty_box_says_what_it_read() {
    let screen = "│ doing work\n❯\u{00a0}\n──────────\n";
    let (text, detail) = box_text(screen);
    assert_eq!("", text);
    assert!(detail.contains("nothing after the sigil"), "{detail}");
    assert!(detail.contains("row 2 of 3"), "{detail}");
}

#[test]
fn test_a_draft_above_an_empty_prompt_is_still_found() {
    let screen = concat!(
        "│ ❯ check your inbox\n",
        "│\n",
        "❯\u{00a0}\n",
        "─────────────────────────────────────────────\n",
        "  ⏵⏵ bypass permissions on · 1 shell\n",
    );
    let (text, detail) = box_text(screen);
    assert_eq!("check your inbox", text);
    assert!(detail.contains("text starts on row 1"), "{detail}");
}

#[test]
fn test_a_dim_hint_is_not_a_draft() {
    let screen = "❯\u{00a0}\n\x1b[2mnew task? /clear to save 1k tokens\x1b[0m\n";
    let (text, detail) = box_text(screen);
    assert_eq!("", text, "{detail}");
    assert!(!detail.is_empty(), "the empty read needs a diagnostic");
}

#[test]
fn test_the_reader_still_says_why_it_called_the_box_empty() {
    let screen = [
        "› VP keep-moving: continue t81",
        "",
        "› VP probe 4 (2.70.24): composer check",
        "",
        "› [inbox] new message for codex-poolrad from %19. Read it with: dunki",
        "",
        "",
        "  GPT-6-Astra high · ~/workspace/poolrad-codex-a · Implement t81 handoff",
        "",
    ]
    .join("\n");
    let (text, detail) = box_text(&screen);
    assert_eq!("", text);
    assert!(detail.contains("transcript"), "{detail}");
}

#[test]
fn test_the_reader_still_calls_that_frame_inconclusive() {
    let screen = [
        "› an earlier instruction",
        "",
        "› a nudge",
        "",
        "",
        "  GPT-6-Astra high · ~/workspace/poolrad-codex-a · Implement t81 handoff",
        "",
    ]
    .join("\n");
    let (text, detail) = box_text(&screen);
    assert_eq!("", text);
    assert!(detail.contains("transcript"), "{detail}");
}

#[test]
fn test_is_our_send_recognises_our_prefix_and_collapsed_pastes() {
    assert!(is_our_send(
        "check your inbox",
        Some("Check your inbox now please"),
        false
    ));
    assert!(!is_our_send(
        "a human draft",
        Some("check your inbox"),
        false
    ));
    assert!(!is_our_send("", None, false));
    assert!(!is_our_send("[Pasted text #1 +75 lines]", None, false));
    assert!(is_our_send("[Pasted text #1 +75 lines]", None, true));
}

// --- slice 3: classify_delivery ----------------------------------------------------------
// Ported from tests/test_dunk_delivery.py (ClassifyDeliveryTests, PlaceholderTests),
// test_dunk_codex_composer.py (the classify assertion) and test_dunk_inbox.py's
// WORKING_SCREEN case.

const LIVE: &str = "\u{2502} \u{25cf} wrote the phrase \"server connection could not be restored\" into a test file\n\u{2502} \u{2500}\u{2500} Working \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{2502} ~/workspace/dunkingsheep (main)\n";
const DEAD: &str = "\u{2502} \u{25a0} Server connection could not be restored\n\u{2502} \u{2022} Reconnect failed - check the endpoint, then relaunch (41h 22m)\n\u{2502} \u{203a} [inbox] new message\n\u{2502}   ^c quit\n";
const PAUSED: &str = "\u{2502} \u{273b} Reading files\n\u{2502} GPT-6-Astra high \u{b7} ~/workspace/crackdown-switch \u{b7} Port Crackdown   Goal paused (/goal resume)\n";
const CONTEXT_FULL: &str =
    "\u{2502} \u{273b} Working\n\u{2502} Context limit reached. Run /clear to continue.\n";
const HINT_GREY_RGB: &str = "\x1b[38;2;153;153;153mnew task? \x1b[0m\x1b[38;2;177;185;249m/clear\x1b[0m\x1b[38;2;153;153;153m to save 497.4k tokens\x1b[0m";
const HINT_DIM: &str = "\x1b[2mnew task? /clear to save 497.4k tokens\x1b[0m";

fn classify(screen: &str, status: &str) -> (&'static str, Option<String>) {
    classify_delivery(screen, Some(status), None, None, false)
}

#[test]
fn test_a_live_pane_that_quotes_the_marker_is_not_dead() {
    assert_eq!(("ready", None), classify(LIVE, "idle"));
}

#[test]
fn test_a_banner_line_is_a_dead_session() {
    let (state, reason) = classify(DEAD, "idle");
    assert_eq!("dead", state);
    assert!(reason
        .unwrap()
        .contains("server connection could not be restored"));
}

#[test]
fn test_escape_sequences_do_not_hide_a_banner() {
    let (state, _) = classify(
        "\x1b[31m\u{25a0} Server connection could not be restored\x1b[0m\n",
        "idle",
    );
    assert_eq!(
        "dead", state,
        "a colour code before the glyph hid the banner"
    );
}

#[test]
fn test_a_paused_goal_blocks_delivery_and_says_so() {
    let (state, reason) = classify(PAUSED, "idle");
    assert_eq!("blocked", state);
    assert!(reason.unwrap().contains("paused goal"));
}

#[test]
fn test_a_full_context_blocks_delivery_and_names_the_fix() {
    let (state, reason) = classify(CONTEXT_FULL, "idle");
    assert_eq!("blocked", state);
    assert!(reason.unwrap().contains("/clear"));
}

#[test]
fn test_a_draft_in_the_box_no_longer_blocks_delivery() {
    // Policy 2026-10-24: the text is preserved at send time, not refused.
    let (state, reason) = classify_delivery(
        "\u{2502} \u{203a} half a sentence",
        Some("idle"),
        Some("half a sentence"),
        None,
        false,
    );
    assert_eq!("ready", state);
    assert_eq!(None, reason);
}

#[test]
fn test_working_is_not_ready_and_not_blocked() {
    assert_eq!(
        ("working", Some("working".to_string())),
        classify("\u{2502} \u{273b} Working", "working")
    );
}

#[test]
fn test_the_hint_is_not_a_state_however_it_is_drawn() {
    for (label, screen) in [("grey rgb", HINT_GREY_RGB), ("dim", HINT_DIM)] {
        assert_eq!(
            ("ready", None),
            classify(&format!("\u{276f}\u{a0}\n{screen}\n"), "idle"),
            "{label}"
        );
    }
}

#[test]
fn test_the_hint_does_not_mask_a_real_draft() {
    let screen = format!("\u{276f} half a sentence\n{HINT_GREY_RGB}\n");
    assert_eq!("half a sentence", box_text(&screen).0);
    let (state, _) = classify_delivery(&screen, Some("idle"), Some("half a sentence"), None, false);
    assert_eq!("ready", state, "a draft is preserved, not a refusal");
}

#[test]
fn test_a_real_full_context_banner_still_blocks() {
    let (state, reason) = classify(
        "\u{2502} Context limit reached. Run /clear to continue\n",
        "idle",
    );
    assert_eq!("blocked", state);
    assert!(reason.unwrap().contains("/clear"));
}

#[test]
fn test_the_captured_screen_is_deliverable_and_not_foreign() {
    let typed = typed_text(TRANSCRIPT_PLUS_HINT);
    assert_eq!(Some(String::new()), typed);
    assert_eq!(
        ("ready", None),
        classify_delivery(
            TRANSCRIPT_PLUS_HINT,
            Some("idle"),
            typed.as_deref(),
            None,
            false
        )
    );
}

#[test]
fn test_the_working_status_is_information_not_a_refusal() {
    let screen = "\u{2502} \u{273b} Working (55m) \u{b7} esc to interrupt\n\u{276f}\u{a0}\n\x1b[2mAsk Pi to do anything\x1b[0m\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n";
    let (state, reason) = classify(screen, "working");
    assert_eq!("working", state);
    assert!(!matches!(reason.as_deref(), Some("blocked") | Some("dead")));
}
