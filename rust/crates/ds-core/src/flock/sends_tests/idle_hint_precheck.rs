//! t3661: a typed nudge is held by the outbox's pre-send check (outbox_step -> clear_to_send ->
//! send_blocker) before send_guarded runs. box_text and classify_delivery are not that check. These
//! tests run the constructed idle hint frames (terminal/hint_placeholder_tests.rs) through the
//! pre-check, and test the t3477 max-hold fallback on the same path, which the merged change did not
//! reach: it lived only in send_guarded.
//!
//! A claude pane is never held by a working marker (harness_queues_while_busy), so for claude the only
//! pre-send hold is an inconclusive box read. That is the read the fallback has to clear.

use super::*;
use crate::flock::sends::{clear_to_send_with_fallback, send_blocker_with_fallback};
use crate::terminal::screen::hint_placeholder_tests::idle_hint_frames;
use crate::terminal::screen::{box_text, AMBIGUOUS_COMPOSER};

/// A styled, inconclusive composer: a dim echo row above a bright draft row, with no provenance.
const INCONCLUSIVE: &str =
    "\x1b[1;2m\u{203a}\x1b[0m old prompt\n\x1b[1m\u{203a}\x1b[0m live draft\n\n\n  gpt \u{b7} ~/w\n";

fn pane_showing(screen: &str) -> Pane {
    let mut pane = submit_pane("idle");
    pane.screen_script.borrow_mut().clear();
    pane.screens.insert(P.into(), screen.to_string());
    pane
}

fn inconclusive_pane() -> Pane {
    let (_, detail) = box_text(INCONCLUSIVE);
    assert!(
        detail.starts_with(AMBIGUOUS_COMPOSER),
        "fixture must be an inconclusive read, got {detail:?}"
    );
    pane_showing(INCONCLUSIVE)
}

fn blocker_kind(pane: &Pane, inconclusive_fallback: bool) -> Option<&'static str> {
    send_blocker_with_fallback(pane, P, inconclusive_fallback).map(|(kind, _)| kind)
}

#[test]
fn an_idle_hint_frame_does_not_hold_the_outbox_precheck() {
    for (name, screen) in idle_hint_frames() {
        let pane = pane_showing(&screen);
        assert_eq!(
            None,
            send_blocker(&pane, P),
            "{name}: the pre-send check holds an idle hint frame"
        );
        assert!(
            clear_to_send(&pane, P, &|_| false),
            "{name}: clear_to_send refuses an idle hint frame"
        );
    }
}

#[test]
fn an_inconclusive_box_holds_the_precheck_until_the_max_hold() {
    let pane = inconclusive_pane();
    assert_eq!(Some("unknown"), blocker_kind(&pane, false));
    assert!(!clear_to_send_with_fallback(&pane, P, &|_| false, false));
}

#[test]
fn the_max_hold_fallback_clears_the_precheck_for_an_inconclusive_box() {
    let pane = inconclusive_pane();
    assert_eq!(None, blocker_kind(&pane, true));
    assert!(clear_to_send_with_fallback(&pane, P, &|_| false, true));
}

#[test]
fn the_max_hold_fallback_still_blocks_a_working_pane() {
    // A harness that does not queue while busy holds on a working indicator. The fallback clears
    // only an inconclusive read, so the working hold stands even after the max-hold.
    let screen = format!("{INCONCLUSIVE}  esc to interrupt\n");
    let (_, detail) = box_text(&screen);
    assert!(
        detail.starts_with(AMBIGUOUS_COMPOSER),
        "fixture must stay an inconclusive read, got {detail:?}"
    );
    let mut pane = pane_showing(&screen);
    pane.harness.insert(P.into(), "unknown-harness".into());
    assert_eq!(Some("blocked"), blocker_kind(&pane, true));
    assert!(!clear_to_send_with_fallback(&pane, P, &|_| false, true));
}
