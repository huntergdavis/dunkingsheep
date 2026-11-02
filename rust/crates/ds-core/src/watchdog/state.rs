//! Persistent watchdog state (`empty_state`, `load_state`, and `save_state`).
//!
//! Reads preserve unknown keys for forward compatibility and add defaults for keys introduced
//! by a newer watchdog. Writes use the shared same-directory atomic JSON writer.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::error::DunkError;

/// The complete default state shape used on a first run or after state loss.
pub fn empty_state() -> Value {
    json!({
        "alerts": {},
        "streaks": {},
        "owner_pages": {},
        "owner_page_times": [],
    })
}

/// Load state and its fail-toward-not-paging diagnostic.
///
/// A missing or malformed file returns a fresh state plus a problem string. A valid JSON object
/// is preserved and receives any missing keys from `empty_state`; non-object JSON is malformed.
pub fn load_state(path: &Path) -> (Value, Option<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                empty_state(),
                Some(format!(
                    "state {}: missing (first run, or the dedupe store was lost)",
                    path.display()
                )),
            );
        }
        Err(error) => {
            return (
                empty_state(),
                Some(format!("state {}: corrupt ({error})", path.display())),
            );
        }
    };
    let mut state: Value = match serde_json::from_str(&text) {
        Ok(Value::Object(state)) => Value::Object(state),
        Ok(_) => {
            return (
                empty_state(),
                Some(format!("state {}: corrupt (not an object)", path.display())),
            );
        }
        Err(error) => {
            return (
                empty_state(),
                Some(format!("state {}: corrupt ({error})", path.display())),
            );
        }
    };

    if let (Value::Object(state), Value::Object(defaults)) = (&mut state, empty_state()) {
        for (key, value) in defaults {
            state.entry(key).or_insert(value);
        }
    }
    (state, None)
}

/// Persist watchdog state atomically, preserving the shared helper's sorted JSON and mode 0600.
pub fn save_state(path: &Path, state: &Value) -> Result<(), DunkError> {
    crate::atomic_json::write(path, state)
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
