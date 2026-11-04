//! Bind close to the request terminal and the canonical timer/persistence owner.
use super::RequestRuntimeAdapter;
use crate::api::layout_close::{CloseOperation, LayoutCloseEnv};
use crate::error::{refusal, DunkError};
use serde_json::Value;
impl LayoutCloseEnv for RequestRuntimeAdapter<'_> {
    fn with_layout(&self, operation: &mut CloseOperation<'_>) -> Result<Value, DunkError> {
        // Check the lifecycle service before any destructive backend operation.
        self.dunk_runtime
            .ok_or_else(|| refusal("request dunk runtime is not attached"))?;
        let mut layout = RequestRuntimeAdapter::new(
            self.terminal.flock,
            self.request,
            self.runtime,
            self.terminal.terminal,
        );
        operation(&mut layout)
    }
    fn close(&self, kind: &str, id: &str) -> crate::terminal::RawCreateResult<()> {
        self.terminal
            .with_terminal_mut(|terminal| terminal.close_raw(kind, id))
    }
    fn halt_aimed_at(&self, panes: &[String]) -> Result<Vec<String>, DunkError> {
        self.dunk_runtime
            .ok_or_else(|| refusal("request dunk runtime is not attached"))?
            .halt_dunks_aimed_at(panes)
    }
    fn backend_name(&self) -> String {
        self.terminal
            .with_terminal_mut(|terminal| terminal.backend_name().to_owned())
    }
    fn herdr_run(
        &self,
        argv: &[String],
        timeout_s: u64,
    ) -> Result<(i32, String, String), DunkError> {
        self.terminal
            .with_terminal_mut(|terminal| terminal.herdr_run(argv, timeout_s))
    }
}

#[cfg(test)]
#[path = "layout_close_tests.rs"]
mod tests;
