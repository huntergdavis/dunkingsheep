//! T33 key routing (t1526): ports only `Flock._press_keys_unlocked` for an already-normalized
//! key sequence. This boundary deliberately has no production adapter, typing lock, or submit
//! retry loop; those belong to later submission-ladder work. A scripted callback test here does
//! not establish that a real terminal submission succeeds after retrying Enter.

/// Operations needed to route keys and inspect/confirm text already in a pane.
pub trait KeyPressEnv {
    /// Send one key batch, returning the backend's result and detail unchanged.
    fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String);
    /// Read the text currently waiting in the pane's composer, if available.
    fn pending_text(&mut self, pane_id: &str) -> Option<String>;
    /// Confirm submission of the exact pending text.
    fn confirm_pending(&mut self, pane_id: &str, text: &str) -> (bool, String);
}

/// Route an already-normalized key sequence, matching `Flock._press_keys_unlocked`.
pub fn press_keys_unlocked(
    env: &mut dyn KeyPressEnv,
    pane_id: &str,
    keys: &[String],
) -> (bool, String) {
    let Some(last) = keys.last() else {
        return env.send_keys(pane_id, keys);
    };
    if !matches!(
        last.as_str(),
        "Enter" | "enter" | "Return" | "return" | "KP_Enter"
    ) {
        return env.send_keys(pane_id, keys);
    }

    if keys.len() > 1 {
        let result = env.send_keys(pane_id, &keys[..keys.len() - 1]);
        if !result.0 {
            return result;
        }
    }

    let Some(pending) = env.pending_text(pane_id).filter(|text| !text.is_empty()) else {
        return env.send_keys(pane_id, std::slice::from_ref(last));
    };
    env.confirm_pending(pane_id, &pending)
}

#[cfg(test)]
mod tests {
    use super::{press_keys_unlocked, KeyPressEnv};

    #[derive(Debug, PartialEq)]
    enum Call {
        Send(String, Vec<String>),
        Pending(String),
        Confirm(String, String),
    }

    struct ScriptedEnv {
        calls: Vec<Call>,
        send_results: Vec<(bool, String)>,
        pending: Option<String>,
        confirm_result: (bool, String),
    }

    impl ScriptedEnv {
        fn new(pending: Option<&str>) -> Self {
            Self {
                calls: Vec::new(),
                send_results: Vec::new(),
                pending: pending.map(str::to_owned),
                confirm_result: (true, "confirmed detail".into()),
            }
        }

        fn result(mut self, result: (bool, &str)) -> Self {
            self.send_results.push((result.0, result.1.into()));
            self
        }

        fn confirm_result(mut self, result: (bool, &str)) -> Self {
            self.confirm_result = (result.0, result.1.into());
            self
        }
    }

    impl KeyPressEnv for ScriptedEnv {
        fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String) {
            self.calls.push(Call::Send(pane_id.into(), keys.to_vec()));
            if self.send_results.is_empty() {
                (true, "sent detail".into())
            } else {
                self.send_results.remove(0)
            }
        }

        fn pending_text(&mut self, pane_id: &str) -> Option<String> {
            self.calls.push(Call::Pending(pane_id.into()));
            self.pending.clone()
        }

        fn confirm_pending(&mut self, pane_id: &str, text: &str) -> (bool, String) {
            self.calls.push(Call::Confirm(pane_id.into(), text.into()));
            self.confirm_result.clone()
        }
    }

    fn keys(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn test_keys_that_are_not_enter_are_sent_untouched() {
        let input = keys(&["Escape", "C-x"]);
        let mut env = ScriptedEnv::new(Some("draft"));
        let result = press_keys_unlocked(&mut env, "pane-1", &input);
        assert_eq!(result, (true, "sent detail".into()));
        assert_eq!(env.calls, vec![Call::Send("pane-1".into(), input)]);
    }

    #[test]
    fn empty_sequence_is_sent_once_without_reading_pending_text() {
        let mut env = ScriptedEnv::new(Some("draft"));
        let result = press_keys_unlocked(&mut env, "pane-1", &[]);
        assert_eq!(result, (true, "sent detail".into()));
        assert_eq!(env.calls, vec![Call::Send("pane-1".into(), vec![])]);
    }

    #[test]
    fn test_a_bare_enter_on_an_empty_box_is_just_a_key_press() {
        for spelling in ["Enter", "enter", "Return", "return", "KP_Enter"] {
            let mut env = ScriptedEnv::new(Some(""));
            let result = press_keys_unlocked(&mut env, "pane-1", &keys(&[spelling]));
            assert_eq!(result, (true, "sent detail".into()), "{spelling}");
            assert_eq!(
                env.calls,
                vec![
                    Call::Pending("pane-1".into()),
                    Call::Send("pane-1".into(), keys(&[spelling])),
                ],
                "{spelling}"
            );
        }
    }

    #[test]
    fn enter_prefix_is_one_batch_before_pending_text_check() {
        let mut env = ScriptedEnv::new(Some("message"));
        let result = press_keys_unlocked(&mut env, "pane-1", &keys(&["a", "b", "Enter"]));
        assert_eq!(result, (true, "confirmed detail".into()));
        assert_eq!(
            env.calls,
            vec![
                Call::Send("pane-1".into(), keys(&["a", "b"])),
                Call::Pending("pane-1".into()),
                Call::Confirm("pane-1".into(), "message".into()),
            ]
        );
    }

    #[test]
    fn failed_prefix_returns_unchanged_without_read_or_confirmation() {
        let mut env = ScriptedEnv::new(Some("message")).result((false, "prefix rejected"));
        let result = press_keys_unlocked(&mut env, "pane-1", &keys(&["a", "Enter"]));
        assert_eq!(result, (false, "prefix rejected".into()));
        assert_eq!(env.calls, vec![Call::Send("pane-1".into(), keys(&["a"]))]);
    }

    #[test]
    fn unavailable_pending_text_sends_only_original_enter() {
        let mut env = ScriptedEnv::new(None);
        let result = press_keys_unlocked(&mut env, "pane-1", &keys(&["Return"]));
        assert_eq!(result, (true, "sent detail".into()));
        assert_eq!(
            env.calls,
            vec![
                Call::Pending("pane-1".into()),
                Call::Send("pane-1".into(), keys(&["Return"])),
            ]
        );
    }

    #[test]
    fn confirmation_failure_is_returned_unchanged_without_bare_enter() {
        let mut env = ScriptedEnv::new(Some("message")).confirm_result((false, "not submitted"));
        let result = press_keys_unlocked(&mut env, "pane-1", &keys(&["KP_Enter"]));
        assert_eq!(result, (false, "not submitted".into()));
        assert_eq!(
            env.calls,
            vec![
                Call::Pending("pane-1".into()),
                Call::Confirm("pane-1".into(), "message".into()),
            ]
        );
    }

    #[test]
    fn confirmation_receives_exact_nonempty_unicode_text_once() {
        let text = "draft 🐑 é";
        let mut env = ScriptedEnv::new(Some(text));
        let result = press_keys_unlocked(&mut env, "pane-1", &keys(&["enter"]));
        assert_eq!(result, (true, "confirmed detail".into()));
        assert_eq!(
            env.calls,
            vec![
                Call::Pending("pane-1".into()),
                Call::Confirm("pane-1".into(), text.into()),
            ]
        );
    }
}
