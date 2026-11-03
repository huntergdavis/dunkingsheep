//! Concrete terminal-backed pane and screen reads for agent verification.

use serde_json::Value;

use crate::flock::panes::PaneInfoEnv;
use crate::server_runtime::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete VerifyBackend assembly belongs to its dependent leaf.
    fn helper_verifybackend_get_pane(&self, pane_id: &str) -> Option<Value> {
        self.terminal
            .with_env(|env| PaneInfoEnv::get_pane(env, pane_id))
    }

    #[allow(dead_code)] // The complete VerifyBackend assembly belongs to its dependent leaf.
    fn helper_verifybackend_read_pane(&mut self, pane_id: &str, lines: usize) -> Option<String> {
        let lines = i64::try_from(lines).unwrap_or(i64::MAX);
        self.terminal
            .with_terminal(|terminal| terminal.read_pane(pane_id, lines, false).ok().flatten())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_verify_backend_reads_pane_and_screen_without_fabrication() {
        let dir = crate::testsupport::dir("verify-runtime");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut fake = FakeTerminal::new();
        fake.screens
            .insert("w1:p1".into(), "first line\nsecond line\nthird line".into());
        fake.unreadable.insert("w1:p2".into());
        let reads = fake.reads.clone();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let mut adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let pane = adapter.helper_verifybackend_get_pane("w1:p1").unwrap();
        assert_eq!("w1:p1", pane["pane_id"]);
        assert_eq!("work", pane["workspace_label"]);
        assert!(adapter.helper_verifybackend_get_pane("missing").is_none());

        assert_eq!(
            Some("second line\nthird line".into()),
            adapter.helper_verifybackend_read_pane("w1:p1", 2)
        );
        assert_eq!(None, adapter.helper_verifybackend_read_pane("w1:p2", 20));
        assert_eq!(None, adapter.helper_verifybackend_read_pane("missing", 20));
        assert_eq!(
            vec![
                ("w1:p1".into(), 2, false),
                ("w1:p2".into(), 20, false),
                ("missing".into(), 20, false)
            ],
            *reads.lock().unwrap()
        );
    }
}
