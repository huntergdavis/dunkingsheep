//! Tests for `sends.rs`, ported by name from tests/test_busy_native_queue.py,
//! tests/test_codex_ambiguous_composer.py (the `_box_holds` / `send_blocker` halves) and
//! tests/test_dunk_core.py (the `_clear_to_send` tests). The scripted pane below is the
//! `FakeHerdr` surface those tests drive. NOT ported (later slices): everything that goes through
//! `submit_text`/`send_guarded`/`flush_box`/`delivery_state`, and the thread-timing assertions of
//! the clear-to-send tests (the settle wait is injected, so wall-clock is not asserted).

use super::sends::*;
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Default)]
struct Pane {
    screens: BTreeMap<String, String>,
    harness: BTreeMap<String, String>,
    statuses: BTreeMap<String, String>,
    gone: bool,
    federated: bool,
    keys: Vec<(String, String)>,
    reads: RefCell<Vec<(String, bool)>>,
}

impl SendEnv for Pane {
    fn read_pane(&self, pane_id: &str, _lines: i64, ansi: bool) -> Option<String> {
        self.reads.borrow_mut().push((pane_id.to_string(), ansi));
        self.screens.get(pane_id).cloned()
    }
    fn pane_harness(&self, pane_id: &str) -> Option<String> {
        self.harness.get(pane_id).cloned()
    }
    fn session_process_gone(&self, _: &str) -> bool {
        self.gone
    }
    fn read_status(&self, pane_id: &str) -> Option<String> {
        self.statuses.get(pane_id).cloned()
    }
    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.keys.push((pane_id.to_string(), key.to_string()));
        (true, "sent".to_string())
    }
    fn federated_input(&self) -> bool {
        self.federated
    }
}

const P: &str = "w2:p1";

fn busy_codex() -> Pane {
    let mut pane = Pane::default();
    pane.harness.insert(P.into(), "codex".into());
    pane.statuses.insert(P.into(), "working".into());
    pane.screens.insert(
        P.into(),
        "\u{2022} Working (1m \u{b7} esc to interrupt)\n\u{203a} Ask Codex to do anything\n".into(),
    );
    pane
}

// --- test_busy_native_queue.py ---------------------------------------------------------------

#[test]
fn test_codex_busy_is_deliverable() {
    assert_eq!(None, send_blocker(&busy_codex(), P));
}

#[test]
fn test_codex_uses_tab_and_recomputes_after_turn_finishes() {
    let mut pane = busy_codex();
    assert_eq!(
        (true, "sent".to_string(), Some("Tab".to_string())),
        send_submit_key(&mut pane, P, "Enter", false)
    );
    pane.statuses.insert(P.into(), "idle".into());
    pane.screens
        .insert(P.into(), "\u{203a} Ask Codex to do anything\n".into());
    // A learned Tab must never leak into a later idle send: the fallback here is what the
    // Python's `_submit_key[pane]` held, and the recomputed key is Enter.
    assert_eq!(
        Some("Enter".to_string()),
        send_submit_key(&mut pane, P, "Tab", false).2
    );
}

#[test]
fn test_busy_claude_and_pi_keep_enter() {
    for harness in ["claude", "pi"] {
        let mut pane = busy_codex();
        pane.harness.insert(P.into(), harness.into());
        assert_eq!(None, send_blocker(&pane, P));
        assert_eq!(
            Some("Enter".to_string()),
            send_submit_key(&mut pane, P, "Enter", false).2
        );
    }
}

#[test]
fn test_dialog_opening_before_retry_gets_no_key() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "Would you like to run the following command?\n\u{276f} 1. Yes\n  2. No\n\n      Press Enter to confirm or Esc to cancel\n".into(),
    );
    let (ok, detail, key) = send_submit_key(&mut pane, P, "Enter", false);
    assert!(!ok);
    assert!(detail.contains("dialog"));
    assert_eq!(None, key);
    assert!(pane.keys.is_empty());
}

#[test]
fn test_long_busy_composer_with_unknown_backend_status_queues() {
    let mut pane = busy_codex();
    pane.statuses.remove(P);
    pane.screens.insert(
        P.into(),
        format!(
            "\u{2022} Working (1m \u{b7} esc to interrupt)\n\u{203a} message\n{}  GPT-6-Astra high \u{b7} ~/project\n",
            "  continuation\n".repeat(12)
        ),
    );
    assert_eq!(
        Some("Tab".to_string()),
        send_submit_key(&mut pane, P, "Enter", false).2
    );
}

#[test]
fn test_an_unreadable_pane_gets_no_key() {
    let mut pane = Pane::default();
    let (ok, detail, key) = send_submit_key(&mut pane, P, "Enter", false);
    assert!(!ok && key.is_none());
    assert_eq!("submission unconfirmed: pane unreadable before key", detail);
}

// --- send_blocker (policy halves of test_dunk_delivery / test_dunk_inbox) --------------------

#[test]
fn test_a_dead_session_refuses_and_names_the_marker() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "\u{25a0} Server connection could not be restored\n".into(),
    );
    let (state, reason) = send_blocker(&pane, P).unwrap();
    assert_eq!("dead", state);
    assert!(reason.contains("server connection could not be restored"));
}

#[test]
fn test_a_gone_process_refuses_even_when_the_screen_looks_alive() {
    let mut pane = busy_codex();
    pane.gone = true;
    assert_eq!(
        Some(("dead", "the lane's recorded process is gone".to_string())),
        send_blocker(&pane, P)
    );
}

#[test]
fn test_an_unknown_harness_keeps_the_capability_hold_while_working() {
    let mut pane = busy_codex();
    pane.harness.insert(P.into(), "mystery".into());
    let (state, reason) = send_blocker(&pane, P).unwrap();
    assert_eq!("blocked", state);
    assert!(reason.starts_with("mystery does not queue text typed during a turn"));
    pane.harness.remove(P);
    assert!(send_blocker(&pane, P)
        .unwrap()
        .1
        .starts_with("this harness does not queue"));
}

#[test]
fn test_a_modal_is_not_a_hold() {
    let mut pane = busy_codex();
    pane.screens.insert(
        P.into(),
        "  \u{2514}   1. Yes, proceed (y)\n\n      Press Enter to confirm or Esc to cancel\n"
            .into(),
    );
    assert_eq!(None, send_blocker(&pane, P));
}

// --- test_codex_ambiguous_composer.py (the _box_holds / send_blocker halves) -------------------

const NOTICE: &str =
    "[inbox] new message for fixture from reviewer. Read it with: dunkingsheep i\u{2026}";
const FOOTER: &str = "  model \u{b7} /tmp/synthetic-fixture \u{b7} task";

fn frame(body: &str, continuation: &[&str]) -> String {
    let mut rows: Vec<String> = vec!["synthetic transcript".into()];
    rows.extend(std::iter::repeat(String::new()).take(17));
    rows.push(format!("\u{203a} {body}"));
    rows.extend(continuation.iter().map(|c| c.to_string()));
    rows.push(String::new());
    rows.push(String::new());
    rows.push(FOOTER.to_string());
    rows.join("\n") + "\n"
}

fn with_screen(screen: String) -> Pane {
    let mut pane = Pane::default();
    pane.harness.insert(P.into(), "codex".into());
    pane.screens.insert(P.into(), screen);
    pane
}

fn check_ambiguous(screen: String) {
    let pane = with_screen(screen.clone());
    let (_text, detail) = crate::terminal::screen::box_text(&screen);
    assert!(detail.contains("ambiguous composer"), "{detail}");
    assert!(!detail.contains("box is empty"), "{detail}");
    assert_eq!(None, box_holds(&pane, P, NOTICE));
    let (state, _reason) = send_blocker(&pane, P).expect("an ambiguous composer must block");
    assert_eq!("unknown", state);
}

#[test]
fn test_ellipsised_notice_is_ambiguous_not_empty() {
    check_ambiguous(frame(NOTICE, &[]));
}

#[test]
fn test_wrapped_human_draft_is_preserved() {
    check_ambiguous(frame(
        "my unfinished draft",
        &["  second line", "  third line"],
    ));
}

#[test]
fn test_notice_prefix_with_appended_human_text_stays_unknown() {
    check_ambiguous(frame(NOTICE, &["  do not submit this human addition"]));
}

#[test]
fn test_placeholder_with_footer_shaped_human_continuation_is_unknown() {
    check_ambiguous(frame(
        "\x1b[2mAsk Codex to do anything\x1b[0m",
        &["  human draft \u{b7} still unfinished"],
    ));
}

#[test]
fn test_whitespace_and_ansi_only_rows_do_not_prove_empty() {
    let screen = frame(NOTICE, &[]).replace(
        &format!("\n\n\n{FOOTER}"),
        &format!("\n \x1b[0m\n\t\n{FOOTER}"),
    );
    check_ambiguous(screen);
}

#[test]
fn test_empty_placeholder_is_still_explicitly_empty() {
    let pane = with_screen(frame("\x1b[2mAsk Codex to do anything\x1b[0m", &[]));
    assert_eq!(Some(false), box_holds(&pane, P, NOTICE));
    assert_eq!(None, send_blocker(&pane, P));
}

#[test]
fn test_box_holds_sees_our_own_text_and_a_federated_composer() {
    // Our text still in the box (claude-style bordered composer): held.
    let screen = "\u{2502} \u{276f} check your inbox now\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n".to_string();
    let pane = with_screen(screen);
    assert_eq!(Some(true), box_holds(&pane, P, "check your inbox now"));
    // A human draft is NOT ours.
    assert_eq!(Some(false), box_holds(&pane, P, "something else entirely"));
    // Unreadable pane: unknown.
    assert_eq!(None, box_holds(&Pane::default(), P, "x"));
    // A modal open: our text left the box and opened it.
    let modal = with_screen("      Press Enter to confirm or Esc to cancel\n".into());
    assert_eq!(Some(false), box_holds(&modal, P, "x"));
}

// --- test_dunk_core.py: _clear_to_send ---------------------------------------------------------

fn hint_screen() -> String {
    "\u{276f}\u{a0}\n\x1b[2mnew task? /clear to save 497.4k tokens\x1b[0m\n".to_string()
}

#[test]
fn test_two_clear_checks_a_second_apart_allow_the_send() {
    let pane = with_screen(hint_screen());
    let waited = RefCell::new(Vec::new());
    let wait = |s: f64| {
        waited.borrow_mut().push(s);
        false
    };
    assert!(clear_to_send(&pane, P, &wait));
    assert_eq!(
        vec![TYPING_SETTLE_S],
        *waited.borrow(),
        "one settle wait of TYPING_SETTLE_S"
    );
    // send_blocker reads the box once (ansi) and the dead check once (plain); the redraw check
    // then reads the styled screen again, and the dead check once more.
    let ansi = pane.reads.borrow().iter().filter(|(_, ansi)| *ansi).count();
    assert_eq!(2, ansi, "two styled reads, a settle apart");
}

#[test]
fn test_text_in_the_box_no_longer_stops_a_send() {
    let pane = with_screen("\u{276f} mid sentence\n".to_string());
    assert!(clear_to_send(&pane, P, &|_| false));
}

#[test]
fn test_a_stopped_dunk_does_not_finish_its_settle_wait() {
    let pane = with_screen(hint_screen());
    assert!(!clear_to_send(&pane, P, &|_| true));
}

#[test]
fn test_a_dead_session_is_not_clear_to_send_and_a_redraw_into_one_is_caught() {
    let pane = with_screen("\u{25a0} Reconnect failed\n".to_string());
    assert!(!clear_to_send(&pane, P, &|_| false));
    // A screen that cannot be read on the second look is inconclusive, not a hold.
    struct Flaky(RefCell<u32>, Pane);
    impl SendEnv for Flaky {
        fn read_pane(&self, p: &str, l: i64, a: bool) -> Option<String> {
            *self.0.borrow_mut() += 1;
            // reads 1 (dead check) and 2 (box) succeed, 3+ (second look) do not
            if *self.0.borrow() <= 2 {
                self.1.read_pane(p, l, a)
            } else {
                None
            }
        }
        fn pane_harness(&self, p: &str) -> Option<String> {
            self.1.pane_harness(p)
        }
        fn session_process_gone(&self, p: &str) -> bool {
            self.1.session_process_gone(p)
        }
        fn read_status(&self, p: &str) -> Option<String> {
            self.1.read_status(p)
        }
        fn send_key(&mut self, p: &str, k: &str) -> (bool, String) {
            self.1.send_key(p, k)
        }
        fn federated_input(&self) -> bool {
            false
        }
    }
    let flaky = Flaky(RefCell::new(0), with_screen(hint_screen()));
    assert!(clear_to_send(&flaky, P, &|_| false));
}

// --- Message ----------------------------------------------------------------------------------

#[test]
fn test_message_outcomes_and_wire_form() {
    let mut message = Message::new("m1", "w2:p1", "Sheep", "hello", "send_text");
    assert_eq!("queued", message.outcome());
    let queued = message.to_dict_at(message.created_at + 7.9);
    assert_eq!(serde_json::json!(true), queued["queued"]);
    assert_eq!(serde_json::Value::Null, queued["ok"]);
    assert_eq!(serde_json::json!(7), queued["waiting_s"]);
    assert_eq!(
        serde_json::json!("queued for Sheep, delivering once the input box is clear"),
        queued["message"]
    );
    message.holds = 2;
    assert_eq!(
        "queued: the box in Sheep would not clear",
        message.queued_message()
    );
    message.status = "sent".into();
    assert_eq!("ok", message.outcome());
    let sent = message.to_dict();
    assert_eq!(serde_json::json!(true), sent["ok"]);
    assert_eq!(serde_json::json!("sent"), sent["message"]);
    assert_eq!(serde_json::Value::Null, sent["waiting_s"]);
    message.status = "failed".into();
    message.detail = Some("submission unconfirmed: pane unreadable".into());
    assert_eq!("unconfirmed", message.outcome());
    message.detail = Some("agent session is dead (x)".into());
    assert_eq!("refused", message.outcome());
    message.detail = Some("the Enter did not submit".into());
    assert_eq!("unconfirmed", message.outcome());
    for outcome in ["ok", "queued", "unconfirmed", "refused"] {
        assert!(OUTCOMES.contains(&outcome));
    }
}
