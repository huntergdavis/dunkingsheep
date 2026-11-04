//! T39 read-only dunk lookup adapter.

use super::{python_value_string, Args, HandlerContext};
use crate::error::DunkError;
use serde_json::Value;

/// `dunk_api.h_get_dunk`.
pub fn h_get_dunk(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args
        .get("id")
        .ok_or_else(|| DunkError::Refused("'id'".into()))?;
    context
        .flock
        .dunks
        .get(&python_value_string(id))
        .map(Value::Object)
}
