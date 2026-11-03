//! Short confirmation rounds for an already-pressed guarded submission.
//! The caller performs the initial paste and key; each retry is guarded by SubmitEnv.

use super::sends::{
    box_holds, is_working_status, turn_started, StatusBaseline, SubmitEnv, SUBMIT_POLL_S,
};

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
            Some("claude".into())
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
}
