//! Reviewer eligibility and process evidence used by the task push loop.

use crate::server_runtime::LoopRuntimeAdapter;

impl LoopRuntimeAdapter {
    /// `may_review(pane, role)` with the caller's resolved role; never re-resolved here.
    pub(crate) fn helper_pushenv_may_review(&self, pane_id: &str, role: &str) -> bool {
        self.flock
            .with_roles(|roles, _| roles.may_review(pane_id, Some(role)))
            .unwrap_or(false)
    }

    pub(crate) fn helper_pushenv_provider_of(&self, pane_id: &str) -> Option<String> {
        self.flock
            .with_roles(|roles, _| roles.provider_of(pane_id))
            .ok()
            .flatten()
    }

    pub(crate) fn helper_pushenv_lane_agent_alive(&self, pane_id: &str) -> Option<bool> {
        // Registrations can outlive the pane process. Python always resolves the
        // current pane before inspecting its process tree, including followers.
        let terminal_factory = std::sync::Arc::clone(&self.terminal_factory);
        let pane_id = pane_id.to_owned();
        let pane_pid = std::thread::Builder::new()
            .name("push-lane-pane-info".into())
            .spawn(move || {
                (terminal_factory)()
                    .get_pane(&pane_id)
                    .and_then(|pane| pane.pane_pid)
            })
            .ok()?
            .join()
            .ok()
            .flatten()?;
        super::delivery::lane_agent_alive_for_pane_pid(pane_pid)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::Env;
    use crate::flock::loops_push::PushEnv;
    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::roles::Roles;
    use crate::tasks::{Task, Tasks};
    use crate::terminal::fakes::FakeTerminal;

    struct PushEligibilityEnv<'a> {
        adapter: &'a LoopRuntimeAdapter,
        pane: serde_json::Value,
    }

    use crate::flock::loop_env::LoopFuture;

    impl PushEnv for PushEligibilityEnv<'_> {
        fn list_panes<'a>(
            &'a self,
        ) -> LoopFuture<'a, Result<Vec<serde_json::Value>, crate::error::DunkError>> {
            Box::pin(async move { Ok(vec![self.pane.clone()]) })
        }

        fn caller_role(&self, pane_id: &str, pane_info: Option<&serde_json::Value>) -> String {
            self.adapter.helper_pushenv_caller_role(pane_id, pane_info)
        }

        fn registered_lanes(&self) -> Option<std::collections::BTreeSet<String>> {
            self.adapter.helper_pushenv_registered_lanes()
        }

        fn may_review(&self, pane_id: &str, role: &str) -> bool {
            self.adapter.helper_pushenv_may_review(pane_id, role)
        }

        fn provider_of(&self, pane_id: &str) -> Option<String> {
            self.adapter.helper_pushenv_provider_of(pane_id)
        }

        fn lane_agent_alive(&self, pane_id: &str) -> Option<bool> {
            self.adapter.helper_pushenv_lane_agent_alive(pane_id)
        }

        fn unread_mail<'a>(&'a self, _: &'a str) -> LoopFuture<'a, bool> {
            Box::pin(async move { false })
        }

        fn announce_task<'a>(&'a self, _: &'a str, _: &'a Task, _: f64) -> LoopFuture<'a, ()> {
            Box::pin(async move {})
        }
    }

    #[test]
    fn production_env_push_eligibility_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-push-eligibility");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .expect("roles store");
        roles
            .register(
                "w1:p1",
                "reviewer",
                None,
                None,
                Some("CLAUDE"),
                None,
                None,
                None,
            )
            .expect("register allowed reviewer");
        roles
            .register(
                "w2:p2",
                "reviewer",
                None,
                None,
                Some("nvidia"),
                None,
                None,
                None,
            )
            .expect("register ineligible reviewer");
        roles
            .register(
                "w3:p3",
                "worker",
                Some(i64::from(std::process::id())),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("register live process root");
        flock.attach_roles(roles, None);

        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        assert!(adapter.helper_pushenv_may_review("w1:p1", "reviewer"));
        assert_eq!(
            adapter.helper_pushenv_provider_of("w1:p1"),
            Some("claude".into())
        );
        assert!(!adapter.helper_pushenv_may_review("w2:p2", "reviewer"));
        assert_eq!(
            adapter.helper_pushenv_provider_of("w2:p2"),
            Some("nvidia".into())
        );
        assert_eq!(adapter.helper_pushenv_provider_of("w9:p9"), None);

        // A registered root with an available process table and no agent evidence is dead;
        // an unknown pane has no PID evidence and remains unknown.
        assert_eq!(
            adapter.helper_pushenv_lane_agent_alive("w3:p3"),
            Some(false)
        );
        assert!(adapter.helper_pushenv_lane_agent_alive("w1:p1").is_some());
        assert_eq!(adapter.helper_pushenv_lane_agent_alive("w9:p9"), None);

        let unregistered_dir = crate::testsupport::dir("runtime-push-eligibility-unregistered");
        let unregistered_flock = Arc::new(Flock::new(
            unregistered_dir.path().join("launches.json"),
            unregistered_dir.path().join("lanes.json"),
            false,
        ));
        let unregistered_roles = Roles::new(
            &Env::default(),
            Some(unregistered_dir.path().join("roles.json")),
            Some(unregistered_dir.path().join("tokens.json")),
            None,
        )
        .expect("empty roles store");
        unregistered_flock.attach_roles(unregistered_roles, None);
        let unregistered_adapter = LoopRuntimeAdapter::new(
            unregistered_flock,
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        assert!(unregistered_adapter
            .helper_pushenv_lane_agent_alive("w1:p1")
            .is_some());

        let active_agent = [crate::push::ChildProc {
            comm: "codex".into(),
            state: "S".into(),
        }];
        assert_eq!(
            crate::push::lane_agent_alive(Some(&active_agent), || None),
            Some(true)
        );
        assert_eq!(crate::push::lane_agent_alive(None, || None), None);
    }

    #[tokio::test]
    async fn production_env_push_skips_following_pane_after_role_unregister() {
        let dir = crate::testsupport::dir("runtime-push-eligibility-unregistered-follower");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .expect("roles store");
        roles
            .register("w1:p1", "worker", None, None, None, None, None, None)
            .expect("register pane");
        flock.attach_roles(roles, None);

        let (scope, _) = flock.task_follow(None, Some("w1:p1")).expect("follow pane");
        assert_eq!(scope, "global");
        flock
            .with_roles(|roles, _| roles.unregister("w1:p1"))
            .expect("role store")
            .expect("unregister pane");
        assert_eq!(
            flock.task_followers(),
            vec![("w1:p1".into(), "global".into())]
        );

        let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        let task = tasks.add("global", "waiting work", "").await.expect("task");

        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.panes[0].pane_pid = Some(9_999_999_999);
                Box::new(terminal)
            }),
        );
        let env = PushEligibilityEnv {
            adapter: &adapter,
            pane: serde_json::json!({
                "pane_id": "w1:p1",
                "agent_status": "idle",
            }),
        };

        assert_eq!(
            adapter.helper_pushenv_lane_agent_alive("w1:p1"),
            Some(false)
        );
        // Owner decision on t3554: only a subscriber is a target, so the lane subscribes first.
        flock
            .task_waiters
            .lock()
            .unwrap()
            .insert("w1:p1".into(), "global".into());
        let decisions = flock.push_tasks(&env).await;
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, "skip");
        assert_eq!(decisions[0].detail, "no_live_agent");
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn production_pushenv_liveness_uses_current_pane_not_registered_pid() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let dir = crate::testsupport::dir("push-current-pane-pid");
        // A test-owned process named like an agent, never a live terminal pane.
        let executable = dir.path().join("codex");
        std::os::unix::fs::symlink("/bin/sleep", &executable).unwrap();
        let child = ChildGuard(
            std::process::Command::new(executable)
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let agent_pid = i64::from(child.0.id());
        assert_eq!(
            super::super::delivery::lane_agent_alive_for_pane_pid(agent_pid),
            Some(true)
        );
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(
                "w1:p1",
                "worker",
                Some(agent_pid),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        flock.attach_roles(roles, None);
        flock.task_follow(None, Some("w1:p1")).unwrap();
        let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        let task = tasks
            .add("global", "must not feed a reused shell", "")
            .await
            .unwrap();
        let current_pid = Arc::new(std::sync::Mutex::new(Some(9_999_999_999_i64)));
        let factory_pid = Arc::clone(&current_pid);
        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(move || {
                let mut terminal = FakeTerminal::new();
                terminal.panes[0].pane_pid = *factory_pid.lock().unwrap();
                Box::new(terminal)
            }),
        );
        let env: &dyn PushEnv = &adapter;
        assert_eq!(
            env.lane_agent_alive("w1:p1"),
            Some(false),
            "old live registration must not conceal a dead current pane"
        );
        flock
            .task_waiters
            .lock()
            .unwrap()
            .insert("w1:p1".into(), "global".into());
        let decisions = flock.push_tasks(env).await;
        assert!(decisions
            .iter()
            .any(|d| d.pane_id == "w1:p1" && d.detail == "no_live_agent"));
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");

        flock
            .with_roles(|roles, _| {
                roles.register(
                    "w1:p1",
                    "worker",
                    Some(9_999_999_999),
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            })
            .unwrap();
        *current_pid.lock().unwrap() = Some(agent_pid);
        assert_eq!(
            env.lane_agent_alive("w1:p1"),
            Some(true),
            "old dead registration must not suppress the current live agent"
        );
        *current_pid.lock().unwrap() = None;
        assert_eq!(
            env.lane_agent_alive("w1:p1"),
            None,
            "missing current PID remains unknown"
        );
        assert_eq!(env.lane_agent_alive("missing-pane"), None);
    }
}

#[cfg(test)]
#[path = "push_reviewer_parity_tests.rs"]
mod push_reviewer_parity_tests;
