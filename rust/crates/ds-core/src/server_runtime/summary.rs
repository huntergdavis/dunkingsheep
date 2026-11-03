//! Production lane and task sources for the project summary view.

use serde_json::{json, Value};

use crate::flock::project_summary::SummaryEnv;
use crate::server_runtime::RequestRuntimeAdapter;

impl SummaryEnv for RequestRuntimeAdapter<'_> {
    fn lanes(&self) -> Vec<Value> {
        match self
            .terminal
            .flock
            .resolve_lanes(&self.terminal, crate::clock::now_s())
        {
            Ok(lanes) => lanes,
            Err(error) => {
                eprintln!("project summary could not resolve lanes: {error}");
                Vec::new()
            }
        }
    }

    fn task_list(&self, include_done: bool) -> Vec<Value> {
        let result = self.runtime.block_on(self.terminal.flock.task_list(
            None,
            None,
            None,
            include_done,
            500,
            None,
            None,
            false,
        ));
        match result {
            Ok((_, tasks)) => tasks
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
                .collect(),
            Err(error) => {
                eprintln!("project summary could not list tasks: {error}");
                Vec::new()
            }
        }
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

        let lanes = SummaryEnv::lanes(&adapter);
        assert_eq!(3, lanes.len());
        assert!(lanes.iter().all(|lane| lane.get("pane_id").is_some()));

        let open = SummaryEnv::task_list(&adapter, false);
        assert_eq!(500, open.len(), "uses the project-summary task limit");
        assert!(open.iter().all(|task| task["status"] != "done"));
        assert!(open.iter().all(|task| task["title"] != "pane-private task"));

        let all = SummaryEnv::task_list(&adapter, true);
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
        );
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
}
