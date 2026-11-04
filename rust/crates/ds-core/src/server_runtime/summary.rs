//! Production lane and task sources for the project summary view.

use serde_json::{json, Value};

use crate::flock::project_summary::{absolute_summary_path, expand_summary_path, SummaryEnv};
use crate::server_runtime::RequestRuntimeAdapter;

fn named_home(user: &str) -> Result<Option<std::path::PathBuf>, crate::error::DunkError> {
    nix::unistd::User::from_name(user)
        .map(|entry| entry.map(|entry| entry.dir))
        .map_err(|error| crate::error::DunkError::Refused(error.to_string()))
}

impl SummaryEnv for RequestRuntimeAdapter<'_> {
    fn global_context_root(&self) -> Result<std::path::PathBuf, crate::error::DunkError> {
        let state = self.terminal.flock.status_state()?;
        let root = state
            .python_home
            .config_dir(&state.env)
            .map_err(crate::error::DunkError::Refused)?
            .join("context");
        let home = state
            .python_home
            .path()
            .map_err(crate::error::DunkError::Refused)?;
        expand_summary_path(&root, Some(home), true, named_home)
    }

    fn project_dir(&self, raw: &str) -> Result<std::path::PathBuf, crate::error::DunkError> {
        let state = self.terminal.flock.status_state()?;
        let home = if raw == "~" || raw.starts_with("~/") {
            state
                .python_home
                .expanduser_home()
                .map_err(crate::error::DunkError::Refused)?
        } else {
            None
        };
        let expanded = expand_summary_path(std::path::Path::new(raw), home, false, named_home)?;
        Ok(absolute_summary_path(&expanded, &std::env::current_dir()?))
    }

    fn merges(&self, repo: &str) -> Result<Value, crate::error::DunkError> {
        let state = self.terminal.flock.status_state()?;
        let home = if repo == "~" || repo.starts_with("~/") {
            state
                .python_home
                .expanduser_home()
                .map_err(crate::error::DunkError::Refused)?
        } else {
            None
        };
        let expanded = expand_summary_path(std::path::Path::new(repo), home, false, named_home)?;
        Ok(crate::flock::views::git_merges_expanded(
            &expanded.to_string_lossy(),
            10,
        ))
    }

    fn lanes(&self) -> Result<Vec<Value>, crate::error::DunkError> {
        self.terminal
            .flock
            .resolve_lanes(&self.terminal, crate::clock::now_s())
    }

    fn task_list(&self, include_done: bool) -> Result<Vec<Value>, crate::error::DunkError> {
        let (_, tasks) = self.runtime.block_on(self.terminal.flock.task_list(
            None,
            None,
            None,
            include_done,
            500,
            None,
            None,
            false,
        ))?;
        Ok(tasks
            .into_iter()
            .map(|row| {
                let task = row.task;
                json!({
                    "id": task.id,
                    "title": task.title,
                    "status": task.status,
                    "priority": task.priority,
                    "assignees": row.assignees,
                    "completed_at": task.completed_at,
                    "completed_by": task.completed_by,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::flock::panes::Stores;
    use crate::flock::project_summary::SummaryEnv;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::tasks::{AddOptions, Tasks};
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_env_summary_uses_concrete_services() {
        let dir = crate::testsupport::dir("summary-runtime");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        flock
            .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
                "fixture",
                dir.path().join("state.json"),
                dir.path().join("daemon"),
                crate::config::Env::from_pairs(&[("HOME", dir.path().to_str().unwrap())]),
            ))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(async {
            let completed = tasks
                .add_with(
                    "global",
                    "completed task",
                    "done",
                    AddOptions {
                        priority: Some("urgent".into()),
                        ..AddOptions::default()
                    },
                )
                .await
                .unwrap();
            tasks
                .set_status(&completed.id, "done", None, Some("%worker"))
                .await
                .unwrap();
            for index in 0..500 {
                tasks
                    .add("global", &format!("open task {index}"), "open")
                    .await
                    .unwrap();
            }
            tasks
                .add_with(
                    "w1:p1",
                    "pane-private task",
                    "private to pane",
                    AddOptions {
                        status: Some("in_progress".into()),
                        priority: Some("urgent".into()),
                        ..AddOptions::default()
                    },
                )
                .await
                .unwrap();
        });
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });

        let request = RequestCtx {
            self_pane_id: Some("w1:p1".into()),
            ..RequestCtx::default()
        };
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
            &flock,
            &request,
            runtime.handle(),
            &terminal,
        );

        let lanes = SummaryEnv::lanes(&adapter).unwrap();
        assert_eq!(3, lanes.len());
        assert!(lanes.iter().all(|lane| lane.get("pane_id").is_some()));

        let open = SummaryEnv::task_list(&adapter, false).unwrap();
        assert_eq!(500, open.len(), "uses the project-summary task limit");
        assert!(open.iter().all(|task| task["status"] != "done"));
        assert!(open.iter().all(|task| task["title"] != "pane-private task"));

        let all = SummaryEnv::task_list(&adapter, true).unwrap();
        assert_eq!(500, all.len(), "done rows share the same limit");
        let completed = all
            .iter()
            .find(|task| task["title"] == "completed task")
            .expect("urgent completed row survives the limit");
        assert!(completed["completed_at"].as_f64().is_some());
        assert_eq!("%worker", completed["completed_by"]);

        let summary = crate::flock::project_summary::project_summary(
            &adapter,
            "global",
            None,
            &[],
            1_800_000_000.0,
        )
        .unwrap();
        assert_eq!(1, summary["history"].as_array().unwrap().len());
        assert_eq!("%worker", summary["history"][0]["by"]);
        assert_ne!(
            "no completed task has a timestamp yet",
            summary["history_note"]
        );
        assert!(summary["now"]["tasks_in_flight"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn production_summary_propagates_terminal_and_task_store_errors() {
        let dir = crate::testsupport::dir("summary-runtime-errors");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let mut fake = FakeTerminal::new();
        fake.fail_grouped_panes = Some(crate::terminal::TerminalError::refused("summary offline"));
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
            &flock,
            &request,
            runtime.handle(),
            &terminal,
        );
        assert_eq!(
            SummaryEnv::lanes(&adapter).unwrap_err().to_string(),
            "cannot list panes: summary offline"
        );
        for include_done in [false, true] {
            let direct = runtime
                .block_on(flock.task_list(None, None, None, include_done, 500, None, None, false))
                .unwrap_err();
            assert_eq!(
                SummaryEnv::task_list(&adapter, include_done)
                    .unwrap_err()
                    .to_string(),
                direct.to_string()
            );
        }
        assert_eq!(
            SummaryEnv::global_context_root(&adapter)
                .unwrap_err()
                .to_string(),
            "status startup state is not configured"
        );
    }

    #[test]
    fn production_summary_uses_captured_context_root_and_home() {
        use crate::config::Env;
        use crate::flock::status_runtime::StatusState;
        use std::path::PathBuf;
        let dir = crate::testsupport::dir("summary-captured-paths");
        for (pairs, expected) in [
            (
                vec![("HOME", "/captured")],
                PathBuf::from("/captured/.config/dunkingsheep/context"),
            ),
            (
                vec![("HOME", "/captured"), ("DUNKINGSHEEP_DIR", "/explicit")],
                PathBuf::from("/explicit/context"),
            ),
            (
                vec![("HOME", "/captured"), ("DUNKINGSHEEP_DIR", "")],
                PathBuf::from("context"),
            ),
            (
                vec![("HOME", "/captured"), ("DUNKINGSHEEP_DIR", "  ")],
                PathBuf::from("  /context"),
            ),
            (
                vec![("HOME", "/captured"), ("DUNKINGSHEEP_DIR", "~/custom")],
                PathBuf::from("/captured/custom/context"),
            ),
            (
                vec![("HOME", "")],
                PathBuf::from("/.config/dunkingsheep/context"),
            ),
            (
                vec![("HOME", "~/a"), ("DUNKINGSHEEP_DIR", "/explicit")],
                PathBuf::from("/explicit/context"),
            ),
        ] {
            let flock = Flock::new(
                dir.path().join("launches.json"),
                dir.path().join("lanes.json"),
                false,
            );
            flock
                .attach_status_state(StatusState::at_start(
                    "fixture",
                    dir.path().join("state"),
                    dir.path().join("daemon"),
                    Env::from_pairs(&pairs),
                ))
                .unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let request = RequestCtx::default();
            let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
            let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
                &flock,
                &request,
                runtime.handle(),
                &terminal,
            );
            assert_eq!(SummaryEnv::global_context_root(&adapter).unwrap(), expected);
            let home = if pairs[0].1.is_empty() {
                "/"
            } else {
                pairs[0].1
            };
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "~/workspace/a/../b").unwrap(),
                crate::flock::project_summary::absolute_summary_path(
                    &PathBuf::from(home).join("workspace/b"),
                    &std::env::current_dir().unwrap()
                )
            );
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "~").unwrap(),
                crate::flock::project_summary::absolute_summary_path(
                    std::path::Path::new(home),
                    &std::env::current_dir().unwrap()
                )
            );
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "//host/a/../b")
                    .unwrap()
                    .as_os_str(),
                "//host/b"
            );
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "relative/../project").unwrap(),
                std::env::current_dir().unwrap().join("project")
            );
            let merges = SummaryEnv::merges(&adapter, "~/workspace/dunkingsheep").unwrap();
            assert_eq!(
                merges["repo"],
                PathBuf::from(home)
                    .join("workspace/dunkingsheep")
                    .to_string_lossy()
                    .as_ref()
            );
        }
    }

    #[test]
    fn production_summary_missing_captured_home_fails_closed_even_with_override() {
        use crate::config::{CapturedPythonHome, Env};
        use crate::flock::status_runtime::StatusState;
        let dir = crate::testsupport::dir("summary-missing-home");
        for pairs in [
            vec![],
            vec![("DUNKINGSHEEP_DIR", "")],
            vec![("DUNKINGSHEEP_DIR", "/explicit")],
        ] {
            let env = Env::from_pairs(&pairs);
            let mut state = StatusState::at_start(
                "fixture",
                dir.path().join("state"),
                dir.path().join("daemon"),
                env.clone(),
            );
            // Python dunk_context evaluates Path.home() eagerly as environ.get's default.
            state.python_home = CapturedPythonHome::capture_with(&env, || Ok(None));
            let flock = Flock::new(
                dir.path().join("launches.json"),
                dir.path().join("lanes.json"),
                false,
            );
            flock.attach_status_state(state).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let request = RequestCtx::default();
            let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
            let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
                &flock,
                &request,
                runtime.handle(),
                &terminal,
            );
            assert_eq!(
                SummaryEnv::global_context_root(&adapter)
                    .unwrap_err()
                    .to_string(),
                "Could not determine home directory."
            );
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "~/project").unwrap(),
                std::env::current_dir().unwrap().join("~/project")
            );
            assert_eq!(
                SummaryEnv::project_dir(&adapter, "/explicit/project").unwrap(),
                std::path::PathBuf::from("/explicit/project")
            );
        }
    }
    #[test]
    fn production_summary_preserves_captured_lookup_errors_only_when_home_is_needed() {
        use crate::config::{CapturedPythonHome, Env};
        use crate::flock::status_runtime::StatusState;
        let dir = crate::testsupport::dir("summary-home-lookup-error");
        let env = Env::from_pairs(&[]);
        let mut state = StatusState::at_start(
            "fixture",
            dir.path().join("state"),
            dir.path().join("daemon"),
            env.clone(),
        );
        state.python_home =
            CapturedPythonHome::capture_with(&env, || Err("account lookup failed".into()));
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_status_state(state).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
            &flock,
            &request,
            runtime.handle(),
            &terminal,
        );
        assert_eq!(
            SummaryEnv::project_dir(&adapter, "~/project")
                .unwrap_err()
                .to_string(),
            "account lookup failed"
        );
        assert_eq!(
            SummaryEnv::global_context_root(&adapter)
                .unwrap_err()
                .to_string(),
            "account lookup failed"
        );
        assert_eq!(
            SummaryEnv::project_dir(&adapter, "/explicit/project").unwrap(),
            std::path::PathBuf::from("/explicit/project")
        );
    }
}
