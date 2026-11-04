//! Request-local binding to the existing validated inbox runtime view.

use super::{inbox_data::InboxRuntimeView, RequestRuntimeAdapter, TerminalCallAdapter};
use crate::{error::DunkError, flock::commands_inbox::InboxEnv};
use serde_json::Value;
use std::sync::Arc;

impl RequestRuntimeAdapter<'_> {
    pub(super) fn with_request_inbox(
        &self,
        operation: &mut dyn FnMut(&dyn InboxEnv) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        let outbox = self.outbox.ok_or_else(|| {
            DunkError::Refused("request inbox outbox service is not attached".into())
        })?;
        let adapter = RequestRuntimeAdapter {
            terminal: TerminalCallAdapter {
                flock: self.terminal.flock,
                terminal: self.terminal.terminal,
                runtime: self.terminal.runtime,
            },
            request: self.request,
            runtime: self.runtime,
            inbox: self.inbox.clone(),
            outbox: None,
            next_jobs: self.next_jobs,
        }
        .with_outbox(outbox)?;
        let view =
            InboxRuntimeView::new(adapter, Arc::clone(&outbox.flock), Arc::clone(&outbox.pool))?;
        operation(&view)
    }
}

#[cfg(test)]
mod tests;
