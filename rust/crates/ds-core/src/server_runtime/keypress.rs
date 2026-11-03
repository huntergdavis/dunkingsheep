//! Concrete key routing for the connection thread. Confirmation waits run after the
//! caller's typing guard is released; only individual retry keys acquire that guard.

use super::TerminalCallAdapter;
use crate::flock::send_keys::KeyPressEnv;
use crate::flock::sends::{self, SendEnv, StatusBaseline};

impl KeyPressEnv for TerminalCallAdapter<'_> {
    fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String) {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        match self.with_terminal_mut(|terminal| terminal.send_keys(pane_id, &keys)) {
            Ok(()) => (true, "sent".into()),
            Err(error) => (false, error.to_string()),
        }
    }

    fn pending_text(&mut self, pane_id: &str) -> Option<String> {
        let screen = SendEnv::read_pane(self, pane_id, sends::CONSUMPTION_READ_LINES, true)?;
        self.flock.recovery_box_text(self, pane_id, &screen)
    }

    fn confirm_pending(&mut self, pane_id: &str, text: &str) -> (bool, String) {
        sends::press_until_submitted(self, pane_id, text, false, false, StatusBaseline::ReadNow)
    }
}

#[cfg(test)]
mod tests;
