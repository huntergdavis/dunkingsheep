//! `Flock.delivery_target` and `Flock.delivery_capability` (T38, slice 4): resolve a
//! DIRECT-DELIVERY target (owner push, keystrokes) to one live pane with the tokenised
//! refusals the desk renders (`dead_lane`, `unhoused_lane`, `unknown_name`), and say how the
//! desk may deliver there and what it may show.
//!
//! The environment is a trait (`DeliveryEnv`): the Python reads `self.herdr`, the process
//! table, and `self.roles`; none of those is a field of the Rust `Flock` yet. NOT ported
//! here: `_desk_send_rights` beyond the token-exists test (the Python also reads only that),
//! and the `_invalidate_view` cache.

use serde_json::{json, Value};

use super::mailbox::{is_id, MailboxEnv};
use super::Flock;
use crate::error::DunkError;
use crate::owner::OwnerPush;

/// `Flock.DRAIN_HOOKS`: which harness drains an owner-push queue.
pub const DRAIN_HOOKS: [(&str, &str); 1] = [("pi", "owner_push")];
/// `Flock.DRAIN_FRESH_S`: a poll older than this is not evidence of a running drainer.
pub const DRAIN_FRESH_S: f64 = 300.0;

/// What `delivery_target` and `delivery_capability` read from the outside.
pub trait DeliveryEnv: MailboxEnv {
    /// `herdr.get_pane(spec)`: one live pane as the backend's dict, or None.
    fn get_pane(&self, spec: &str) -> Option<Value>;
    /// `_lane_agent_alive`: Some(true) agent process, Some(false) bare shell, None cannot say.
    fn agent_alive(&self, pane_id: &str) -> Option<bool>;
    /// `any(t.get("role") == "web" for t in roles.tokens())`; Err is "capability never crashes".
    fn has_web_token(&self) -> Result<bool, DunkError>;
    /// `herdr.server_error_hint` for the refusal text.
    fn server_error_hint(&self) -> String;
}

fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn text(pane: &Value, key: &str) -> String {
    pane.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

impl Flock {
    /// `delivery_target`: resolve a target to ONE live pane, failing closed.
    pub fn delivery_target(
        &self,
        env: &dyn DeliveryEnv,
        target: &str,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        let spec = target.trim();
        if spec.is_empty() {
            return Err(DunkError::Refused(
                "a delivery target is required (a pane id like %9 or a pinned lane name)".into(),
            ));
        }
        if is_id(spec) {
            return env.get_pane(spec).ok_or_else(|| {
                DunkError::Refused(format!(
                    "no live pane {}: this terminal is gone. reason=dead_lane.",
                    py_repr(spec)
                ))
            });
        }
        if let Some(pane) = env.get_pane(spec) {
            return Ok(pane); // a bare '%4'
        }
        let (key, _label, pane_id) =
            self.resolve_mailbox(env, spec, self_pane_id, true, &env.server_error_hint())?;
        let Some(pane_id) = pane_id else {
            return Err(DunkError::Refused(format!(
                "lane {} resolved but has no live pane to deliver to. reason=unhoused_lane.",
                py_repr(&key)
            )));
        };
        env.get_pane(&pane_id).ok_or_else(|| {
            DunkError::Refused(format!(
                "no live pane {}: this terminal is gone. reason=dead_lane.",
                py_repr(&pane_id)
            ))
        })
    }

    /// `delivery_capability`: how the desk may deliver to this target. `drains` is OBSERVED
    /// (a poll heartbeat inside `DRAIN_FRESH_S`), never inferred from the harness name: a
    /// manual `owner take` must not confer capability, and an idle poller draining empty
    /// queues must not lose it.
    pub fn delivery_capability(
        &self,
        env: &dyn DeliveryEnv,
        owner: &OwnerPush,
        target: &str,
        self_pane_id: Option<&str>,
        now: f64,
    ) -> Result<Value, DunkError> {
        let pane = self.delivery_target(env, target, self_pane_id)?;
        let pane_id = text(&pane, "pane_id");
        let agent = text(&pane, "agent").trim().to_lowercase();
        let registry = self.lane_registry(false).unwrap_or_default();
        let (name, declared) = Flock::row_identity(registry.get(&pane_id));
        let drain = DRAIN_HOOKS.iter().any(|(harness, _)| *harness == agent);
        let (evidence, last) = match owner.last_poll(&pane_id) {
            Ok(Some(at)) if at != 0.0 => {
                let age = (now - at) as i64;
                (
                    if age < 86400 { format!("harness polled {age}s ago") } else { "harness last polled over a day ago".to_string() },
                    Some(at),
                )
            }
            Ok(_) if drain => (
                "harness has a poller but has never polled this pane (a manual owner take does not count)".to_string(),
                None,
            ),
            Ok(_) => ("no poll observed".to_string(), None),
            Err(_) => ("drain evidence unavailable".to_string(), None),
        };
        let observed = last.is_some_and(|at| now - at < DRAIN_FRESH_S);
        let label = text(&pane, "label");
        let (has_web, _) = match env.has_web_token() {
            Ok(v) => (v, ()),
            Err(_) => (false, ()),
        };
        Ok(json!({
            "pane_id": pane_id,
            "mailbox": name.unwrap_or_else(|| if label.is_empty() { pane_id.clone() } else { label }),
            "declared": declared,
            "agent": if agent.is_empty() { Value::Null } else { json!(agent) },
            "alive": true,
            // PROCESS evidence, not the backend's `agent` string (empty for every tmux pane).
            "agent_alive": env.agent_alive(&pane_id),
            "drains": observed,
            "drain_evidence": evidence,
            // push only when a drain has ACTUALLY been seen recently.
            "send": if observed { "owner_push" } else { "guarded" },
            "expect": if observed {
                "push drained by the lane's own poller, no inbox row"
            } else {
                "immediate guarded send (Enter, then C-m); 'sent' means it left the box"
            },
            "desk_credential": has_web,
            "desk_can_type": has_web,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Env {
        panes: Vec<Value>,
        alive: BTreeMap<String, Option<bool>>,
        web: bool,
    }

    impl MailboxEnv for Env {
        fn list_panes(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
            (false, false, None)
        }
    }

    impl DeliveryEnv for Env {
        fn get_pane(&self, spec: &str) -> Option<Value> {
            self.panes.iter().find(|p| p["pane_id"] == spec).cloned()
        }
        fn agent_alive(&self, pane_id: &str) -> Option<bool> {
            self.alive.get(pane_id).copied().flatten()
        }
        fn has_web_token(&self) -> Result<bool, DunkError> {
            Ok(self.web)
        }
        fn server_error_hint(&self) -> String {
            "tmux server not running".into()
        }
    }

    fn pane(id: &str, agent: &str, label: &str) -> Value {
        json!({"pane_id": id, "agent": agent, "label": label, "tab_label": label, "workspace_label": "w", "cwd": "/tmp", "agent_status": "idle"})
    }

    fn setup(tag: &str, env_panes: Vec<Value>) -> (Flock, Env, OwnerPush) {
        let dir = std::env::temp_dir().join(format!("ds-delivery-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), true);
        let owner = OwnerPush::open(dir.join("owner-push.db")).unwrap();
        (
            flock,
            Env {
                panes: env_panes,
                ..Env::default()
            },
            owner,
        )
    }

    const NOW: f64 = 10_000.0;

    #[test]
    fn capability_reads_an_observed_poll_not_a_harness_name() {
        let (flock, env, owner) = setup("poll", vec![pane("%7", "pi", "pi lane")]);
        flock.remember_lane("%7", "dunky pi lane", true);
        let before = flock
            .delivery_capability(&env, &owner, "dunky pi lane", None, NOW)
            .unwrap();
        assert_eq!(before["drains"], false, "{}", before["drain_evidence"]);
        assert_eq!(before["send"], "guarded");
        assert_eq!(before["drain_evidence"], "harness has a poller but has never polled this pane (a manual owner take does not count)");
        owner.poll("%7", Some("pi-extension"), NOW - 5.0).unwrap(); // an EMPTY poll still counts
        let after = flock
            .delivery_capability(&env, &owner, "dunky pi lane", None, NOW)
            .unwrap();
        assert_eq!(after["drains"], true);
        assert_eq!(after["send"], "owner_push");
        assert_eq!(after["drain_evidence"], "harness polled 5s ago");
        assert_eq!(
            after["expect"],
            "push drained by the lane's own poller, no inbox row"
        );
        // A manual take of nothing must not confer capability, and a stale poll is not drained.
        let stale = flock
            .delivery_capability(&env, &owner, "%7", None, NOW + 400.0)
            .unwrap();
        assert_eq!(stale["drains"], false);
        assert_eq!(stale["drain_evidence"], "harness polled 405s ago");
        let old = flock
            .delivery_capability(&env, &owner, "%7", None, NOW + 90_000.0)
            .unwrap();
        assert_eq!(old["drain_evidence"], "harness last polled over a day ago");
    }

    #[test]
    fn push_or_guarded_per_harness_and_the_declared_name_is_shown() {
        let (flock, env, owner) = setup(
            "harness",
            vec![
                pane("%7", "pi", "pi lane"),
                pane("%8", "claude", "claude lane"),
            ],
        );
        flock.remember_lane("%7", "dunky", true);
        flock.remember_lane("%8", "PM (par-a)", true);
        owner.poll("%7", Some("pi-extension"), NOW).unwrap();
        let a = flock
            .delivery_capability(&env, &owner, "dunky", None, NOW)
            .unwrap();
        assert_eq!(
            (
                a["drains"].as_bool(),
                a["send"].as_str(),
                a["mailbox"].as_str()
            ),
            (Some(true), Some("owner_push"), Some("dunky"))
        );
        assert_eq!(a["declared"], true);
        assert_eq!(a["agent"], "pi");
        let b = flock
            .delivery_capability(&env, &owner, "PM (par-a)", None, NOW)
            .unwrap();
        assert_eq!(
            (b["drains"].as_bool(), b["send"].as_str()),
            (Some(false), Some("guarded"))
        );
        assert_eq!(b["drain_evidence"], "no poll observed");
    }

    #[test]
    fn a_target_that_does_not_resolve_raises_the_tokenised_refusals() {
        let (flock, env, owner) = setup("refuse", vec![pane("%7", "pi", "lane")]);
        let dead = flock
            .delivery_capability(&env, &owner, "%99", None, NOW)
            .unwrap_err()
            .to_string();
        assert_eq!(
            dead,
            "no live pane '%99': this terminal is gone. reason=dead_lane."
        );
        let unknown = flock
            .delivery_capability(&env, &owner, "nobody-at-all", None, NOW)
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("reason=unknown_name"), "{unknown}");
        let empty = flock
            .delivery_capability(&env, &owner, "  ", None, NOW)
            .unwrap_err()
            .to_string();
        assert_eq!(
            empty,
            "a delivery target is required (a pane id like %9 or a pinned lane name)"
        );
    }

    #[test]
    fn capability_says_whether_the_desk_credential_covers_this_pane() {
        let (flock, mut env, owner) = setup("desk", vec![pane("%7", "claude", "guarded lane")]);
        let before = flock
            .delivery_capability(&env, &owner, "%7", None, NOW)
            .unwrap();
        assert_eq!(
            (
                before["desk_credential"].as_bool(),
                before["desk_can_type"].as_bool()
            ),
            (Some(false), Some(false))
        );
        env.web = true;
        let after = flock
            .delivery_capability(&env, &owner, "%7", None, NOW)
            .unwrap();
        assert_eq!(
            (
                after["desk_credential"].as_bool(),
                after["desk_can_type"].as_bool()
            ),
            (Some(true), Some(true))
        );
    }

    #[test]
    fn capability_reports_process_evidence_not_the_backend_agent_string() {
        let (flock, mut env, owner) = setup("alive", vec![pane("%7", "", "lane")]);
        for (state, want) in [
            (Some(true), json!(true)),
            (Some(false), json!(false)),
            (None, Value::Null),
        ] {
            env.alive.insert("%7".into(), state);
            let got = flock
                .delivery_capability(&env, &owner, "%7", None, NOW)
                .unwrap();
            assert_eq!(got["agent_alive"], want);
        }
        assert_eq!(
            flock
                .delivery_capability(&env, &owner, "%7", None, NOW)
                .unwrap()["agent"],
            Value::Null,
            "an empty agent string is null"
        );
    }
}
