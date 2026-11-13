//! `task_backlog` (t4648): one deterministic answer to "how much is open, per project".
//!
//! A task's project is its row in `planner_task_projects`, else the project of its `repo` in
//! `planner_repo_projects`, else the literal bucket [`UNMAPPED`]. Nothing is inferred from titles,
//! assignees or parents. Every task counted is in exactly one project row, the `unmapped` row is
//! always present, and the rows add up to the totals row. Only `global` tasks are counted: a
//! pane's private tasks are not backlog. Read-only.

use rusqlite::params;
use serde_json::{json, Map, Value};

use super::{db, Tasks, KINDS, PRIORITIES};
use crate::error::DunkError;

/// The bucket for a task with no project mapping.
pub const UNMAPPED: &str = "unmapped";
/// The unfinished statuses, in display order.
pub const UNFINISHED: [&str; 3] = ["open", "in_progress", "blocked"];
/// Every status, unfinished first.
pub const STATUSES: [&str; 5] = ["open", "in_progress", "blocked", "done", "cancelled"];
/// What the numbers mean, returned with every answer so a count is never read without it.
pub const DEFINITION: &str = "global tasks only; project = task mapping, else repo mapping, else \
    'unmapped' (never inferred); active = open + in_progress; blocked_on_owner = blocked with a \
    'needs-owner:' reason; finished tasks are excluded unless requested";

/// The filters of one backlog query. Empty lists mean "no filter".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BacklogQuery {
    /// A project name, or `unmapped`.
    pub project: Option<String>,
    pub statuses: Vec<String>,
    pub priorities: Vec<String>,
    pub kinds: Vec<String>,
    /// Include `done` and `cancelled`.
    pub all: bool,
    /// Also return the matching tasks, so any count can be opened.
    pub list: bool,
}

impl BacklogQuery {
    /// Refuse a filter value that could never match, instead of answering zero.
    pub fn validate(&self) -> Result<(), DunkError> {
        let check = |what: &str, values: &[String], allowed: &[&str]| match values
            .iter()
            .find(|v| !allowed.contains(&v.as_str()))
        {
            Some(bad) => Err(DunkError::Refused(format!(
                "task backlog: unknown {what} {bad:?}; expected one of {}",
                allowed.join(", ")
            ))),
            None => Ok(()),
        };
        check("status", &self.statuses, &STATUSES)?;
        check("priority", &self.priorities, &PRIORITIES)?;
        check("kind", &self.kinds, &KINDS)?;
        if self.project.as_deref().is_some_and(|p| p.trim().is_empty()) {
            return Err(DunkError::Refused(
                "task backlog: an empty project name matches nothing; omit it or use 'unmapped'"
                    .into(),
            ));
        }
        Ok(())
    }

    fn statuses(&self) -> Vec<&str> {
        if !self.statuses.is_empty() {
            STATUSES
                .iter()
                .copied()
                .filter(|s| self.statuses.iter().any(|wanted| wanted == s))
                .collect()
        } else if self.all {
            STATUSES.to_vec()
        } else {
            UNFINISHED.to_vec()
        }
    }
}

struct Row {
    project: String,
    id: String,
    title: String,
    status: String,
    priority: String,
    kind: String,
    on_owner: bool,
}

fn counts(rows: &[&Row], statuses: &[&str]) -> Value {
    let mut out = Map::new();
    let count = |pred: &dyn Fn(&Row) -> bool| rows.iter().filter(|r| pred(r)).count();
    for status in statuses {
        out.insert((*status).into(), json!(count(&|r| r.status == *status)));
    }
    out.insert(
        "active".into(),
        json!(count(&|r| r.status == "open" || r.status == "in_progress")),
    );
    out.insert(
        "blocked_on_owner".into(),
        json!(count(&|r| r.status == "blocked" && r.on_owner)),
    );
    out.insert("total".into(), json!(rows.len()));
    let tally = |field: &dyn Fn(&Row) -> &str, order: &[&str]| -> Value {
        let mut map = Map::new();
        for key in order {
            let n = count(&|r| field(r) == *key);
            if n > 0 {
                map.insert((*key).into(), json!(n));
            }
        }
        Value::Object(map)
    };
    let by_priority: Vec<&str> = PRIORITIES.iter().rev().copied().collect();
    out.insert(
        "by_priority".into(),
        tally(&|r| r.priority.as_str(), &by_priority),
    );
    out.insert("by_kind".into(), tally(&|r| r.kind.as_str(), &KINDS));
    Value::Object(out)
}

impl Tasks {
    /// The backlog counts, and the matching tasks when asked. See the module comment.
    pub async fn backlog(&self, query: BacklogQuery, now: f64) -> Result<Value, DunkError> {
        query.validate()?;
        let statuses: Vec<String> = query.statuses().iter().map(|s| s.to_string()).collect();
        let all_rows: Vec<Row> = self
            .writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT COALESCE(p.project, rp.project), t.id, t.title, t.status, \
                         t.priority, t.kind, COALESCE(t.block_reason, '') \
                         FROM tasks t \
                         LEFT JOIN planner_task_projects p ON p.task_id = t.id \
                         LEFT JOIN planner_repo_projects rp ON rp.repo = t.repo \
                         WHERE t.scope = ?1 ORDER BY t.created_at, t.id",
                    )
                    .map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map(params!["global"], |row| {
                        let project: Option<String> = row.get(0)?;
                        let reason: String = row.get(6)?;
                        Ok(Row {
                            project: project
                                .filter(|p| !p.is_empty())
                                .unwrap_or_else(|| UNMAPPED.to_string()),
                            id: row.get(1)?,
                            title: row.get(2)?,
                            status: row.get(3)?,
                            priority: row.get(4)?,
                            kind: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                            on_owner: reason.trim_start().starts_with("needs-owner:"),
                        })
                    })
                    .map_err(db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
            })
            .await?;
        let wanted = |r: &&Row| {
            statuses.iter().any(|s| *s == r.status)
                && (query.priorities.is_empty() || query.priorities.contains(&r.priority))
                && (query.kinds.is_empty() || query.kinds.contains(&r.kind))
                && query.project.as_deref().is_none_or(|p| p == r.project)
        };
        let matching: Vec<&Row> = all_rows.iter().filter(wanted).collect();
        let shown: Vec<&str> = statuses.iter().map(String::as_str).collect();
        // Mapped projects by name, then the unmapped bucket, which is always shown: with a
        // project filter only that project's row is shown, and it is shown even when empty.
        let mut names: Vec<&str> = matching.iter().map(|r| r.project.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names.retain(|name| *name != UNMAPPED);
        match query.project.as_deref() {
            Some(UNMAPPED) => names.clear(),
            Some(project) if names.is_empty() => names.push(project),
            _ => {}
        }
        if query.project.as_deref().is_none_or(|p| p == UNMAPPED) {
            names.push(UNMAPPED);
        }
        let projects: Vec<Value> = names
            .iter()
            .map(|name| {
                let rows: Vec<&Row> = matching
                    .iter()
                    .copied()
                    .filter(|r| r.project == *name)
                    .collect();
                let mut row = counts(&rows, &shown);
                row["project"] = json!(name);
                row
            })
            .collect();
        let mut out = json!({
            "as_of": now,
            "scope": "global",
            "definition": DEFINITION,
            "statuses": shown,
            "filters": {
                "project": query.project,
                "status": query.statuses,
                "priority": query.priorities,
                "kind": query.kinds,
                "all": query.all,
            },
            "projects": projects,
            "totals": counts(&matching, &shown),
        });
        if query.list {
            out["tasks"] = matching
                .iter()
                .map(|r| {
                    json!({"id": r.id, "title": r.title, "status": r.status,
                        "priority": r.priority, "kind": r.kind, "project": r.project,
                        "blocked_on_owner": r.status == "blocked" && r.on_owner})
                })
                .collect();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Tasks) {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        (dir, tasks)
    }

    async fn sql(tasks: &Tasks, statement: String) {
        tasks
            .writer
            .call(move |conn| conn.execute_batch(&statement).map_err(db::map_sqlite))
            .await
            .unwrap();
    }

    /// Mixed projects, a repo-only mapping, unmapped tasks, a private pane task, finished tasks
    /// and a task blocked on the owner: every counted task is in exactly one row, the rows add
    /// up to the totals, and `unmapped` is its own row.
    #[tokio::test]
    async fn backlog_counts_by_project_with_an_explicit_unmapped_bucket() {
        let (_dir, tasks) = store().await;
        let mut ids = Vec::new();
        for title in ["a1", "a2", "a3", "b1", "repo1", "loose1", "loose2", "done1"] {
            ids.push(tasks.add("global", title, "").await.unwrap().id);
        }
        tasks.add("w1:p1", "private", "").await.unwrap();
        let statement = format!(
            "INSERT INTO planner_task_projects(task_id, project) VALUES \
             ('{0}','alpha'),('{1}','alpha'),('{2}','alpha'),('{3}','beta'),('{6}','alpha');\
             INSERT INTO planner_repo_projects(repo, project) VALUES ('o/r','beta');\
             UPDATE tasks SET repo='o/r' WHERE id='{4}';\
             UPDATE tasks SET status='in_progress', priority='urgent' WHERE id='{1}';\
             UPDATE tasks SET status='blocked', block_reason='needs-owner: pick one' WHERE id='{2}';\
             UPDATE tasks SET status='blocked', block_reason='waiting on CI' WHERE id='{5}';\
             UPDATE tasks SET status='done' WHERE id='{6}';\
             UPDATE tasks SET kind='review' WHERE id='{3}';",
            ids[0], ids[1], ids[2], ids[3], ids[4], ids[6], ids[7]
        );
        sql(&tasks, statement).await;

        let out = tasks.backlog(BacklogQuery::default(), 5.0).await.unwrap();
        assert_eq!(out["statuses"], json!(["open", "in_progress", "blocked"]));
        let row = |name: &str| {
            out["projects"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["project"] == name)
                .unwrap_or_else(|| panic!("no row {name}"))
                .clone()
        };
        let names: Vec<&str> = out["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["project"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "beta", "unmapped"]);
        let alpha = row("alpha");
        assert_eq!(
            (&alpha["open"], &alpha["in_progress"], &alpha["blocked"]),
            (&json!(1), &json!(1), &json!(1))
        );
        assert_eq!(
            (
                &alpha["active"],
                &alpha["blocked_on_owner"],
                &alpha["total"]
            ),
            (&json!(2), &json!(1), &json!(3))
        );
        assert_eq!(alpha["by_priority"]["urgent"], 1);
        // beta: one by task mapping, one only through its repo.
        assert_eq!(row("beta")["total"], 2);
        assert_eq!(row("beta")["by_kind"]["review"], 1);
        let unmapped = row("unmapped");
        assert_eq!(
            (
                &unmapped["open"],
                &unmapped["blocked"],
                &unmapped["blocked_on_owner"]
            ),
            (&json!(1), &json!(1), &json!(0))
        );
        let sum: u64 = out["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["total"].as_u64().unwrap())
            .sum();
        assert_eq!(
            sum, 7,
            "the private pane task and the finished task are not counted"
        );
        assert_eq!(out["totals"]["total"], 7);
        assert!(out.get("tasks").is_none());

        // --project unmapped --list opens exactly the unmapped tasks.
        let listed = tasks
            .backlog(
                BacklogQuery {
                    project: Some(UNMAPPED.into()),
                    list: true,
                    ..Default::default()
                },
                5.0,
            )
            .await
            .unwrap();
        let listed_ids: Vec<&str> = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed_ids, [ids[5].as_str(), ids[6].as_str()]);
        assert_eq!(listed["projects"].as_array().unwrap().len(), 1);

        // Filters narrow rows and totals together; finished tasks appear only when asked.
        let blocked = tasks
            .backlog(
                BacklogQuery {
                    statuses: vec!["blocked".into()],
                    ..Default::default()
                },
                5.0,
            )
            .await
            .unwrap();
        assert_eq!(blocked["totals"]["total"], 2);
        assert_eq!(blocked["totals"]["blocked_on_owner"], 1);
        let all = tasks
            .backlog(
                BacklogQuery {
                    project: Some("alpha".into()),
                    all: true,
                    ..Default::default()
                },
                5.0,
            )
            .await
            .unwrap();
        assert_eq!(all["totals"]["done"], 1);
        assert_eq!(all["totals"]["total"], 4);

        // A project with nothing left still answers with its own zero row; a bad filter is refused.
        let empty = tasks
            .backlog(
                BacklogQuery {
                    project: Some("gamma".into()),
                    ..Default::default()
                },
                5.0,
            )
            .await
            .unwrap();
        assert_eq!(empty["projects"][0]["project"], "gamma");
        assert_eq!(empty["totals"]["total"], 0);
        for bad in [
            BacklogQuery {
                statuses: vec!["stuck".into()],
                ..Default::default()
            },
            BacklogQuery {
                priorities: vec!["p0".into()],
                ..Default::default()
            },
            BacklogQuery {
                kinds: vec!["chore".into()],
                ..Default::default()
            },
            BacklogQuery {
                project: Some(" ".into()),
                ..Default::default()
            },
        ] {
            assert!(tasks.backlog(bad, 5.0).await.is_err());
        }
    }
}
