//! Request-scoped layout mutations with canonical process verification and lane services.
use super::RequestRuntimeAdapter;
use crate::error::DunkError;
use crate::flock::commands_layout::AgentOperation;
use serde_json::Value;

impl RequestRuntimeAdapter<'_> {
    pub(super) fn with_request_layout_agent(
        &self,
        operation: &mut AgentOperation<'_>,
    ) -> Result<Value, DunkError> {
        let status = self.terminal.flock.status_state()?;
        let mut layout = RequestRuntimeAdapter::new(
            self.terminal.flock,
            self.request,
            self.runtime,
            self.terminal.terminal,
        );
        let mut verifier = RequestRuntimeAdapter::new(
            self.terminal.flock,
            self.request,
            self.runtime,
            self.terminal.terminal,
        );
        // Each terminal operation holds only its own short lock. Verification and
        // lane registration retain the original flock/request identity.
        operation(
            &mut layout,
            self,
            &mut verifier,
            &status.env,
            status.agent_verify_s,
        )
    }
}

#[cfg(test)]
#[path = "layout_agent_tests.rs"]
mod tests;
