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
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::Flock;
use crate::metrics::Event;
use crate::server::{Dispatcher, PaneInfo, RequestCtx};
use crate::server_runtime::TerminalCallAdapter;
use crate::terminal::Terminal;

#[allow(dead_code)] // The server startup assembly is outside this dispatcher leaf.
pub(crate) type TerminalFactory = Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static>;

/// Shared server dispatcher; terminal instances stay local to each method call.
#[allow(dead_code)] // Constructed by the dependent production server startup assembly.
pub(crate) struct RuntimeDispatcher {
    flock: Arc<Flock>,
    runtime: Handle,
    terminal_factory: TerminalFactory,
    handlers: api::Dispatcher,
}

#[allow(dead_code)] // Used once the dispatcher is wired by the production startup assembly.
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
    #[allow(dead_code)] // Constructed by the dependent production server startup assembly.
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

    #[allow(dead_code)] // Used by Dispatcher::pane_info after production startup wiring.
    fn pane_info_at(&self, pane_id: &str, now: f64) -> Result<Option<PaneInfo>, DunkError> {
        if pane_id.is_empty() {
            return Ok(None);
        }
        let terminal = Mutex::new((self.terminal_factory)());
        let terminal = TerminalCallAdapter {
            flock: &self.flock,
            terminal: &terminal,
            runtime: &self.runtime,
        };
        let list_panes_grouped = || terminal.try_list_panes_grouped();
        let get_pane = |id: &str| PaneInfoEnv::get_pane(&terminal, id);
        let query = RolePaneQuery {
            list_panes_grouped: &list_panes_grouped,
            get_pane: &get_pane,
            list_error: Mutex::new(None),
        };
        let pane = self.flock.role_pane_info(&query, pane_id, now);
        if pane.is_none() {
            if let Some(error) = query
                .list_error
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
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
        self.pane_info_at(pane_id, crate::clock::now_s())
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
