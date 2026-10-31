//! `flock/lanes.rs` (T30, slice 5): the `terminal::Pane` -> JSON adapter, and
//! `lane_activity`, `lane_status`, `resolve_lanes`.
//!
//! The adapter is `TmuxBackend._parse_panes`' dict (the shape every resolver in this module
//! reads) built from the typed [`Pane`]; the grouped listing is `TerminalBackend
//! .list_panes_grouped` (labels fall back to the id, sorted by workspace then tab number).
//! `managed` is the one key the typed `Pane` cannot supply (it is the start_agent registry's
//! knowledge), so it is left out rather than guessed.

use serde_json::{json, Value};

use super::mailbox::MailboxEnv;
use super::panes::PaneInfoEnv;
use super::Flock;
use crate::activity::{self, Turn};
use crate::error::DunkError;
use crate::terminal::parse::agent_kind;
use crate::terminal::{Pane, Terminal};

/// `Flock.IDLE_CLAIM_S` (= `dunk_activity.IDLE_CLAIM_S`).
pub const IDLE_CLAIM_S: f64 = activity::IDLE_CLAIM_S;

/// One typed pane as the tmux backend's dict. `agent_status` is the backend's answer
/// (`Terminal::agent_status`), "unknown" when it has none, exactly as `_parse_panes` starts.
pub fn pane_to_json(pane: &Pane, agent_status: Option<&str>) -> Value {
    // `parse_panes` stores the recognised kind in `command` (or the raw command when none was
    // recognised); `agent_kind` is idempotent over both, so the kind is recomputed, not guessed.
    let kind = agent_kind(&pane.command, &pane.title);
    json!({
        "pane_id": pane.pane_id,
        "terminal_id": pane.pane_id,
        "tab_id": pane.window_id,
        "workspace_id": pane.session_id,
        "cwd": pane.cwd,
        "foreground_cwd": pane.cwd,
        "agent": kind,
        "agent_status": agent_status.unwrap_or("unknown"),
        "agent_session": Value::Null,
        "focused": pane.active,
        "label": pane.label,
        "terminal_title_stripped": pane.label,
        "terminal_title": pane.title,
        "pane_pid": pane.pane_pid,
        "cols": pane.width,
        "rows": pane.height,
        "tab_label": pane.tab,
        "workspace_label": pane.workspace,
        "tab_number": pane.window_index,
        "workspace_number": pane.workspace_index,
    })
}

/// A [`Terminal`] seen through the JSON shape the resolvers read. `registered` is the role
/// registry's `_registered_pane` (no backend call).
pub struct TerminalEnv<'a> {
    pub terminal: &'a dyn Terminal,
    pub registered: &'a dyn Fn(&str) -> (bool, bool, Option<String>),
}

impl PaneInfoEnv for TerminalEnv<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        MailboxEnv::list_panes_grouped(self)
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.terminal
            .get_pane_with_status(pane_id)
            .map(|(pane, status)| pane_to_json(&pane, status.as_deref()))
    }
}

impl MailboxEnv for TerminalEnv<'_> {
    fn list_panes(&self) -> Vec<Value> {
        // One listing with the statuses (t1296): not an `agent_status` call per pane.
        self.terminal
            .list_panes_with_status()
            .iter()
            .map(|(pane, status)| pane_to_json(pane, status.as_deref()))
            .collect()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        let mut panes = self.list_panes();
        for pane in &mut panes {
            // `tab.get("label") or pane.get("tab_id", "?")`, likewise for the workspace.
            for (label, id) in [("tab_label", "tab_id"), ("workspace_label", "workspace_id")] {
                if pane[label].as_str().unwrap_or("").is_empty() {
                    pane[label] = pane[id].clone();
                }
            }
        }
        // Python's sort is stable on (workspace_number, tab_number).
        panes.sort_by_key(|p| {
            (
                p["workspace_number"].as_i64().unwrap_or(0),
                p["tab_number"].as_i64().unwrap_or(0),
            )
        });
        panes
    }
    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>) {
        (self.registered)(pane_id)
    }
}

impl Flock {
    /// Record a declared turn for a pane (`self.turns[pane_id]`); T36 owns turn_start/end and
    /// extends this, `lane_activity` only reads it.
    pub fn set_turn(&self, pane_id: &str, turn: Turn) {
        self.turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(pane_id.to_string(), turn);
    }

    /// `lane_activity`: what the lane is DOING. A backend-reported status wins; else the
    /// declared-turn derivation; `None` state (not "unknown") when there is no signal.
    pub fn lane_activity(&self, pane: &Value, now: f64) -> Value {
        let backend = pane["agent_status"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if !backend.is_empty() && backend != "unknown" {
            return json!({"state": backend, "source": "terminal backend",
                          "reason": Value::Null, "inferred": false});
        }
        let Some(pane_id) = pane["pane_id"].as_str().filter(|s| !s.is_empty()) else {
            return json!({"state": Value::Null, "source": "none",
                          "reason": "no activity source: pane identity missing", "inferred": true});
        };
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(pane_id)
            .cloned();
        let harness = pane["agent"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("lane");
        let (derived, _since, reason) =
            activity::turn_state(turn.as_ref(), now, IDLE_CLAIM_S, harness);
        json!({"state": derived, "source": format!("{harness} declared turns"),
               "reason": reason, "inferred": true})
    }

    /// `lane_status`: just the state.
    pub fn lane_status(&self, pane: &Value, now: f64) -> Value {
        self.lane_activity(pane, now)["state"].clone()
    }

    /// `resolve_lanes`: every pane with an id, as the dispatcher's lane rows.
    pub fn resolve_lanes(&self, env: &dyn MailboxEnv, now: f64) -> Result<Vec<Value>, DunkError> {
        let text = |pane: &Value, key: &str| {
            pane[key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Ok(env
            .list_panes_grouped()
            .iter()
            .filter_map(|pane| {
                let id = text(pane, "pane_id")?;
                let name = text(pane, "agent_name")
                    .or_else(|| text(pane, "tab_label"))
                    .or_else(|| text(pane, "label"))
                    .unwrap_or_else(|| id.clone());
                Some(json!({
                    "pane_id": id,
                    "pane_pid": pane["pane_pid"],
                    "agent": pane["agent"],
                    "cwd": pane["cwd"],
                    "agent_name": name,
                    "tab_label": pane["tab_label"],
                    "agent_status": self.lane_status(pane, now),
                }))
            })
            .collect())
    }
}
