//! Short confirmation rounds for an already-pressed guarded submission.
//! The caller performs the initial paste and key; each retry is guarded by SubmitEnv.

use super::sends::{
    box_holds, is_working_status, read_box, turn_started, StatusBaseline, SubmitEnv, SUBMIT_POLL_S,
};

/// t4779: a text at least this long, typed into a Codex pane as a keystroke burst, is buffered
/// by Codex before it reaches the composer: the box keeps showing its empty hint for a second
/// or more and only then shows the text, usually collapsed to `[Pasted Content N chars]`.
/// Shorter texts (inbox notices among them) are not affected and take no extra wait.
pub const BURST_SETTLE_MIN_CHARS: usize = 500;
/// How many times, and how far apart, the box is read again after it first read empty.
pub const BURST_SETTLE_READS: usize = 4;
pub const BURST_SETTLE_GAP_S: f64 = 1.0;

/// The `N` of a collapsed paste shown as `[Pasted Content N chars]`, if `shown` is one.
pub(crate) fn collapsed_paste_chars(shown: &str) -> Option<usize> {
    let low = shown.to_lowercase();
    let rest = low.split_once("[pasted content ")?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !rest[digits.len()..].trim_start().starts_with("char") {
        return None;
    }
    digits.parse().ok()
}

/// t4779: an EMPTY box right after typing a long text into a Codex pane is not yet proof that
/// it was submitted. Read the box again for a bounded time. `None` keeps the caller's verdict
/// ("sent"): the box stayed empty, a turn started, the pane could not be read, or this send is
/// not a long text to a Codex pane. `Some` replaces it:
///
/// * the box now shows a collapsed paste whose character count is OURS (the text's length, or
///   one more for our own Enter kept as a newline): press the submit key ONCE more, and report
///   sent only if the box then empties or a turn starts, failed otherwise;
/// * the box shows a collapsed paste with any other count: it cannot be proven to be this
///   send, so nothing is pressed and the send is reported failed.
///
/// Any other text that appears is left alone and does not change the verdict: it is not ours
/// to submit, and it is not evidence that our send failed.
pub(crate) fn settle_collapsed_paste(
    env: &mut dyn SubmitEnv,
    pane_id: &str,
    text: &str,
    pre_status: Option<&str>,
    busy_hint: bool,
) -> Option<(bool, String)> {
    let chars = text.chars().count();
    if chars < BURST_SETTLE_MIN_CHARS || env.pane_harness(pane_id).as_deref() != Some("codex") {
        return None;
    }
    for _ in 0..BURST_SETTLE_READS {
        if turn_started(env, pane_id, pre_status) {
            return None;
        }
        env.sleep_s(BURST_SETTLE_GAP_S);
        let (screen, shown, _detail) = read_box(env, pane_id);
        screen.as_ref()?;
        let Some(count) = collapsed_paste_chars(&shown) else {
            continue;
        };
        if count != chars && count != chars + 1 {
            eprintln!(
                "{pane_id}: a collapsed paste of {count} characters is in the input box after a {chars}-character send; it is not provably this send, so nothing is pressed"
            );
            return Some((
                false,
                format!(
                    "submission unconfirmed: the pane's input box holds a collapsed paste of {count} characters that does not match this {chars}-character send; nothing was pressed"
                ),
            ));
        }
        eprintln!(
            "{pane_id}: this {chars}-character send is still in the input box as a collapsed paste; pressing the submit key once more"
        );
        let (ok, detail, _key) = env.send_submit_key_guarded(pane_id, "Enter", busy_hint);
        if !ok {
            return Some((false, detail));
        }
        for wait in SUBMIT_POLL_S {
            env.sleep_s(wait);
            if box_holds(env, pane_id, text) == Some(false) {
                return Some((
                    true,
                    "sent (a collapsed paste needed one more Enter)".into(),
                ));
            }
            if turn_started(env, pane_id, pre_status) {
                return Some((true, "sent (confirmed by the turn it started)".into()));
            }
        }
        return Some((
            false,
            "text did not submit; it is still in the pane's input box as a collapsed paste".into(),
        ));
    }
    None
}

pub const GUARDED_PRESS_ATTEMPTS: usize = 3;
pub const GUARDED_VERIFY_GAPS: [f64; 2] = [0.2, 0.4];

/// Confirm an initial guarded key without holding the typing guard during observations.
pub fn confirm_guarded_submission(
    env: &mut dyn SubmitEnv,
    pane_id: &str,
    text: &str,
    _first_key: &str,
    busy_hint: bool,
    baseline: StatusBaseline,
) -> (bool, String) {
    let pre_status = match baseline {
        StatusBaseline::ReadNow => env.read_status(pane_id),
        StatusBaseline::Captured(status) => status,
    };
    for attempt in 0..GUARDED_PRESS_ATTEMPTS {
        env.sleep_s(SUBMIT_POLL_S[attempt.min(SUBMIT_POLL_S.len() - 1)]);
        let mut held = box_holds(env, pane_id, text);
        if held == Some(false) {
            for gap in GUARDED_VERIFY_GAPS {
                env.sleep_s(gap);
                held = box_holds(env, pane_id, text);
                if held != Some(false) {
                    break;
                }
            }
        }
        if held == Some(false) {
            if let Some(verdict) =
                settle_collapsed_paste(env, pane_id, text, pre_status.as_deref(), busy_hint)
            {
                return verdict;
            }
            return (
                true,
                if attempt == 0 {
                    "sent".into()
                } else {
                    format!("sent (confirmed after {} checks)", attempt + 1)
                },
            );
        }
        if turn_started(env, pane_id, pre_status.as_deref()) {
            return (true, "sent (confirmed by the turn it started)".into());
        }
        let fallback = env
            .remembered_submit_key(pane_id)
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| {
                if attempt == 0 {
                    "Enter".into()
                } else {
                    "C-m".into()
                }
            });
        let (ok, detail, _actual_key) = env.send_submit_key_guarded(pane_id, &fallback, busy_hint);
        if !ok {
            return (false, detail);
        }
    }
    if is_working_status(pre_status.as_deref()) {
        return (false, "submission unconfirmed: the lane was already working before this send; status alone cannot confirm a new message - check the pane".into());
    }
    (
        false,
        "text did not submit; it is still in the pane's input box".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::sends::{send_submit_key, SendEnv};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    const P: &str = "w1:p1";
    const TEXT: &str = "hello";
    fn pending() -> String {
        format!("│ ❯ {TEXT}\n────────\n")
    }
    fn clear() -> String {
        "❯\u{a0}\n\x1b[2mnew task? /clear to save 497.4k tokens\x1b[0m\n".into()
    }
    fn modal() -> String {
        "Press Enter to confirm or Esc to cancel\n".into()
    }

    #[derive(Default)]
    struct Pane {
        screens: RefCell<VecDeque<Option<String>>>,
        status: Option<String>,
        /// The pane's harness; `None` is the fixture's default, claude.
        harness: Option<String>,
        remembered: Option<String>,
        sleeps: Vec<f64>,
        keys: Vec<String>,
        fallbacks: Vec<(String, bool)>,
        locked: bool,
    }
    impl SendEnv for Pane {
        fn read_pane(&self, _: &str, _: i64, _: bool) -> Option<String> {
            self.screens
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Some(pending()))
        }
        fn pane_harness(&self, _: &str) -> Option<String> {
            Some(self.harness.clone().unwrap_or_else(|| "claude".into()))
        }
        fn session_process_gone(&self, _: &str) -> bool {
            false
        }
        fn read_status(&self, _: &str) -> Option<String> {
            self.status.clone()
        }
        fn send_key(&mut self, _: &str, key: &str) -> (bool, String) {
            assert!(self.locked);
            self.keys.push(key.into());
            (true, "sent".into())
        }
        fn federated_input(&self) -> bool {
            false
        }
    }
    impl SubmitEnv for Pane {
        fn sleep_s(&mut self, seconds: f64) {
            assert!(!self.locked);
            self.sleeps.push(seconds);
        }
        fn now_s(&mut self) -> f64 {
            panic!("short path has no deadline")
        }
        fn send_submit_key_guarded(
            &mut self,
            pane: &str,
            fallback: &str,
            busy: bool,
        ) -> (bool, String, Option<String>) {
            assert!(!self.locked);
            self.fallbacks.push((fallback.into(), busy));
            self.locked = true;
            let result = send_submit_key(self, pane, fallback, busy);
            self.locked = false;
            result
        }
        fn remembered_submit_key(&self, _: &str) -> Option<String> {
            self.remembered.clone()
        }
        fn remember_submit_key(&mut self, _: &str, _: &str) {
            panic!("short path does not remember")
        }
        fn record_submit_attempt(&mut self, _: &str, _: &str, _: usize, _: usize) {
            panic!("short path has no ladder metric")
        }
    }
    fn run(p: &mut Pane, baseline: Option<&str>) -> (bool, String) {
        confirm_guarded_submission(
            p,
            P,
            TEXT,
            "Tab",
            true,
            StatusBaseline::Captured(baseline.map(str::to_string)),
        )
    }
    fn script(p: &Pane, screens: impl IntoIterator<Item = Option<String>>) {
        p.screens.borrow_mut().extend(screens);
    }

    #[test]
    fn short_rounds_match_python_poll_and_verify_gaps() {
        let mut p = Pane::default();
        script(
            &p,
            [
                Some(pending()),
                Some(pending()),
                Some(pending()),
                Some(pending()),
                Some(clear()),
                Some(clear()),
                Some(clear()),
            ],
        );
        assert_eq!(
            run(&mut p, Some("idle")),
            (true, "sent (confirmed after 3 checks)".into())
        );
        assert_eq!(p.sleeps, [0.1, 0.2, 0.4, 0.2, 0.4]);
        assert_eq!(p.fallbacks, [("Enter".into(), true), ("C-m".into(), true)]);
        assert_eq!(p.keys, ["Enter", "C-m"]);
    }
    #[test]
    fn clear_box_requires_both_verify_reads() {
        let mut p = Pane::default();
        script(
            &p,
            [
                Some(clear()),
                Some(clear()),
                Some(pending()),
                Some(pending()),
                Some(modal()),
                Some(modal()),
                Some(modal()),
            ],
        );
        assert_eq!(
            run(&mut p, Some("idle")),
            (true, "sent (confirmed after 2 checks)".into())
        );
        assert_eq!(p.keys.len(), 1);
        assert_eq!(p.sleeps, [0.1, 0.2, 0.4, 0.2, 0.2, 0.4]);
    }
    #[test]
    fn working_transition_requires_readable_idle_baseline() {
        for baseline in [None, Some("working")] {
            let mut p = Pane {
                status: Some("working".into()),
                ..Default::default()
            };
            assert!(!run(&mut p, baseline).0);
            assert_eq!(p.keys.len(), 3);
        }
        let mut p = Pane {
            status: Some("working".into()),
            ..Default::default()
        };
        assert_eq!(
            run(&mut p, Some("idle")),
            (true, "sent (confirmed by the turn it started)".into())
        );
        assert!(p.keys.is_empty());
    }
    #[test]
    fn unknown_box_retries_without_false_confirmation() {
        let mut p = Pane::default();
        script(
            &p,
            [
                None,
                Some(pending()),
                None,
                Some(pending()),
                None,
                Some(pending()),
            ],
        );
        assert!(!run(&mut p, None).0);
        assert_eq!(p.keys.len(), 3);
        assert_eq!(p.sleeps, [0.1, 0.2, 0.4]);
    }
    #[test]
    fn retry_key_uses_existing_guarded_submit_and_stops_on_refusal() {
        let mut p = Pane {
            remembered: Some("C-m".into()),
            ..Default::default()
        };
        script(&p, [Some(pending()), Some(modal())]);
        let (ok, detail) = run(&mut p, Some("idle"));
        assert!(!ok);
        assert_eq!(detail, "submission held: a dialog opened before the key");
        assert_eq!(p.fallbacks, [("C-m".into(), true)]);
        assert!(p.keys.is_empty());
    }
    #[test]
    fn exhaustion_distinguishes_stale_working_from_idle_pending() {
        let mut p = Pane::default();
        assert_eq!(
            run(&mut p, Some("idle")).1,
            "text did not submit; it is still in the pane's input box"
        );
        let mut p = Pane::default();
        assert_eq!(run(&mut p, Some("busy")).1, "submission unconfirmed: the lane was already working before this send; status alone cannot confirm a new message - check the pane");
    }

    // ---- t4779: a long text typed into a Codex pane; the box reads empty, then a collapsed paste ----

    /// The Codex composer as captured in a private pty: prompt row, one blank row, footer.
    fn codex(prompt_row: &str) -> String {
        format!("synthetic transcript\n\n{prompt_row}\n\n  GPT-6.1 Terra low \u{b7} ~/work\n")
    }
    fn codex_hint() -> Option<String> {
        Some(codex("\u{203a} \x1b[2mAsk Codex to do anything\x1b[0m"))
    }
    fn codex_paste(chars: usize) -> Option<String> {
        Some(codex(&format!(
            "\x1b[1m\u{203a}\x1b[22m \x1b[38;5;6m[Pasted Content {chars} chars]\x1b[0m"
        )))
    }
    fn long_text() -> String {
        "inspect every coordinator. ".repeat(30)[..600].to_string()
    }
    fn codex_pane() -> Pane {
        Pane {
            harness: Some("codex".into()),
            ..Pane::default()
        }
    }
    fn run_text(p: &mut Pane, text: &str) -> (bool, String) {
        confirm_guarded_submission(p, P, text, "Enter", false, StatusBaseline::Captured(None))
    }

    #[test]
    fn the_collapsed_paste_count_is_read_from_the_placeholder() {
        assert_eq!(
            Some(1379),
            collapsed_paste_chars("[Pasted Content 1379 chars]")
        );
        assert_eq!(
            Some(7),
            collapsed_paste_chars("x [pasted content 7 chars] y")
        );
        assert_eq!(None, collapsed_paste_chars("[Pasted text #1 +75 lines]"));
        assert_eq!(None, collapsed_paste_chars("[Pasted Content chars]"));
        assert_eq!(None, collapsed_paste_chars("[Pasted Content 12 lines]"));
        assert_eq!(None, collapsed_paste_chars("a draft about pasted content"));
    }

    #[test]
    fn the_fixture_screens_read_as_the_reader_reads_real_codex() {
        // The premise of the tests below: the hint is an empty box, the placeholder is text.
        let p = codex_pane();
        script(&p, [codex_hint(), codex_paste(601)]);
        assert_eq!(Some(false), box_holds(&p, P, &long_text()));
        assert_eq!(Some(true), box_holds(&p, P, &long_text()));
    }

    #[test]
    fn an_empty_box_then_our_collapsed_paste_gets_one_more_enter_and_is_then_confirmed() {
        let text = long_text();
        let mut p = codex_pane();
        // Empty while Codex buffers the burst (three ladder reads, one settle read), then the
        // collapsed paste with our own Enter kept as a newline (N + 1), then empty once it went.
        script(
            &p,
            [codex_hint(), codex_hint(), codex_hint(), codex_hint()]
                .into_iter()
                .chain([codex_paste(601), codex_paste(601)])
                .chain(std::iter::repeat_with(codex_hint).take(8)),
        );
        let (ok, detail) = run_text(&mut p, &text);
        assert!(ok, "{detail}");
        assert_eq!("sent (a collapsed paste needed one more Enter)", detail);
        assert_eq!(vec!["Enter".to_string()], p.keys, "exactly one more Enter");
    }

    #[test]
    fn a_collapsed_paste_that_stays_is_reported_failed_after_one_enter_never_sent() {
        let text = long_text();
        let mut p = codex_pane();
        script(
            &p,
            std::iter::repeat_with(codex_hint)
                .take(4)
                .chain(std::iter::repeat_with(|| codex_paste(600)).take(16)),
        );
        let (ok, detail) = run_text(&mut p, &text);
        assert!(!ok, "{detail}");
        assert_eq!(
            "text did not submit; it is still in the pane's input box as a collapsed paste",
            detail
        );
        assert_eq!(
            vec!["Enter".to_string()],
            p.keys,
            "one Enter, not a retry loop"
        );
    }

    #[test]
    fn a_collapsed_paste_that_is_not_provably_ours_is_left_alone_and_reported_failed() {
        let text = long_text();
        let mut p = codex_pane();
        // 250 characters is neither our 600 nor 601: a person's paste, or somebody else's.
        script(
            &p,
            std::iter::repeat_with(codex_hint)
                .take(4)
                .chain(std::iter::repeat_with(|| codex_paste(250)).take(8)),
        );
        let (ok, detail) = run_text(&mut p, &text);
        assert!(!ok, "{detail}");
        assert!(
            detail.contains("does not match this 600-character send"),
            "{detail}"
        );
        assert!(
            p.keys.is_empty(),
            "nothing is pressed on text that is not ours"
        );
    }

    #[test]
    fn a_long_send_whose_box_stays_empty_is_sent_after_the_bounded_settle_reads() {
        let text = long_text();
        let mut p = codex_pane();
        script(&p, std::iter::repeat_with(codex_hint).take(12));
        let (ok, detail) = run_text(&mut p, &text);
        assert!(ok, "{detail}");
        assert_eq!("sent", detail);
        assert!(p.keys.is_empty());
        // The ladder's own three reads, then exactly the bounded settle reads.
        assert_eq!(
            vec![0.1, 0.2, 0.4, 1.0, 1.0, 1.0, 1.0],
            p.sleeps,
            "four settle reads one second apart, and no more"
        );
    }

    #[test]
    fn short_texts_and_other_harnesses_take_no_extra_wait() {
        // A short text to a Codex pane (an inbox notice, for example).
        let mut p = codex_pane();
        script(&p, std::iter::repeat_with(codex_hint).take(6));
        let (ok, detail) = run_text(&mut p, "a one-line notice");
        assert!(ok && detail == "sent", "{detail}");
        assert_eq!(vec![0.1, 0.2, 0.4], p.sleeps);

        // A long text to a harness that does not buffer a burst this way.
        let mut p = Pane::default();
        script(&p, std::iter::repeat_with(|| Some(clear())).take(6));
        let (ok, detail) = run_text(&mut p, &long_text());
        assert!(ok && detail == "sent", "{detail}");
        assert_eq!(vec![0.1, 0.2, 0.4], p.sleeps);
    }
}
