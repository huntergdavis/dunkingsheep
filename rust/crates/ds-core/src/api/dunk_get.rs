//! T39 read-only dunk lookup adapter.

use super::{python_value_repr, python_value_string, Args, HandlerContext};
use crate::error::DunkError;
use serde_json::Value;

/// `dunk_api.h_get_dunk`.
pub fn h_get_dunk(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args
        .get("id")
        .ok_or_else(|| DunkError::Refused("'id'".into()))?;
    let dunk_id = python_value_string(id);
    // Python str.strip also recognizes the four ASCII information separators.
    let normalized_id =
        dunk_id.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}'));
    match context.flock.dunks.get(normalized_id) {
        Ok(dunk) => Ok(Value::Object(dunk)),
        Err(error) => {
            let core_missing_id = format!("no dunk with id {normalized_id:?}");
            if matches!(&error, DunkError::Refused(message) if message == &core_missing_id) {
                Err(DunkError::Refused(format!(
                    "no dunk with id {}",
                    python_value_repr(&Value::String(normalized_id.to_owned()))
                )))
            } else {
                Err(error)
            }
        }
    }
}
