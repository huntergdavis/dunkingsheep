//! `Flock.lanes` and `Flock.lane_set` (T38, slice 3; `lanes.rs` is T30's resolve_lanes): the lane registry as rows, and the
//! repair path that declares who a pane IS after the fact.
//!
//! NOT ported here: `_invalidate_view` (the Python drops a cached resolve view after
//! `lane_set`; the Rust Flock has no such cache yet, so there is nothing to drop), and
//! `lane_register`/`lane_unregister`.

use serde_json::{json, Value};

use super::Flock;
use crate::error::DunkError;
use crate::terminal::Terminal;

impl Flock {
    /// `lanes()`: the registry as rows - pane id, mailbox, declared?, alive?, dead_since.
    /// Sorted by pane id, as Python's `sorted(items)` does.
    pub fn lanes(&self) -> Vec<Value> {
        let registry = self.lane_registry(false).unwrap_or_default();
        let mut keys: Vec<&String> = registry.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|pane_id| {
                let value = &registry[pane_id];
                let (name, declared) = Flock::row_identity(Some(value));
                let (alive, dead_since) = match value {
                    Value::Object(row) => (
                        // `bool(value.get("alive", True))`: absent means alive.
                        json!(row.get("alive").map(super::truthy).unwrap_or(true)),
                        row.get("dead_since").cloned().unwrap_or(Value::Null),
                    ),
                    _ => (Value::Null, Value::Null),
                };
                json!({"pane_id": pane_id, "mailbox": name, "declared": declared, "alive": alive, "dead_since": dead_since})
            })
            .collect()
    }

    /// `lane_set`: declare who a pane IS. Superuser-gated by the caller. Refuses a pane that
    /// does not exist, so a typo cannot mint a lane.
    pub fn lane_set(
        &self,
        terminal: &dyn Terminal,
        pane_id: &str,
        mailbox: &str,
    ) -> Result<Value, DunkError> {
        let (pane_id, mailbox) = (pane_id.trim(), mailbox.trim());
        if pane_id.is_empty() || mailbox.is_empty() {
            return Err(DunkError::Refused(
                "lane set must be given a pane id and a mailbox name".into(),
            ));
        }
        if terminal.get_pane(pane_id).is_none() {
            return Err(DunkError::Refused(format!(
                "no live pane {}: a lane row for a dead pane would be a guess. Create the pane first.",
                py_repr(pane_id)
            )));
        }
        self.remember_lane(pane_id, mailbox, true);
        let registry = self.lane_registry(false).unwrap_or_default();
        let (name, _) = Flock::row_identity(registry.get(pane_id));
        Ok(json!({"pane_id": pane_id, "mailbox": name, "declared": true}))
    }
}

/// Python `repr(str)`.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::Pane;

    fn flock(tag: &str) -> crate::testsupport::Scratch<Flock> {
        let dir = crate::testsupport::dir(&format!("lanes-{tag}"));
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            true,
        );
        crate::testsupport::Scratch::new(flock, dir)
    }

    fn term_with(pane: &str) -> FakeTerminal {
        let mut term = FakeTerminal::new();
        term.panes.push(Pane {
            pane_id: pane.into(),
            ..Pane::default()
        });
        term
    }

    #[test]
    fn lane_set_declares_a_live_pane_and_refuses_a_dead_one() {
        let f = flock("set");
        let term = term_with("%7");
        let out = f.lane_set(&term, " %7 ", " par-b ").unwrap();
        assert_eq!(
            out,
            json!({"pane_id": "%7", "mailbox": "par-b", "declared": true})
        );
        let err = f.lane_set(&term, "%99", "ghost").unwrap_err().to_string();
        assert_eq!(err, "no live pane '%99': a lane row for a dead pane would be a guess. Create the pane first.");
        assert!(
            f.lanes().iter().all(|r| r["pane_id"] != "%99"),
            "a typo did not mint a lane"
        );
        for (p, m) in [("", "x"), ("%7", "  ")] {
            assert_eq!(
                f.lane_set(&term, p, m).unwrap_err().to_string(),
                "lane set must be given a pane id and a mailbox name"
            );
        }
    }

    #[test]
    fn a_declared_name_survives_an_inferred_one() {
        let f = flock("declared");
        let term = term_with("%7");
        f.lane_set(&term, "%7", "par-b").unwrap();
        assert!(
            !f.remember_lane("%7", "claude-conversation-title", false),
            "an inferred name never overwrites a declared one"
        );
        assert_eq!(
            f.lane_set(&term, "%7", "par-b-2").unwrap()["mailbox"],
            "par-b-2",
            "lane set re-declares"
        );
    }

    #[test]
    fn lanes_lists_rows_sorted_with_alive_defaulting_true() {
        let f = flock("rows");
        let mut data = serde_json::Map::new();
        data.insert("%9".into(), json!("plain-name"));
        data.insert(
            "%10".into(),
            json!({"mailbox": "pm", "declared": true, "alive": false, "dead_since": 5.0}),
        );
        data.insert("%2".into(), json!({"mailbox": "w"}));
        f.write_lane_registry(&data);
        let rows = f.lanes();
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| r["pane_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["%10", "%2", "%9"],
            "string order, as Python's sorted()"
        );
        assert_eq!(
            rows[0],
            json!({"pane_id": "%10", "mailbox": "pm", "declared": true, "alive": false, "dead_since": 5.0})
        );
        assert_eq!(rows[1]["alive"], true);
        assert_eq!(
            rows[2],
            json!({"pane_id": "%9", "mailbox": "plain-name", "declared": false, "alive": null, "dead_since": null})
        );
    }
}
