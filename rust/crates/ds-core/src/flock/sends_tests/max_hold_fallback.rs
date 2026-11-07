//! t3477 max-hold fallback (owner-approved, i48518). After a send has been held for the hold limit,
//! an INCONCLUSIVE composer read (`ambiguous-composer`) may be typed into. A genuinely pending
//! foreign draft stays preserved and held, and so does a dialog. The production rule is
//! `fallback_overrides_hold` in flock/sends.rs.
//!
//! A child of `sends_tests`, so the parent's `Pane`, `P`, `submit_pane`, `submit_pending_screen`
//! and `submit_modal_screen` are shared.

use super::*;
use crate::flock::sends::{fallback_overrides_hold, BOX_HOLD};
use crate::terminal::screen::{box_text, AMBIGUOUS_COMPOSER};

/// A styled, inconclusive composer: a dim echo row above a bright draft row, with no provenance.
fn ambiguous_frame() -> String {
    let frame = "\x1b[1;2m\u{203a}\x1b[0m old prompt\n\x1b[1m\u{203a}\x1b[0m live draft\n\n\n  gpt \u{b7} ~/w\n";
    let (_, detail) = box_text(frame);
    assert!(
        detail.starts_with(AMBIGUOUS_COMPOSER),
        "fixture must be an inconclusive read, got {detail:?}"
    );
    frame.to_string()
}

fn flock() -> Flock {
    Flock::new(
        "/tmp/ds-sends-test-launches.json",
        "/tmp/ds-sends-test-lanes.json",
        false,
    )
}

fn options(preserve_draft: bool, inconclusive_fallback: bool) -> SendGuardOptions {
    SendGuardOptions {
        hold_while_typing: true,
        allow_modal: false,
        preserve_draft,
        inconclusive_fallback,
    }
}

/// Runs one guarded send against a fixed screen. Returns whether the action ran, and the result.
fn guarded_send(options: SendGuardOptions, screen: String) -> (bool, (bool, Option<bool>, String)) {
    let flock = flock();
    let mut pane = submit_pane("idle");
    pane.screen_script.borrow_mut().clear();
    pane.screens.insert(P.into(), screen);
    let mut action_ran = false;
    let result = send_guarded(
        &flock,
        &mut pane,
        P,
        options,
        || None,
        |_, _| {
            action_ran = true;
            Ok(SendAction::completed(true, "typed after max-hold"))
        },
    );
    (action_ran, result)
}

#[test]
fn an_inconclusive_box_is_held_before_the_max_hold() {
    let (ran, (sent, ok, detail)) = guarded_send(options(false, false), ambiguous_frame());

    assert!(!ran, "the hold must stand before the max-hold fallback");
    assert!(!sent);
    assert_eq!(None, ok);
    assert!(detail.starts_with(BOX_HOLD), "{detail}");
    assert!(detail.ends_with("(ambiguous-composer)"), "{detail}");
}

#[test]
fn the_max_hold_fallback_types_into_an_inconclusive_box() {
    let (ran, (_, _, detail)) = guarded_send(options(false, true), ambiguous_frame());

    assert!(
        ran,
        "after the max-hold an inconclusive read must be typed into: {detail}"
    );
    assert!(!detail.starts_with(BOX_HOLD), "{detail}");
}

#[test]
fn the_max_hold_fallback_still_types_into_an_inconclusive_box_when_drafts_are_preserved() {
    // The ambiguous read is inconclusive, not a draft we can name, so preserve_draft does not hold it.
    let (ran, (_, _, detail)) = guarded_send(options(true, true), ambiguous_frame());

    assert!(
        ran,
        "inconclusive read must type after the max-hold: {detail}"
    );
}

#[test]
fn the_max_hold_fallback_never_types_over_a_pending_foreign_draft() {
    let (ran, (sent, _, detail)) =
        guarded_send(options(true, true), submit_pending_screen("human draft"));

    assert!(
        !ran,
        "a pending foreign draft stays preserved after the max-hold: {detail}"
    );
    assert!(!sent);
    assert!(detail.starts_with(BOX_HOLD), "{detail}");
    assert!(detail.ends_with("(human_draft)"), "{detail}");
}

#[test]
fn the_max_hold_fallback_does_not_override_a_dialog() {
    let (ran, (_, _, detail)) = guarded_send(options(false, true), submit_modal_screen());

    assert!(!ran, "a dialog is not an inconclusive read: {detail}");
    assert!(detail.ends_with("(dialog)"), "{detail}");
}

#[test]
fn the_fallback_overrides_only_the_inconclusive_reason() {
    // Every refusal `ensure_box_clear` can return, other than the inconclusive one, stays a hold.
    let holds = [
        "dialog",
        "human_draft",
        "federated-pending-input",
        "stranded-will-not-submit",
        "partial-model-command",
        "foreign-text-will-not-submit",
    ];
    assert!(fallback_overrides_hold("ambiguous-composer", true));
    assert!(!fallback_overrides_hold("ambiguous-composer", false));
    for reason in holds {
        assert!(
            !fallback_overrides_hold(reason, true),
            "{reason} must stay a hold with the fallback on"
        );
        assert!(
            !fallback_overrides_hold(reason, false),
            "{reason} must stay a hold with the fallback off"
        );
    }
}
