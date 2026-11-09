//! Explicit project membership for live panes. This is not a planner authority grant.
use super::{panes::PaneInfoEnv, Flock};
use crate::error::DunkError;
use crate::tasks::lane_project::{self, Membership, ProjectScope};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

fn failure(error: impl std::fmt::Display) -> DunkError {
    DunkError::Refused(format!("lane project: {error}"))
}
pub(crate) fn identity(env: &dyn PaneInfoEnv, pane: &str) -> Result<(i64, String), DunkError> {
    let row = env
        .get_pane(pane)
        .ok_or_else(|| failure("pane is not live"))?;
    let pid = row
        .get("pane_pid")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|pid| *pid > 0)
        .ok_or_else(|| failure("pane process is unknown"))?;
    let start =
        crate::proc::process_start(pid).ok_or_else(|| failure("pane process is not live"))?;
    Ok((pid, start))
}
fn display(scope: &ProjectScope) -> Value {
    match scope {
        ProjectScope::Global => json!({"scope":"global","project":null}),
        ProjectScope::Project(name) => json!({"scope":"project","project":name}),
    }
}
impl Flock {
    fn project_connection(&self) -> Result<Connection, DunkError> {
        let store = self.task_store()?;
        let db = Connection::open_with_flags(store.path(), OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(failure)?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        Ok(db)
    }
    /// The API authenticates the owner before invoking this operation.
    pub fn planner_scope_admin(
        &self,
        env: &dyn PaneInfoEnv,
        args: &Value,
    ) -> Result<Value, DunkError> {
        let field = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| failure(format!("{key} is required")))
        };
        let operation = field("operation")?;
        let binding = if operation == "grant" {
            Some(identity(env, field("pane")?)?)
        } else {
            None
        };
        let mut db = self.project_connection()?;
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(failure)?;
        let mut result = json!({"ok":true,"operation":operation});
        match operation {
            "grant" => {
                let pane = field("pane")?;
                let (pid, start) = binding.as_ref().expect("grant identity captured");
                let scope = match args.get("project").and_then(Value::as_str) {
                    Some(project) => ProjectScope::Project(project.to_owned()),
                    None if args.get("global").and_then(Value::as_bool) == Some(true) => {
                        ProjectScope::Global
                    }
                    _ => return Err(failure("choose a project or global grant")),
                };
                if args.get("global").and_then(Value::as_bool) == Some(true)
                    && args.get("project").is_some_and(|value| !value.is_null())
                {
                    return Err(failure("choose a project or global grant"));
                }
                crate::tasks::planner_scope::set_grant(&tx, pane, *pid, start, &scope)
                    .map_err(failure)?;
            }
            "revoke" => {
                crate::tasks::planner_scope::revoke_grant(&tx, field("pane")?).map_err(failure)?
            }
            "task-project" => {
                let task = field("task")?;
                crate::tasks::planner_scope::set_task_project(&tx, task, field("project")?)
                    .map_err(failure)?;
                let raw: String = tx
                    .query_row("SELECT depends_on FROM tasks WHERE id=?1", [task], |row| {
                        row.get(0)
                    })
                    .map_err(failure)?;
                let dependencies: Value = serde_json::from_str(&raw).map_err(failure)?;
                if !dependencies.is_array() {
                    return Err(failure("task dependencies are invalid"));
                }
                result["existing_dependencies"] = dependencies;
            }
            "repo-project" => {
                let repo = field("repo")?;
                let project = field("project")?;
                if repo.trim() != repo
                    || project.trim() != project
                    || repo.split('/').count() != 2
                    || repo.split('/').any(str::is_empty)
                {
                    return Err(failure("expected exact owner/repository and project"));
                }
                tx.execute("INSERT INTO planner_repo_projects(repo,project) VALUES (?1,?2) ON CONFLICT(repo) DO UPDATE SET project=excluded.project", rusqlite::params![repo, project]).map_err(failure)?;
            }
            "repo-revoke" => {
                tx.execute(
                    "DELETE FROM planner_repo_projects WHERE repo=?1",
                    [field("repo")?],
                )
                .map_err(failure)?;
            }
            _ => return Err(failure("unknown planner scope operation")),
        }
        tx.commit().map_err(failure)?;
        Ok(result)
    }
    /// Look up a currently live pane. A changed process or missing binding is unknown.
    pub fn lane_project(
        &self,
        env: &dyn PaneInfoEnv,
        pane: &str,
    ) -> Result<Option<ProjectScope>, DunkError> {
        let (pid, start) = identity(env, pane)?;
        let db = self.project_connection()?;
        Ok(lane_project::get(&db, pane, pid, &start)
            .map_err(failure)?
            .map(|m| m.scope))
    }
    /// Bind an existing pane after authorization by the API handler.
    pub fn lane_project_set(
        &self,
        env: &dyn PaneInfoEnv,
        pane: &str,
        scope: ProjectScope,
    ) -> Result<Value, DunkError> {
        let (pid, start) = identity(env, pane)?;
        let db = self.project_connection()?;
        lane_project::set(
            &db,
            &Membership {
                pane: pane.to_owned(),
                pid,
                start,
                scope: scope.clone(),
            },
        )
        .map_err(failure)?;
        let mut out = display(&scope);
        out["pane_id"] = json!(pane);
        Ok(out)
    }
    /// Query representation for clients without treating missing membership as global.
    pub fn lane_project_view(&self, env: &dyn PaneInfoEnv, pane: &str) -> Value {
        match self.lane_project(env, pane) {
            Ok(Some(scope)) => display(&scope),
            Ok(None) | Err(_) => {
                let state = match self
                    .project_connection()
                    .and_then(|db| lane_project::contains(&db, pane).map_err(failure))
                {
                    Ok(true) => "stale",
                    Ok(false) => "unknown",
                    Err(_) => "unavailable",
                };
                json!({"scope":state,"project":null})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    struct Pane(i64);
    impl PaneInfoEnv for Pane {
        fn list_panes_grouped(&self) -> Vec<Value> {
            vec![]
        }
        fn get_pane(&self, id: &str) -> Option<Value> {
            Some(json!({"pane_id":id,"pane_pid":self.0}))
        }
    }
    #[tokio::test]
    async fn project_association_reports_existing_dependencies() {
        let dir = crate::testsupport::dir("planner-project-association");
        let tasks = Arc::new(
            crate::tasks::Tasks::open(dir.path().join("tasks.db"))
                .await
                .unwrap(),
        );
        let blocker = tasks.add("global", "blocker", "").await.unwrap();
        let project = tasks
            .add_with(
                "global",
                "project",
                "",
                crate::tasks::AddOptions {
                    depends_on: vec![blocker.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            true,
        );
        flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(tasks),
            ..Default::default()
        });
        let result = flock
            .planner_scope_admin(
                &Pane(i64::from(std::process::id())),
                &json!({
                    "operation":"task-project", "task":project.id, "project":"alpha"
                }),
            )
            .unwrap();
        assert_eq!(result["existing_dependencies"], json!([blocker.id]));
    }
    #[tokio::test]
    async fn binding_survives_reopen_and_detects_process_change() {
        let dir = crate::testsupport::dir("lane-project-live");
        let path = dir.path().join("tasks.db");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            true,
        );
        let tasks = Arc::new(crate::tasks::Tasks::open(&path).await.unwrap());
        flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(tasks),
            ..Default::default()
        });
        let live = Pane(i64::from(std::process::id()));
        assert_eq!(flock.lane_project_view(&live, "%1")["scope"], "unknown");
        flock
            .lane_project_set(&live, "%1", ProjectScope::Project("poolrad".into()))
            .unwrap();
        assert_eq!(flock.lane_project_view(&live, "%1")["project"], "poolrad");
        let stale = Pane(i64::MAX);
        assert_eq!(flock.lane_project_view(&stale, "%1")["scope"], "stale");
        let reopened = Flock::new(
            dir.path().join("launches-2.json"),
            dir.path().join("lanes-2.json"),
            true,
        );
        let tasks = Arc::new(crate::tasks::Tasks::open(&path).await.unwrap());
        reopened.attach_stores(crate::flock::panes::Stores {
            tasks: Some(tasks),
            ..Default::default()
        });
        assert_eq!(
            reopened.lane_project_view(&live, "%1")["project"],
            "poolrad"
        );
    }
}
