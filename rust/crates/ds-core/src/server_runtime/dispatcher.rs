//! Production bridge from the socket server to the command API.
//!
//! The shared dispatcher stores only Send+Sync services. Each request constructs its
//! non-Send terminal on the existing connection thread, then borrows the request context
//! through the synchronous API call.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::runtime::Handle;

use super::RequestRuntimeAdapter;
use crate::api;
use crate::error::DunkError;
use crate::flock::Flock;
use crate::metrics::Event;
use crate::server::{Dispatcher, PaneInfo, RequestCtx};
use crate::terminal::Terminal;

pub(crate) type TerminalFactory = Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static>;

/// Shared server dispatcher; terminal instances stay local to each method call.
pub(crate) struct RuntimeDispatcher {
    flock: Arc<Flock>,
    runtime: Handle,
    terminal_factory: TerminalFactory,
    handlers: api::Dispatcher,
}

impl RuntimeDispatcher {
    pub(crate) fn new(
        flock: Arc<Flock>,
        runtime: Handle,
        terminal_factory: TerminalFactory,
        handlers: api::Dispatcher,
    ) -> Self {
        Self {
            flock,
            runtime,
            terminal_factory,
            handlers,
        }
    }
}

impl Dispatcher for RuntimeDispatcher {
    fn dispatch(&self, cmd: &str, args: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
        let args = args
            .as_object()
            .cloned()
            .ok_or_else(|| DunkError::Refused("args must be an object".into()))?;
        let terminal = Mutex::new((self.terminal_factory)());
        let adapter = RequestRuntimeAdapter::new(&self.flock, ctx, &self.runtime, &terminal);
        let context = adapter.handler_context();
        let gate = api::FlockGate(&context);
        // api::Dispatcher::dispatch installs and drops RequestCallerGuard around the real
        // handler call. Installing another guard here would clear the same state too early.
        self.handlers.dispatch(&context, &gate, cmd, Some(args))
    }

    fn pane_info(&self, pane_id: &str) -> Result<Option<PaneInfo>, DunkError> {
        if pane_id.is_empty() {
            return Ok(None);
        }
        let terminal = (self.terminal_factory)();
        let groups = terminal
            .try_list_panes_grouped()
            .map_err(|error| DunkError::Refused(error.to_string()))?;
        let pane = groups
            .values()
            .flatten()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
            .or_else(|| terminal.get_pane(pane_id));
        Ok(pane.map(|pane| PaneInfo {
            pane_pid: pane.pane_pid,
        }))
    }

    fn record_identity_mismatch(&self, pane_id: &str, reason: &str) {
        if let Some(metrics) = self.flock.stores().metrics {
            metrics.record(&Event {
                at: crate::clock::now_s(),
                kind: "identity.mismatch".into(),
                pane_id: Some(pane_id.into()),
                source: Some(reason.into()),
                ..Event::default()
            });
        }
    }
}

#[cfg(test)]
#[path = "dispatcher/tests.rs"]
mod tests;
