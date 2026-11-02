//! Submission's concrete connection-thread adapter (t2302, t1917/t1932).
//! Python sources: Flock._submit_key initialization, _press_until_submitted's one-burst
//! typing guard and submit.attempt metrics. The final runtime owns the outer pane guard.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::TerminalCallAdapter;
use crate::flock::sends::{self, FlushEnv, SubmitEnv};
use crate::metrics::Event;

#[allow(dead_code)] // Final dispatcher/worker composition remains gated on t1605.
impl TerminalCallAdapter<'_> {
    fn helper_submitenv_sleep_s(&mut self, seconds: f64) {
        // This adapter runs on a dedicated standard thread, never a Tokio task. No terminal
        // mutex or typing guard survives the preceding one-key call into this retry wait.
        std::thread::sleep(Duration::from_secs_f64(seconds));
    }

    fn helper_submitenv_now_s(&mut self) -> f64 {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_secs_f64()
    }

    fn helper_submitenv_send_submit_key_guarded(
        &mut self,
        pane_id: &str,
        fallback: &str,
        busy_hint: bool,
    ) -> (bool, String, Option<String>) {
        let flock = self.flock;
        let _guard = match flock.typing_guard(pane_id) {
            Ok(guard) => guard,
            Err(error) => return (false, error.to_string(), None),
        };
        // The accepted decision re-reads the screen/harness/status on every retry. Each
        // backend operation holds the terminal mutex only for that operation.
        sends::send_submit_key(self, pane_id, fallback, busy_hint)
    }

    fn helper_submitenv_remembered_submit_key(&self, pane_id: &str) -> Option<String> {
        self.flock
            .learned_submit_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .cloned()
    }

    fn helper_submitenv_remember_submit_key(&mut self, pane_id: &str, key: &str) {
        // The accepted ladder normalizes successful Tab to Enter before remembering it.
        self.flock
            .learned_submit_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pane_id.to_string(), key.to_string());
    }

    fn helper_submitenv_record_submit_attempt(
        &mut self,
        pane_id: &str,
        outcome: &str,
        attempts: usize,
        bytes: usize,
    ) {
        // Telemetry must not fail delivery; the daemon attaches the real enabled/disabled
        // metrics store before composing submission workers. Missing wiring is named.
        let Some(metrics) = self.flock.stores().metrics else {
            eprintln!("submit.attempt: metrics store is not wired");
            return;
        };
        metrics.record(&Event {
            at: crate::clock::now_s(),
            kind: "submit.attempt".into(),
            pane_id: Some(pane_id.into()),
            outcome: Some(outcome.into()),
            attempts: Some(attempts as i64),
            bytes: Some(bytes as i64),
            ..Event::default()
        });
    }
}

impl SubmitEnv for TerminalCallAdapter<'_> {
    fn sleep_s(&mut self, seconds: f64) {
        self.helper_submitenv_sleep_s(seconds);
    }
    fn now_s(&mut self) -> f64 {
        self.helper_submitenv_now_s()
    }
    fn send_submit_key_guarded(
        &mut self,
        pane: &str,
        fallback: &str,
        busy: bool,
    ) -> (bool, String, Option<String>) {
        self.helper_submitenv_send_submit_key_guarded(pane, fallback, busy)
    }
    fn remembered_submit_key(&self, pane: &str) -> Option<String> {
        self.helper_submitenv_remembered_submit_key(pane)
    }
    fn remember_submit_key(&mut self, pane: &str, key: &str) {
        self.helper_submitenv_remember_submit_key(pane, key);
    }
    fn record_submit_attempt(&mut self, pane: &str, outcome: &str, attempts: usize, bytes: usize) {
        self.helper_submitenv_record_submit_attempt(pane, outcome, attempts, bytes);
    }
}

impl FlushEnv for TerminalCallAdapter<'_> {
    fn server_error_hint(&self) -> String {
        self.with_terminal(|terminal| terminal.server_error_hint())
    }
}

#[cfg(test)]
mod tests;
