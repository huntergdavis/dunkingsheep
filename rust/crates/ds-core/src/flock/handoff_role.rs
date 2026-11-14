//! t4697: which lanes the next-job handoff may serve without being listed by hand.
//!
//! A pane is eligible when its role registration is LIVE (the registered root pid and start
//! time are the pane's current process), the role is worker or reviewer, the owner has not
//! excluded it, and the pane has a project or global membership bound to that same process.
//! Anything unreadable is "not eligible": a failed read never becomes permission.
use serde_json::Value;

use super::{panes::PaneInfoEnv, Flock};

/// Roles the handoff serves by registration. Planner, desk and owner are never served this way.
pub(crate) const HANDOFF_ROLES: [&str; 2] = ["worker", "reviewer"];

/// The gate's lane snapshot as the pane source, so one decision reads one listing.
struct Snapshot<'a>(&'a [Value]);

impl PaneInfoEnv for Snapshot<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.0.to_vec()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.0
            .iter()
            .find(|lane| lane.get("pane_id").and_then(Value::as_str) == Some(pane_id))
            .cloned()
    }
}

fn int(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

impl Flock {
    /// The registered role of `pane` when the registration still names the pane's running
    /// process. `None` for no registration, a registration without a recorded pid and start,
    /// a pane whose root pid changed, or a pid whose start time no longer matches.
    pub(crate) fn live_role_binding(&self, pane: &str, lanes: &[Value]) -> Option<String> {
        let entry = self.with_roles(|roles, _| roles.entry(pane)).ok()??;
        let stored_pid = int(entry.get("pane_pid")).filter(|pid| *pid > 0)?;
        let stored_start = entry
            .get("start")
            .and_then(Value::as_str)
            .filter(|start| !start.is_empty())?;
        let live_pid = int(Snapshot(lanes).get_pane(pane)?.get("pane_pid"))?;
        if live_pid != stored_pid || !crate::proc::process_alive(stored_pid, Some(stored_start)) {
            return None;
        }
        entry.get("role").and_then(Value::as_str).map(str::to_owned)
    }

    /// Whether the next-job handoff may serve `pane` on its role registration alone.
    /// `exclude` is the owner's list of role names, pane ids or agent names to leave out.
    pub(crate) fn handoff_role_eligible(
        &self,
        pane: &str,
        lanes: &[Value],
        exclude: &[String],
    ) -> bool {
        let Some(role) = self.live_role_binding(pane, lanes) else {
            return false;
        };
        if !HANDOFF_ROLES.contains(&role.as_str()) {
            return false;
        }
        let snapshot = Snapshot(lanes);
        let name = snapshot
            .get_pane(pane)
            .and_then(|lane| {
                lane.get("agent_name")
                    .and_then(Value::as_str)
                    .map(str::to_lowercase)
            })
            .unwrap_or_default();
        let excluded = exclude.iter().any(|entry| {
            let entry = entry.trim().to_lowercase();
            !entry.is_empty() && (entry == role || entry == pane.to_lowercase() || entry == name)
        });
        if excluded {
            return false;
        }
        matches!(self.lane_project(&snapshot, pane), Ok(Some(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::Roles;
    use crate::tasks::lane_project::ProjectScope;
    use serde_json::json;
    use std::sync::Arc;

    fn me() -> (i64, String) {
        let pid = i64::from(std::process::id());
        (
            pid,
            crate::proc::process_start(pid).expect("own start time"),
        )
    }

    /// A flock with a task store and a roles registry; `%N` panes all run this test process.
    async fn fixture(registrations: &[(&str, &str)]) -> (tempfile::TempDir, Flock, Vec<Value>) {
        let dir = crate::testsupport::dir("handoff-role");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let tasks = Arc::new(
            crate::tasks::Tasks::open(dir.path().join("tasks.db"))
                .await
                .unwrap(),
        );
        flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(tasks),
            ..Default::default()
        });
        let mut roles = Roles::new(
            &crate::config::Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        let (pid, start) = me();
        let mut lanes = Vec::new();
        for (pane, role) in registrations {
            roles
                .register(pane, role, Some(pid), Some(&start), None, None, None, None)
                .unwrap();
            lanes.push(
                json!({"pane_id": pane, "pane_pid": pid, "agent_name": format!("lane-{role}")}),
            );
        }
        flock.attach_roles(roles, None);
        for (pane, _) in registrations {
            flock
                .lane_project_set(
                    &Snapshot(&lanes),
                    pane,
                    ProjectScope::Project("dunkingsheep".into()),
                )
                .unwrap();
        }
        (dir, flock, lanes)
    }

    #[tokio::test]
    async fn a_live_worker_or_reviewer_with_a_membership_is_eligible() {
        let (_dir, flock, lanes) = fixture(&[("%1", "worker"), ("%2", "reviewer")]).await;
        assert!(flock.handoff_role_eligible("%1", &lanes, &[]));
        assert!(flock.handoff_role_eligible("%2", &lanes, &[]));
        assert_eq!(
            flock.live_role_binding("%2", &lanes).as_deref(),
            Some("reviewer")
        );
    }

    #[tokio::test]
    async fn a_global_membership_counts_and_no_membership_does_not() {
        let (_dir, flock, mut lanes) = fixture(&[("%1", "worker")]).await;
        flock
            .lane_project_set(&Snapshot(&lanes), "%1", ProjectScope::Global)
            .unwrap();
        assert!(flock.handoff_role_eligible("%1", &lanes, &[]));
        // A registered worker that was never bound to a project or to global.
        let (pid, start) = me();
        flock
            .with_roles(|roles, _| {
                roles.register(
                    "%9",
                    "worker",
                    Some(pid),
                    Some(&start),
                    None,
                    None,
                    None,
                    None,
                )
            })
            .unwrap();
        lanes.push(json!({"pane_id": "%9", "pane_pid": pid}));
        assert_eq!(
            flock.live_role_binding("%9", &lanes).as_deref(),
            Some("worker")
        );
        assert!(!flock.handoff_role_eligible("%9", &lanes, &[]));
    }

    #[tokio::test]
    async fn planner_desk_owner_and_unregistered_panes_are_not_eligible() {
        let (_dir, flock, mut lanes) =
            fixture(&[("%1", "planner"), ("%2", "desk"), ("%3", "owner")]).await;
        for pane in ["%1", "%2", "%3"] {
            assert!(flock.live_role_binding(pane, &lanes).is_some());
            assert!(!flock.handoff_role_eligible(pane, &lanes, &[]), "{pane}");
        }
        lanes.push(json!({"pane_id": "%4", "pane_pid": me().0}));
        assert!(!flock.handoff_role_eligible("%4", &lanes, &[]));
    }

    #[tokio::test]
    async fn a_stale_binding_is_refused_on_pid_or_start_mismatch() {
        let (_dir, flock, lanes) = fixture(&[("%1", "worker")]).await;
        let (pid, start) = me();
        // The pane id was reused by another process.
        let reused = vec![json!({"pane_id": "%1", "pane_pid": 1})];
        assert!(!flock.handoff_role_eligible("%1", &reused, &[]));
        // The pane is gone from the listing.
        assert!(!flock.handoff_role_eligible("%1", &[], &[]));
        // Same pid, another start time: the pid was reused.
        flock
            .with_roles(|roles, _| {
                roles.register("%1", "worker", Some(pid), Some("1"), None, None, None, None)
            })
            .unwrap();
        assert!(flock.live_role_binding("%1", &lanes).is_none());
        assert!(!flock.handoff_role_eligible("%1", &lanes, &[]));
        // A registration that recorded no process cannot be verified.
        flock
            .with_roles(|roles, _| {
                roles.register("%1", "worker", None, None, None, None, None, None)
            })
            .unwrap();
        assert!(!flock.handoff_role_eligible("%1", &lanes, &[]));
        // Re-registered by the running process: eligible again.
        flock
            .with_roles(|roles, _| {
                roles.register(
                    "%1",
                    "worker",
                    Some(pid),
                    Some(&start),
                    None,
                    None,
                    None,
                    None,
                )
            })
            .unwrap();
        assert!(flock.handoff_role_eligible("%1", &lanes, &[]));
    }

    #[tokio::test]
    async fn the_owner_exclusion_removes_a_role_a_pane_or_a_named_lane() {
        let (_dir, flock, lanes) = fixture(&[("%1", "worker"), ("%2", "reviewer")]).await;
        let list = |entries: &[&str]| entries.iter().map(|e| e.to_string()).collect::<Vec<_>>();
        assert!(!flock.handoff_role_eligible("%2", &lanes, &list(&["Reviewer"])));
        assert!(flock.handoff_role_eligible("%1", &lanes, &list(&["reviewer"])));
        assert!(!flock.handoff_role_eligible("%1", &lanes, &list(&["%1"])));
        assert!(flock.handoff_role_eligible("%2", &lanes, &list(&["%1"])));
        assert!(!flock.handoff_role_eligible("%1", &lanes, &list(&["LANE-WORKER"])));
        assert!(flock.handoff_role_eligible("%1", &lanes, &list(&["", " "])));
    }

    #[tokio::test]
    async fn no_role_registry_is_not_eligible() {
        let dir = crate::testsupport::dir("handoff-role-none");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let lanes = vec![json!({"pane_id": "%1", "pane_pid": me().0})];
        assert!(!flock.handoff_role_eligible("%1", &lanes, &[]));
    }
}
