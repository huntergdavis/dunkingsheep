//! Owner-controlled planner grants and task/project associations.
//!
//! These records are distinct from lane membership. All mutation callers must enforce owner
//! authorization. Assignment checks and mutations must use the same SQLite transaction.
use rusqlite::{params, Connection, OptionalExtension};

use super::lane_project::ProjectScope;

#[derive(Clone, Debug)]
pub struct PlannerIdentity {
    pub pane: String,
    pub pid: i64,
    pub start: String,
    pub recipients: Vec<RecipientIdentity>,
    pub expected_kind: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RecipientIdentity {
    pub pane: String,
    pub pid: i64,
    pub start: String,
}

#[derive(Clone, Debug)]
pub enum DependencyEditor {
    Planner(PlannerIdentity),
    Other,
}

/// A project task's dependencies may be changed by its explicitly granted planner.
pub fn dependency_edit_allowed(
    db: &Connection,
    task: &str,
    editor: &DependencyEditor,
) -> rusqlite::Result<bool> {
    let project: Option<String> = db
        .query_row(
            "SELECT project FROM planner_task_projects WHERE task_id=?1",
            [task],
            |row| row.get(0),
        )
        .optional()?;
    let Some(project) = project else {
        return Ok(true);
    };
    match editor {
        DependencyEditor::Planner(planner) => db.query_row(
            "SELECT EXISTS(SELECT 1 FROM planner_scope_grants WHERE pane_id=?1 AND pane_pid=?2 AND proc_start=?3 AND scope_kind='project' AND project=?4)",
            params![planner.pane, planner.pid, planner.start, project], |row| row.get(0),
        ),
        DependencyEditor::Other => Ok(false),
    }
}

/// Project grants route only to recipients explicitly associated with that project.
pub fn recipients_allowed(db: &Connection, planner: &PlannerIdentity) -> rusqlite::Result<bool> {
    let project: Option<String> = db.query_row(
        "SELECT project FROM planner_scope_grants WHERE pane_id=?1 AND pane_pid=?2 AND proc_start=?3 AND scope_kind='project'",
        params![planner.pane, planner.pid, planner.start], |row| row.get(0),
    ).optional()?;
    let Some(project) = project else {
        return Ok(true);
    };
    for recipient in &planner.recipients {
        let membership =
            super::lane_project::get(db, &recipient.pane, recipient.pid, &recipient.start)?;
        if !membership.is_some_and(|m| m.scope == ProjectScope::Project(project.clone())) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn migrate(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS planner_scope_grants (\
         pane_id TEXT PRIMARY KEY, pane_pid INTEGER NOT NULL, proc_start TEXT NOT NULL,\
         scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global','project')), project TEXT,\
         CHECK((scope_kind='global' AND project IS NULL) OR\
               (scope_kind='project' AND project IS NOT NULL AND length(project)>0)));\
         CREATE TABLE IF NOT EXISTS planner_task_projects (\
         task_id TEXT PRIMARY KEY, project TEXT NOT NULL CHECK(length(project)>0));\
         CREATE TABLE IF NOT EXISTS planner_repo_projects (\
         repo TEXT PRIMARY KEY, project TEXT NOT NULL CHECK(length(project)>0));\
         CREATE TRIGGER IF NOT EXISTS planner_task_deleted AFTER DELETE ON tasks BEGIN \
           DELETE FROM planner_task_projects WHERE task_id=OLD.id; \
         END; \
         CREATE TRIGGER IF NOT EXISTS planner_task_replaced BEFORE INSERT ON tasks \
         WHEN EXISTS(SELECT 1 FROM tasks WHERE id=NEW.id) BEGIN \
           DELETE FROM planner_task_projects WHERE task_id=NEW.id; \
         END;",
    )
}

fn encode_scope(scope: &ProjectScope) -> rusqlite::Result<(&str, Option<&str>)> {
    match scope {
        ProjectScope::Global => Ok(("global", None)),
        ProjectScope::Project(name) if !name.is_empty() && name.trim() == name => {
            Ok(("project", Some(name)))
        }
        _ => Err(rusqlite::Error::InvalidParameterName(
            "invalid project".into(),
        )),
    }
}

/// Set a planner grant bound to a verified pane identity. The caller authenticates the owner.
pub fn set_grant(
    db: &Connection,
    pane: &str,
    pid: i64,
    start: &str,
    scope: &ProjectScope,
) -> rusqlite::Result<()> {
    let (kind, project) = encode_scope(scope)?;
    if pane.trim().is_empty() || pid <= 0 || start.is_empty() {
        return Err(rusqlite::Error::InvalidParameterName(
            "invalid pane identity".into(),
        ));
    }
    db.execute("INSERT INTO planner_scope_grants (pane_id,pane_pid,proc_start,scope_kind,project) \
        VALUES (?1,?2,?3,?4,?5) ON CONFLICT(pane_id) DO UPDATE SET \
        pane_pid=excluded.pane_pid,proc_start=excluded.proc_start,scope_kind=excluded.scope_kind,project=excluded.project",
        params![pane,pid,start,kind,project])?;
    Ok(())
}

pub fn revoke_grant(db: &Connection, pane: &str) -> rusqlite::Result<()> {
    db.execute("DELETE FROM planner_scope_grants WHERE pane_id=?1", [pane])?;
    Ok(())
}

/// Associate existing work with a project by an owner action.
pub fn set_task_project(
    db: &rusqlite::Transaction<'_>,
    task: &str,
    project: &str,
) -> rusqlite::Result<()> {
    if project.is_empty() || project.trim() != project {
        return Err(rusqlite::Error::InvalidParameterName(
            "invalid project".into(),
        ));
    }
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
        [task],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    db.execute("INSERT INTO planner_task_projects(task_id,project) VALUES (?1,?2) ON CONFLICT(task_id) DO UPDATE SET project=excluded.project", params![task,project])?;
    Ok(())
}

/// Resolve a planner-created review's project from the owner's exact repository mapping.
/// `None` means the grant or mapping does not authorize creation; `Some(None)` is
/// a global grant and `Some(Some(project))` is a matching project grant. Call from
/// the same transaction that inserts and associates the task.
pub fn review_creation_project(
    db: &Connection,
    planner: &PlannerIdentity,
    repo: Option<&str>,
) -> rusqlite::Result<Option<Option<String>>> {
    let grant: Option<(String, Option<String>)> = db.query_row(
        "SELECT scope_kind, project FROM planner_scope_grants WHERE pane_id=?1 AND pane_pid=?2 AND proc_start=?3",
        params![planner.pane, planner.pid, planner.start],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    match grant {
        Some((kind, None)) if kind == "global" => Ok(Some(None)),
        Some((kind, Some(project))) if kind == "project" => {
            let mapped: Option<String> = match repo {
                Some(repo) => db
                    .query_row(
                        "SELECT project FROM planner_repo_projects WHERE repo=?1",
                        [repo],
                        |row| row.get(0),
                    )
                    .optional()?,
                None => None,
            };
            Ok((mapped.as_deref() == Some(project.as_str())).then_some(Some(project)))
        }
        _ => Ok(None),
    }
}

/// Check a point-in-time pane grant and target inside the caller's assignment transaction.
pub fn allows(
    db: &Connection,
    pane: &str,
    pid: i64,
    start: &str,
    task: &str,
) -> rusqlite::Result<bool> {
    let grant: Option<(String,Option<String>)> = db.query_row("SELECT scope_kind,project FROM planner_scope_grants WHERE pane_id=?1 AND pane_pid=?2 AND proc_start=?3", params![pane,pid,start], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    let Some((kind, project)) = grant else {
        return Ok(false);
    };
    type Target = (String, Option<String>, String, i64);
    let target: Option<Target> = db.query_row("SELECT t.scope,p.project,t.status,t.landed FROM tasks t LEFT JOIN planner_task_projects p ON p.task_id=t.id WHERE t.id=?1", [task], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let Some((visibility, target_project, status, landed)) = target else {
        return Ok(false);
    };
    if kind == "global" && project.is_none() {
        return Ok(visibility == "global" && target_project.is_none());
    }
    let Some(project) = project.filter(|_| kind == "project") else {
        return Ok(false);
    };
    if target_project.as_deref() == Some(project.as_str()) {
        return Ok(true);
    }
    if visibility != "global" || !super::dependency_blocks(&status, landed != 0) {
        return Ok(false);
    }
    let mut stmt=db.prepare("SELECT t.depends_on FROM planner_task_projects p JOIN tasks t ON t.id=p.task_id WHERE p.project=?1 AND t.status IN ('open','in_progress','blocked')")?;
    let rows = stmt.query_map(params![project], |row| row.get::<_, String>(0))?;
    for deps in rows {
        if super::decode_list(Some(&deps?)).iter().any(|id| id == task) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_creation_uses_bound_grant_and_exact_repo_mapping() {
        let db = Connection::open_in_memory().unwrap();
        db.execute("CREATE TABLE tasks (id TEXT PRIMARY KEY)", [])
            .unwrap();
        migrate(&db).unwrap();
        let planner = PlannerIdentity {
            pane: "%2".into(),
            pid: 42,
            start: "start".into(),
            recipients: vec![],
            expected_kind: None,
        };
        assert_eq!(
            review_creation_project(&db, &planner, Some("org/repo")).unwrap(),
            None
        );
        set_grant(
            &db,
            "%2",
            42,
            "start",
            &ProjectScope::Project("alpha".into()),
        )
        .unwrap();
        db.execute(
            "INSERT INTO planner_repo_projects(repo,project) VALUES ('org/repo','alpha')",
            [],
        )
        .unwrap();
        assert_eq!(
            review_creation_project(&db, &planner, Some("org/repo")).unwrap(),
            Some(Some("alpha".into()))
        );
        assert_eq!(
            review_creation_project(&db, &planner, Some("org/other")).unwrap(),
            None
        );
        let mut stale = planner.clone();
        stale.start = "stale".into();
        assert_eq!(
            review_creation_project(&db, &stale, Some("org/repo")).unwrap(),
            None
        );
        set_grant(&db, "%2", 42, "start", &ProjectScope::Global).unwrap();
        assert_eq!(
            review_creation_project(&db, &planner, None).unwrap(),
            Some(None)
        );
    }

    #[tokio::test]
    async fn review_sync_uses_only_explicit_repository_mapping() {
        let dir = crate::testsupport::dir("review-project-mapping");
        let tasks = super::super::Tasks::open(dir.path().join("tasks.db"))
            .await
            .unwrap();
        let db = Connection::open(tasks.path()).unwrap();
        db.execute(
            "INSERT INTO planner_repo_projects(repo,project) VALUES ('example/one','alpha')",
            [],
        )
        .unwrap();
        for (repo, pr, expected) in [
            ("example/one", "1", Some("alpha")),
            ("example/one", "2", Some("alpha")),
            ("other/one", "1", None),
            ("Example/one", "1", None),
        ] {
            let opts = super::super::AddOptions {
                kind: Some("review".into()),
                repo: Some(repo.into()),
                pr: Some(pr.into()),
                head: Some("abc".into()),
                created_by: Some("review-sync".into()),
                ..Default::default()
            };
            let task = tasks
                .add_review_with_verdict_guard("global", "review", "", opts, &[])
                .await
                .unwrap()
                .unwrap();
            let actual: Option<String> = db
                .query_row(
                    "SELECT project FROM planner_task_projects WHERE task_id=?1",
                    [&task.id],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(actual.as_deref(), expected);
        }
        let manual = super::super::AddOptions {
            kind: Some("review".into()),
            repo: Some("example/one".into()),
            pr: Some("3".into()),
            head: Some("abc".into()),
            created_by: Some("review-sync".into()),
            ..Default::default()
        };
        let task = tasks
            .add_with("global", "manual review", "", manual)
            .await
            .unwrap();
        let assigned: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM planner_task_projects WHERE task_id=?1)",
                [&task.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!assigned);
    }

    #[tokio::test]
    async fn planner_scope_controls_assignment_transaction() {
        let dir = crate::testsupport::dir("planner-scope-assignment");
        let tasks = super::super::Tasks::open(dir.path().join("tasks.db"))
            .await
            .unwrap();
        let project = tasks.add("global", "project work", "").await.unwrap();
        let unrelated = tasks.add("global", "unrelated work", "").await.unwrap();
        let foreign = tasks.add("global", "foreign blocker", "").await.unwrap();
        let mut db = Connection::open(tasks.path()).unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        set_task_project(&tx, &project.id, "alpha").unwrap();
        set_task_project(&tx, &foreign.id, "beta").unwrap();
        set_grant(
            &tx,
            "%2",
            22,
            "start",
            &ProjectScope::Project("alpha".into()),
        )
        .unwrap();
        super::super::lane_project::set(
            &tx,
            &super::super::lane_project::Membership {
                pane: "%3".into(),
                pid: 33,
                start: "recipient".into(),
                scope: ProjectScope::Project("alpha".into()),
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let planner = PlannerIdentity {
            pane: "%2".into(),
            pid: 22,
            start: "start".into(),
            recipients: vec![RecipientIdentity {
                pane: "%3".into(),
                pid: 33,
                start: "recipient".into(),
            }],
            expected_kind: Some("work".into()),
        };
        assert!(tasks
            .assign_with_planner_scope(&foreign.id, &["%3"], "add", true, Some(planner.clone()))
            .await
            .is_err());
        let foreign_dependency = super::super::FieldEdit {
            depends_on: Some(vec![foreign.id.clone()]),
            ..Default::default()
        };
        tasks
            .update_with_status_and_raw_block_reason(
                &project.id,
                foreign_dependency,
                None,
                Some("%2"),
                None,
                Some(DependencyEditor::Planner(planner.clone())),
            )
            .await
            .unwrap();
        tasks
            .assign_with_planner_scope(&foreign.id, &["%3"], "add", true, Some(planner.clone()))
            .await
            .unwrap();
        assert!(tasks
            .get(&foreign.id)
            .await
            .unwrap()
            .assignees
            .contains("%3"));
        let dependency_edit = super::super::FieldEdit {
            depends_on: Some(vec![unrelated.id.clone()]),
            ..Default::default()
        };
        assert!(tasks
            .update_with_status_and_raw_block_reason(
                &project.id,
                dependency_edit.clone(),
                None,
                Some("%2"),
                None,
                Some(DependencyEditor::Other),
            )
            .await
            .is_err());
        tasks
            .update_with_status_and_raw_block_reason(
                &project.id,
                dependency_edit,
                None,
                Some("%2"),
                None,
                Some(DependencyEditor::Planner(planner.clone())),
            )
            .await
            .unwrap();
        assert!(allows(&db, "%2", 22, "start", &unrelated.id).unwrap());
        tasks
            .update(
                &project.id,
                super::super::FieldEdit {
                    depends_on: Some(vec![]),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        tasks
            .assign_with_planner_scope(&project.id, &["%3"], "add", true, Some(planner.clone()))
            .await
            .unwrap();
        db.execute("UPDATE tasks SET kind='review' WHERE id=?1", [&project.id])
            .unwrap();
        let error = tasks
            .assign_with_planner_scope(&project.id, &["%3"], "add", true, Some(planner.clone()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("task kind changed"), "{error}");
        db.execute("UPDATE tasks SET kind='work' WHERE id=?1", [&project.id])
            .unwrap();
        assert!(tasks
            .assign_with_planner_scope(&unrelated.id, &["%3"], "add", true, Some(planner.clone()))
            .await
            .is_err());
        assert_eq!(tasks.get(&unrelated.id).await.unwrap().assignees, "[]");
        super::super::lane_project::set(
            &db,
            &super::super::lane_project::Membership {
                pane: "%3".into(),
                pid: 33,
                start: "recipient".into(),
                scope: ProjectScope::Project("beta".into()),
            },
        )
        .unwrap();
        assert!(tasks
            .assign_with_planner_scope(&project.id, &["%3"], "add", true, Some(planner))
            .await
            .is_err());
    }

    #[test]
    fn grants_route_global_project_and_current_blockers() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE tasks(id TEXT PRIMARY KEY,scope TEXT,depends_on TEXT,status TEXT DEFAULT 'open', landed INTEGER DEFAULT 1); \
            INSERT INTO tasks (id,scope,depends_on) VALUES('global','global','[]'),('prereq','global','[]'),\
            ('project','global','[\"prereq\"]'),('other','global','[]');",
        )
        .unwrap();
        migrate(&db).unwrap();
        {
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            set_task_project(&tx, "project", "alpha").unwrap();
            set_task_project(&tx, "other", "beta").unwrap();
            tx.commit().unwrap();
        }
        set_grant(&db, "%1", 11, "start", &ProjectScope::Global).unwrap();
        set_grant(
            &db,
            "%2",
            22,
            "start",
            &ProjectScope::Project("alpha".into()),
        )
        .unwrap();
        assert!(allows(&db, "%1", 11, "start", "global").unwrap());
        assert!(!allows(&db, "%1", 11, "start", "project").unwrap());
        assert!(allows(&db, "%2", 22, "start", "project").unwrap());
        assert!(!allows(&db, "%2", 22, "start", "other").unwrap());
        assert!(allows(&db, "%2", 22, "start", "prereq").unwrap());
        db.execute(
            "UPDATE tasks SET depends_on='[\"prereq\",\"other\"]' WHERE id='project'",
            [],
        )
        .unwrap();
        assert!(allows(&db, "%2", 22, "start", "other").unwrap());
        assert!(!allows(&db, "%2", 23, "start", "other").unwrap());
        db.execute("UPDATE tasks SET status='done' WHERE id='other'", [])
            .unwrap();
        assert!(!allows(&db, "%2", 22, "start", "other").unwrap());
        db.execute(
            "UPDATE tasks SET status='done', landed=1 WHERE id='prereq'",
            [],
        )
        .unwrap();
        assert!(!allows(&db, "%2", 22, "start", "prereq").unwrap());
        db.execute("UPDATE tasks SET landed=0 WHERE id='prereq'", [])
            .unwrap();
        assert!(allows(&db, "%2", 22, "start", "prereq").unwrap());
        db.execute("UPDATE tasks SET status='cancelled' WHERE id='prereq'", [])
            .unwrap();
        assert!(!allows(&db, "%2", 22, "start", "prereq").unwrap());
        assert!(!allows(&db, "%2", 23, "start", "project").unwrap());
        db.execute("UPDATE tasks SET depends_on='[]' WHERE id='project'", [])
            .unwrap();
        assert!(!allows(&db, "%2", 22, "start", "prereq").unwrap());
        revoke_grant(&db, "%2").unwrap();
        assert!(!allows(&db, "%2", 22, "start", "project").unwrap());
    }
}
