//! Concrete terminal-backed pane and screen reads for agent verification.

use serde_json::Value;

use crate::flock::panes::PaneInfoEnv;
use crate::server_runtime::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete VerifyBackend assembly belongs to its dependent leaf.
    fn helper_verifybackend_agent_child_pids(&mut self, pane_pid: i64) -> Option<Vec<Value>> {
        crate::proc::pane_agent_pids(Some(pane_pid)).map(|entries| {
            entries
                .into_iter()
                .map(|entry| {
                    serde_json::json!({
                        "pid": entry.pid,
                        "comm": entry.comm,
                        "state": entry.state,
                        "start": entry.start,
                        "argv": entry.argv,
                    })
                })
                .collect()
        })
    }

    #[allow(dead_code)] // The complete VerifyBackend assembly belongs to its dependent leaf.
    fn helper_verifybackend_process_start(&self, pid: i64) -> Option<i64> {
        crate::proc::process_start(pid)?.parse().ok()
    }

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
    fn production_verify_backend_uses_process_ancestry_when_available() {
        let dir = crate::testsupport::dir("verify-process-runtime");
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
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let mut adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let pid = i64::from(std::process::id());
        let entries = adapter
            .helper_verifybackend_agent_child_pids(pid)
            .expect("Linux process table available");
        let own = entries
            .iter()
            .find(|entry| entry["pid"] == pid)
            .expect("own process included");
        assert!(own["comm"].as_str().is_some_and(|comm| !comm.is_empty()));
        assert!(own["state"].as_str().is_some_and(|state| !state.is_empty()));
        assert!(own["argv"].is_array());
        assert_eq!(
            own["start"].as_str().and_then(|s| s.parse::<i64>().ok()),
            adapter.helper_verifybackend_process_start(pid)
        );
        assert_eq!(None, adapter.helper_verifybackend_agent_child_pids(0));
        assert_eq!(None, adapter.helper_verifybackend_process_start(0));
    }

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
