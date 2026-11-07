//! t3477: an idle composer that shows Claude's "new task? /clear to save N tokens" hint is a placeholder,
//! not a state. The hint must never block delivery (docs/DELIVERY.md: "a hint never blocks"), in any of the
//! layouts a Claude pane draws. There is no box capture from 2026-11-06, so each frame here is constructed
//! from the hint's grammar and the layouts the screen parser already handles.

use super::*;

const HINT: &str = "new task? /clear to save 497.4k tokens";
const FOOTER: &str = "  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n";

/// The idle frames a Claude pane draws with the hint showing. Each is an empty composer.
pub(crate) fn idle_hint_frames() -> Vec<(&'static str, String)> {
    vec![
        (
            "bordered, hint on the prompt row (dim)",
            format!(
                "\u{256d}{line}\u{256e}\n\u{2502} \u{276f} \x1b[2m{HINT}\x1b[22m \u{2502}\n\u{2570}{line}\u{256f}\n  ? for shortcuts\n",
                line = "\u{2500}".repeat(44)
            ),
        ),
        (
            "plain, prompt row then hint row (bright)",
            format!("\u{276f}\u{a0}\n{HINT}\n{FOOTER}"),
        ),
        (
            "same row, bright, under the footer",
            format!("\u{276f}\u{a0}{HINT}\n{FOOTER}"),
        ),
        (
            "grey RGB hint on the prompt row",
            format!(
                "\u{276f}\u{a0}\x1b[38;2;153;153;153mnew task? \x1b[0m\x1b[38;2;177;185;249m/clear\x1b[0m\x1b[38;2;153;153;153m to save 497.4k tokens\x1b[0m\n{FOOTER}"
            ),
        ),
        (
            "bordered, empty prompt row above the hint row",
            format!(
                "\u{256d}{line}\u{256e}\n\u{2502} \u{276f} \u{2502}\n\u{2502} {HINT} \u{2502}\n\u{2570}{line}\u{256f}\n",
                line = "\u{2500}".repeat(44)
            ),
        ),
    ]
}

#[test]
fn an_idle_hint_frame_is_empty_and_never_blocks_delivery() {
    for (name, screen) in idle_hint_frames() {
        let (text, detail) = box_text(&screen);
        assert!(
            text.is_empty() && !detail.starts_with(AMBIGUOUS_COMPOSER),
            "{name}: the hint is an empty box, not a draft or an unknown boundary: {text:?} {detail:?}"
        );
        let (state, reason) = classify_delivery(&screen, Some("idle"), None, None, false);
        assert_eq!(
            ("ready", None),
            (state, reason.clone()),
            "{name}: the hint never blocks delivery"
        );
    }
}
