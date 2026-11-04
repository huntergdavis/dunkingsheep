//! Watchdog delivery adapters. No daemon startup or owner-channel posting is performed here.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{
    planner::dedupe_key,
    report::{digest, line},
    state::save_state,
};
use crate::client::{ClientError, FlockClient, Transport};
use crate::error::DunkError;

/// Use the caller's configured client credentials; the system sender suppresses pane identity.
/// `call` never starts a daemon and never retries a write after submission.
pub fn inbox_sender<T: Transport>(
    client: &mut FlockClient<T>,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), ClientError> {
    client.call(
        "inbox_send",
        json!({"to":to,"subject":subject,"body":body,"system_sender":"watchdog"})
            .as_object()
            .unwrap()
            .clone(),
    )?;
    Ok(())
}

/// Sender errors retain their Python exception class for the aggregated recipient diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct SendError {
    pub kind: String,
    pub message: String,
}

fn refused(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}
fn read_json(path: &Path) -> Result<Value, DunkError> {
    serde_json::from_str(&std::fs::read_to_string(path)?).map_err(|e| refused(e.to_string()))
}

/// Persist each accepted submission before trying the next recipient. An accepted submission
/// is not a read receipt. Persistence errors propagate immediately, outside the send catch.
pub fn send_starvation_group(
    config: &Value,
    state: &mut Value,
    item: &Value,
    now: f64,
    mut send: impl FnMut(&str, &str, &str) -> Result<(), SendError>,
) -> Result<(), DunkError> {
    let rule_id = item["rule"]
        .as_str()
        .ok_or_else(|| refused("finding rule must be a string"))?;
    let rule = &config["rules"][rule_id];
    let config_dir = config["stores"]["config_dir"]
        .as_str()
        .ok_or_else(|| refused("config_dir must be a string"))?;
    let path = PathBuf::from(crate::native_session::expanduser(config_dir)).join("superusers.json");
    let data = read_json(&path)?;
    let protected = data["panes"]
        .as_array()
        .filter(|panes| panes.iter().all(Value::is_string))
        .ok_or_else(|| refused("superusers.json panes must be a list of pane ids"))?;
    let recipients: BTreeSet<&str> = rule["superuser_panes"]
        .as_array()
        .ok_or_else(|| refused("superuser_panes must be a list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| refused("superuser_panes must contain pane ids"))
        })
        .collect::<Result<_, _>>()?;
    if !recipients
        .iter()
        .all(|pane| protected.iter().any(|p| p.as_str() == Some(pane)))
    {
        return Err(refused(
            "configured starvation recipients are not all current superusers",
        ));
    }
    let cooldown = rule["cooldown_minutes"]
        .as_f64()
        .ok_or_else(|| refused("cooldown_minutes must be numeric"))?
        * 60.0;
    let state_path = config["state_path"]
        .as_str()
        .ok_or_else(|| refused("state_path must be a string"))?;
    let state_path = PathBuf::from(crate::native_session::expanduser(state_path));
    let key = dedupe_key(item);
    let deliveries = state
        .as_object_mut()
        .ok_or_else(|| refused("state must be an object"))?
        .entry("superuser_deliveries")
        .or_insert_with(|| json!({}));
    deliveries
        .as_object_mut()
        .ok_or_else(|| refused("superuser_deliveries must be an object"))?
        .entry(&key)
        .or_insert_with(|| json!({}));
    let body = digest(
        "SUPERUSER allocation action",
        &[line("NOTIFY SUPERUSERS", item, &json!("no"), None, None)],
    );
    let mut errors = Vec::new();
    for pane in recipients {
        let accepted = state["superuser_deliveries"][&key]
            .as_object()
            .ok_or_else(|| refused("accepted deliveries must be an object"))?;
        if let Some(at) = accepted.get(pane) {
            let at = at
                .as_f64()
                .ok_or_else(|| refused("accepted delivery timestamp must be numeric"))?;
            if now - at < cooldown {
                continue;
            }
        }
        match send(
            pane,
            "watchdog: idle capacity needs backlog allocation",
            &body,
        ) {
            Err(error) => errors.push(format!("{pane}: {error}")),
            Ok(()) => {
                state["superuser_deliveries"][&key][pane] = json!(now);
                save_state(&state_path, state)?;
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(refused(errors.join("; ")))
    }
}

/// Route selection stays with the shared routing reader. This adapter never reads credentials.
/// The concrete Rust route reader is not wired by this leaf; callers provide it explicitly.
#[derive(Debug, thiserror::Error)]
#[error("desk brief post not wired: desk API token handling awaits VP decision")]
pub struct OwnerChannelUnavailable;

pub struct DeskBriefChannel<R> {
    pub confirm_path: PathBuf,
    route: R,
}
impl<R: Fn(&Path) -> Result<String, DunkError>> DeskBriefChannel<R> {
    pub fn new(confirm_path: PathBuf, route: R) -> Self {
        Self {
            confirm_path,
            route,
        }
    }
    pub fn desk_channel(&self) -> Result<String, DunkError> {
        (self.route)(self.confirm_path.parent().unwrap_or_else(|| Path::new(".")))
    }
    pub fn post(&self, _body: &str) -> Result<(), OwnerChannelUnavailable> {
        Err(OwnerChannelUnavailable)
    }
    pub fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        let status = read_json(&self.confirm_path)?;
        // Python short-circuits non-object status before resolving the route.
        if !status.is_object() || status.get("channel") != Some(&json!(self.desk_channel()?)) {
            return Err(refused(
                "Telegram confirmation channel differs from configured route or is missing",
            ));
        }
        match status.get("last_brief_id") {
            None | Some(Value::Null) => Ok(None),
            Some(value) if value.is_i64() || value.is_u64() => Ok(Some(value.clone())),
            Some(value) => {
                let repr = crate::api::python_value_repr(value);
                Err(refused(format!("last_brief_id is not an integer: {repr}")))
            }
        }
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
