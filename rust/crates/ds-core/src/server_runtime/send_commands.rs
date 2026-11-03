//! The request-thread bridge to the canonical, guarded send and outbox paths.

use super::{
    outbox_runtime::FlockSnapshot, outbox_runtime::OutboxRuntime, RequestRuntimeAdapter,
    TerminalCallAdapter,
};
use crate::api::SendCommandEnv;
use crate::error::DunkError;
use crate::flock::sends::{DirectSendOptions, DirectTextMetadata};
use serde_json::Value;

impl<'a> RequestRuntimeAdapter<'a> {
    pub(super) fn require_send_outbox(&self) -> Result<&'a OutboxRuntime, DunkError> {
        let outbox = self
            .outbox
            .ok_or_else(|| DunkError::Refused("request outbox service is not attached".into()))?;
        if !std::ptr::eq(self.terminal.flock, outbox.flock.as_ref()) {
            return Err(DunkError::Refused("request outbox flock mismatch".into()));
        }
        Ok(outbox)
    }

    fn send_terminal(&self) -> TerminalCallAdapter<'a> {
        TerminalCallAdapter {
            flock: self.terminal.flock,
            terminal: self.terminal.terminal,
            runtime: self.runtime,
        }
    }
}

impl SendCommandEnv for RequestRuntimeAdapter<'_> {
    fn send_text(
        &self,
        target: &Value,
        text: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
        press_enter: bool,
    ) -> Result<Value, DunkError> {
        let outbox = self.require_send_outbox()?;
        let mut terminal = self.send_terminal();
        outbox.flock.send_now(
            target,
            text,
            DirectSendOptions {
                self_pane_id,
                hold_while_typing,
                wait_s,
            },
            DirectTextMetadata {
                press_enter,
                ..DirectTextMetadata::default()
            },
            &mut terminal,
            outbox.pool.as_ref(),
            &FlockSnapshot,
            || None,
        )
    }

    fn send_keys(
        &self,
        target: &Value,
        keys: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
    ) -> Result<Value, DunkError> {
        let outbox = self.require_send_outbox()?;
        let keys: Vec<String> = keys.split_whitespace().map(str::to_owned).collect();
        let mut terminal = self.send_terminal();
        outbox.flock.send_keys_now(
            target,
            &keys,
            DirectSendOptions {
                self_pane_id,
                hold_while_typing,
                wait_s,
            },
            &mut terminal,
            outbox.pool.as_ref(),
            &FlockSnapshot,
            || None,
        )
    }

    fn flush_box(&self, target: &Value, self_pane_id: Option<&str>) -> Result<Value, DunkError> {
        self.require_send_outbox()?;
        let mut terminal = self.send_terminal();
        crate::flock::sends::flush_box(&mut terminal, target, self_pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CommandEnv;
    use crate::flock::{panes::Stores, Flock};
    use crate::inbox_store::Store;
    use crate::server::RequestCtx;
    use crate::server_runtime::outbox_runtime::OutboxRuntime;
    use crate::terminal::fakes::FakeTerminal;
    use std::sync::{Arc, Mutex};

    #[test]
    fn send_command_service_refuses_unbound_and_mismatched_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let request_flock = Flock::new(
            dir.path().join("request-launches.json"),
            dir.path().join("request-lanes.json"),
            false,
        );
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn crate::terminal::Terminal>> =
            Mutex::new(Box::new(FakeTerminal::new()));
        let unbound =
            RequestRuntimeAdapter::new(&request_flock, &request, runtime.handle(), &terminal);
        assert_eq!(
            "request outbox service is not attached",
            CommandEnv::send_commands(&unbound)
                .err()
                .unwrap()
                .to_string()
        );

        let outbox_flock = Arc::new(Flock::new(
            dir.path().join("outbox-launches.json"),
            dir.path().join("outbox-lanes.json"),
            false,
        ));
        outbox_flock.attach_stores(Stores {
            inbox: Some(Arc::new(
                Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap(),
            )),
            ..Stores::default()
        });
        let outbox = OutboxRuntime::new(
            Arc::clone(&outbox_flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let mismatched =
            RequestRuntimeAdapter::new(&request_flock, &request, runtime.handle(), &terminal)
                .with_outbox(&outbox);
        assert_eq!(
            "request outbox flock mismatch",
            mismatched.err().unwrap().to_string()
        );
        outbox.stop_and_join();
    }
}
