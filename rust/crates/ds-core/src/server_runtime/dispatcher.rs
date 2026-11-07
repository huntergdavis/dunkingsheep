//! Production bridge from the socket server to the command API.
//!
//! The shared dispatcher stores only Send+Sync services. Each request constructs its
//! non-Send terminal on the existing connection thread, then borrows the request context
//! through the synchronous API call.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{services::RuntimeServices, RequestRuntimeAdapter};
use crate::api;
use crate::error::DunkError;
use crate::metrics::Event;
use crate::server::{Dispatcher, PaneInfo, RequestCtx};
/// Shared server dispatcher; terminal instances stay local to each method call.
pub(crate) struct RuntimeDispatcher {
    services: Arc<RuntimeServices>,
    handlers: api::Dispatcher,
}

impl RuntimeDispatcher {
    pub(super) fn new(services: Arc<RuntimeServices>, handlers: api::Dispatcher) -> Self {
        Self { services, handlers }
    }
}

impl Dispatcher for RuntimeDispatcher {
    fn dispatch(&self, cmd: &str, args: &Value, ctx: &RequestCtx) -> Result<Value, DunkError> {
        let args = args
            .as_object()
            .cloned()
            .ok_or_else(|| DunkError::Refused("args must be an object".into()))?;
        let terminal = Mutex::new((self.services.terminal_factory)());
        let adapter = RequestRuntimeAdapter::new(
            &self.services.flock,
            ctx,
            self.services.runtime.handle(),
            &terminal,
        )
        .with_outbox(&self.services.outbox)?
        .with_next_jobs(&self.services.next_jobs)?
        .with_dunk_runtime(&self.services)?
        .with_owner_push(&self.services)?;
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
        // Caller binding reads the root pid from the identity listing, which carries no free text
        // (t3368). A pane that listing does not show, or a listing that cannot be read, is an
        // error here, and the binding refuses the claim: no grace window stands in for it.
        let terminal = Mutex::new((self.services.terminal_factory)());
        let root = self
            .services
            .flock
            .pane_root_pid(pane_id, crate::clock::now_s(), || {
                let terminal = terminal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                terminal
                    .try_pane_root_pid(pane_id)
                    .map_err(|error| DunkError::Refused(error.message))
            })?;
        Ok(root.map(|pane_pid| PaneInfo { pane_pid }))
    }

    fn record_identity_mismatch(&self, pane_id: &str, reason: &str) {
        if let Some(metrics) = self.services.flock.stores().metrics {
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
