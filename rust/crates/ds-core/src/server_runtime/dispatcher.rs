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
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::metrics::Event;
use crate::server::{Dispatcher, PaneInfo, RequestCtx};
use crate::server_runtime::TerminalCallAdapter;
/// Shared server dispatcher; terminal instances stay local to each method call.
pub(crate) struct RuntimeDispatcher {
    services: Arc<RuntimeServices>,
    handlers: api::Dispatcher,
}

struct RolePaneQuery<'a> {
    list_panes_grouped: &'a dyn Fn() -> Result<Vec<Value>, DunkError>,
    get_pane: &'a dyn Fn(&str) -> Option<Value>,
    list_error: Mutex<Option<DunkError>>,
}

impl PaneInfoEnv for RolePaneQuery<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        match (self.list_panes_grouped)() {
            Ok(panes) => panes,
            Err(error) => {
                *self
                    .list_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
                Vec::new()
            }
        }
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        (self.get_pane)(pane_id)
    }
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
        .with_next_jobs(&self.services.next_jobs)?;
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
        let terminal = Mutex::new((self.services.terminal_factory)());
        let terminal = TerminalCallAdapter {
            flock: &self.services.flock,
            terminal: &terminal,
            runtime: self.services.runtime.handle(),
        };
        let list_panes_grouped = || terminal.try_list_panes_grouped();
        let get_pane = |id: &str| PaneInfoEnv::get_pane(&terminal, id);
        let query = RolePaneQuery {
            list_panes_grouped: &list_panes_grouped,
            get_pane: &get_pane,
            list_error: Mutex::new(None),
        };
        let pane = self
            .services
            .flock
            .role_pane_info(&query, pane_id, crate::clock::now_s());
        if pane.is_none() {
            if let Some(error) = query
                .list_error
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                self.services.flock.forget_role_pane(pane_id);
                return Err(error);
            }
        }
        Ok(pane.map(|pane| PaneInfo {
            pane_pid: pane.get("pane_pid").and_then(|pid| {
                pid.as_i64()
                    .or_else(|| pid.as_str().and_then(|text| text.parse().ok()))
            }),
        }))
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

    fn close(&self) {
        self.services.shutdown();
    }
}

#[cfg(test)]
#[path = "dispatcher/tests.rs"]
mod tests;
