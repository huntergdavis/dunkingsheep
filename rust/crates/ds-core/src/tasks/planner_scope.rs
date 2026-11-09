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
}

#[derive(Clone, Debug)]
pub struct RecipientIdentity {
    pub pane: String,
    pub pid: i64,
    pub start: String,
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
    let target: Option<(String,Option<String>)> = db.query_row("SELECT t.scope,p.project FROM tasks t LEFT JOIN planner_task_projects p ON p.task_id=t.id WHERE t.id=?1", [task], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    let Some((visibility, target_project)) = target else {
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
    if visibility != "global" || target_project.is_some() {
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

    #[tokio::test]
    async fn planner_scope_controls_assignment_transaction() {
        let dir = crate::testsupport::dir("planner-scope-assignment");
        let tasks = super::super::Tasks::open(dir.path().join("tasks.db"))
            .await
            .unwrap();
        let project = tasks.add("global", "project work", "").await.unwrap();
        let unrelated = tasks.add("global", "unrelated work", "").await.unwrap();
        let mut db = Connection::open(tasks.path()).unwrap();
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        set_task_project(&tx, &project.id, "alpha").unwrap();
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
        };
        tasks
            .assign_with_planner_scope(&project.id, &["%3"], "add", true, Some(planner.clone()))
            .await
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
    fn grants_route_global_project_and_approved_current_blockers() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE tasks(id TEXT PRIMARY KEY,scope TEXT,depends_on TEXT,status TEXT DEFAULT 'open'); \
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
        assert!(!allows(&db, "%2", 23, "start", "project").unwrap());
        db.execute("UPDATE tasks SET depends_on='[]' WHERE id='project'", [])
            .unwrap();
        assert!(!allows(&db, "%2", 22, "start", "prereq").unwrap());
        revoke_grant(&db, "%2").unwrap();
        assert!(!allows(&db, "%2", 22, "start", "project").unwrap());
    }
}
