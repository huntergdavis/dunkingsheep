//! Source-compatible delivery-state inspection and authorized composer clearing.
//!
//! Ports `Flock.delivery_state` and `Flock.clear_box` only. This module does not mutate the
//! outbox or submit text; the clear action is the guarded `C-u`, `Escape` pair.

use serde_json::{json, Value};

use super::panes::PaneInfoEnv;
use super::resolve::{resolve_target, resolve_target_dict};
use super::sends::{self, SendEnv};
use super::Flock;
use crate::error::DunkError;
use crate::terminal::screen::{
    box_text, classify_delivery, collapse, modal_open, typed_text, AMBIGUOUS_COMPOSER,
};

/// Services needed to resolve a pane and inspect or clear its composer.
pub trait ClearBoxEnv: SendEnv + PaneInfoEnv {
    /// `herdr.server_error_hint()` for an empty pane listing.
    fn server_error_hint(&self) -> String;
}

impl Flock {
    /// Classify whether one pane can receive a message, preserving Python's wire fields.
    pub fn delivery_state(
        &self,
        env: &dyn ClearBoxEnv,
        pane_id: &str,
        status: Option<&str>,
        pane: Option<&Value>,
        screen: Option<&str>,
    ) -> Value {
        let status = status
            .map(str::to_owned)
            .or_else(|| {
                pane.and_then(|pane| {
                    pane.get("agent_status")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
            })
            .or_else(|| env.read_status(pane_id));

        let (screen, detail) = match screen {
            Some(screen) => {
                let (_, detail) = box_text(screen);
                (Some(screen.to_owned()), detail)
            }
            None => {
                let (screen, _, detail) = sends::read_box(env, pane_id);
                (screen, detail)
            }
        };
        let Some(screen) = screen else {
            return json!({"state": "unknown", "reason": "pane unreadable"});
        };

        let typed = if modal_open(&screen) {
            None
        } else {
            typed_text(&screen)
        };
        let (mut state, mut reason) =
            classify_delivery(&screen, status.as_deref(), typed.as_deref(), None, false);
        if matches!(state, "ready" | "working") && detail.starts_with(AMBIGUOUS_COMPOSER) {
            state = "unknown";
            reason = Some(detail);
        }

        let foreign = typed
            .as_deref()
            .is_some_and(|text| !text.is_empty() && !self.looks_like_ours(pane_id, text));
        let excerpt = if foreign {
            typed
                .as_deref()
                .map(|text| collapse(text).chars().take(60).collect::<String>())
        } else {
            None
        };
        json!({
            "state": state,
            "reason": reason,
            "deliverable": matches!(state, "ready" | "working"),
            "foreign_text": foreign,
            "foreign_excerpt": excerpt,
        })
    }

    /// Clear a pane's composer only for an authorized owner. This never submits text.
    pub fn clear_box(
        &self,
        env: &mut dyn ClearBoxEnv,
        target: &Value,
        self_pane_id: Option<&str>,
        reason: Option<&Value>,
    ) -> Result<Value, DunkError> {
        // Authorization deliberately precedes target resolution, matching the destructive
        // Python API and preventing non-owners from probing pane names through this action.
        self.require_superuser(self_pane_id, "clear_box", env)?;

        let panes = env.list_panes_grouped();
        let pane = if target.is_object() {
            resolve_target_dict(&panes, target)?
        } else {
            let spec = match target {
                Value::Null => String::new(),
                Value::String(spec) => spec.clone(),
                Value::Bool(value) => value.to_string(),
                other => other.to_string(),
            };
            resolve_target(&panes, &spec, self_pane_id, &env.server_error_hint())?.clone()
        };
        let pane_id = pane
            .get("pane_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let target_label = ["tab_label", "label", "name", "agent_name"]
            .iter()
            .find_map(|key| {
                pane.get(*key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or(&pane_id)
            .to_owned();
        let before = self.delivery_state(env, &pane_id, None, None, None);

        match before.get("state").and_then(Value::as_str) {
            Some("ready") => {
                return Ok(json!({
                    "pane_id": pane_id,
                    "target_label": target_label,
                    "flushed": false,
                    "before": before,
                    "reason": "nothing to clear: the input box is already clear",
                }));
            }
            Some("working") => {
                return Ok(json!({
                    "pane_id": pane_id,
                    "target_label": target_label,
                    "flushed": false,
                    "before": before,
                    "reason": "the lane is working; a clear would interrupt it",
                }));
            }
            _ => {}
        }

        let pane_lock = self.send_lock_for(&pane_id);
        let _pane_guard = pane_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let typing_guard = self
            .typing_guard(&pane_id)
            .map_err(|error| flush_error(&pane_id, error.to_string()))?;
        for key in ["C-u", "Escape"] {
            let (ok, detail) = env.send_key(&pane_id, key);
            if !ok {
                return Err(flush_error(&pane_id, detail));
            }
        }
        drop(typing_guard);
        drop(_pane_guard);

        let after = self.delivery_state(env, &pane_id, None, None, None);
        tracing::warn!(
            pane_id = %pane_id,
            by = ?self_pane_id,
            reason = ?reason,
            before = ?before.get("state"),
            after = ?after.get("state"),
            "flushed pane composer"
        );
        Ok(json!({
            "pane_id": pane_id,
            "target_label": target_label,
            "flushed": true,
            "before": before,
            "after": after,
            "by": self_pane_id,
            "reason": reason,
        }))
    }
}

fn flush_error(pane_id: &str, detail: String) -> DunkError {
    DunkError::Refused(format!("flush failed for {pane_id}: {detail}"))
}
