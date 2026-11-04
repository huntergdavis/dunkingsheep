//! Request-local access to the canonical outbox; never constructs a second pool.
use super::{outbox_runtime::FlockSnapshot, RequestRuntimeAdapter, TerminalCallAdapter};
use crate::api::outbox_messages::OutboxCommandEnv;
use crate::error::DunkError;
use serde_json::Value;

impl OutboxCommandEnv for RequestRuntimeAdapter<'_> {
    fn list_messages(&self) -> Result<Value, DunkError> {
        let outbox = self.require_send_outbox()?;
        let terminal = TerminalCallAdapter {
            flock: self.terminal.flock,
            terminal: self.terminal.terminal,
            runtime: self.runtime,
        };
        Ok(outbox.flock.list_messages(&terminal))
    }
    fn get_message(&self, id: &Value) -> Result<Value, DunkError> {
        self.require_send_outbox()?.flock.get_message(id)
    }
    fn cancel_message(&self, id: &Value) -> Result<Value, DunkError> {
        self.require_send_outbox()?
            .flock
            .cancel_message(id, &FlockSnapshot)
    }
}

#[cfg(test)]
mod tests;
