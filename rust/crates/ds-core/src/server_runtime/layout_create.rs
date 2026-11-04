//! Request-scoped layout mutation and confirmed submission over the owned terminal.

use super::{RequestRuntimeAdapter, TerminalCallAdapter};
use crate::error::DunkError;
use crate::flock::commands_layout::{LayoutBackend, RawSubmit};
use crate::flock::panes::PaneInfoEnv;
use serde_json::Value;

impl RequestRuntimeAdapter<'_> {
    pub(super) fn with_request_layout_create(
        &self,
        operation: &mut dyn FnMut(
            &mut dyn LayoutBackend,
            &dyn PaneInfoEnv,
            RawSubmit<'_>,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        let mut layout = RequestRuntimeAdapter::new(
            self.terminal.flock,
            self.request,
            self.runtime,
            self.terminal.terminal,
        );
        let mut terminal = TerminalCallAdapter {
            flock: self.terminal.flock,
            terminal: self.terminal.terminal,
            runtime: self.runtime,
        };
        let mut submit = |pane: &str, text: &Value| {
            let mut result = crate::flock::sends::submit_text_raw(
                self.terminal.flock,
                &mut terminal,
                pane,
                text,
            )?;
            crate::flock::sends::confirm_send_action(&mut terminal, &mut result);
            Ok((result.ok, result.detail))
        };
        // The original request remains the pane/role adapter; no service binding is lost.
        // Each terminal method takes its own short lock, never the whole callback.
        operation(&mut layout, self, &mut submit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CommandEnv;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use std::sync::Mutex;

    #[test]
    fn layout_create_bridge_releases_terminal_between_create_and_submit() {
        let dir = tempfile::tempdir().unwrap();
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
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let mut operation =
            |layout: &mut dyn LayoutBackend, panes: &dyn PaneInfoEnv, submit: RawSubmit<'_>| {
                assert!(terminal.try_lock().is_ok());
                let created = layout.split_pane("w1:p1", "right", None, false).unwrap();
                assert!(terminal.try_lock().is_ok());
                assert!(panes.get_pane("w1:p1").is_some());
                let pane = created["root_pane"]["pane_id"].as_str().unwrap();
                let (ran, detail) = submit(pane, &serde_json::json!("echo bridge")).unwrap();
                assert!(ran, "{detail}");
                assert!(terminal.try_lock().is_ok());
                Ok(created)
            };
        assert!(adapter.with_layout_create(&mut operation).is_ok());
    }
}

#[cfg(test)]
#[path = "layout_create_tests.rs"]
mod dispatcher_tests;
