//! T39 recovery API wrappers over the canonical Flock producers.

use super::{truthy, Args, HandlerContext};
use crate::{error::DunkError, flock::recovery_commands::RecoveryCommandError};
use serde_json::Value;

// The accepted handler signature carries wire errors as DunkError. Python's server
// prefixes uncaught exceptions, while domain/backend errors retain their category.
fn wire_error(error: RecoveryCommandError) -> DunkError {
    match error {
        RecoveryCommandError::Backend(error) => error,
        RecoveryCommandError::Python { message, .. } => {
            DunkError::Refused(format!("internal error: {message}"))
        }
    }
}

/// `dunk_api.h_recover`: bool(_opt(...)), not the CLI-style _bool parser.
pub fn h_recover(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let apply = args.get("apply").is_some_and(truthy);
    let record = args.get("record").is_some_and(truthy);
    let env = context.env.recover_env()?;
    context
        .flock
        .recover(env, apply, record)
        .map_err(wire_error)
}

/// `dunk_api.h_doctor`: no conversion, snapshot, or replacement service construction.
pub fn h_doctor(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    let env = context.env.recover_env()?;
    context.flock.doctor(env).map_err(wire_error)
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
