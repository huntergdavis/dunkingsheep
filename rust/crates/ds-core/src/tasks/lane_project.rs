//! Durable explicit lane project membership. This is membership metadata, not a planner grant.
//!
//! Callers must establish owner authorization and verify live pane identity before writing.
//! A lookup validates the recorded PID and process start supplied by its caller. This initial
//! store does not provide atomic revocation against legacy role writers or inherited lineage.

use rusqlite::{params, Connection, OptionalExtension};

/// A named project is distinct from the explicit global scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectScope {
    Project(String),
    Global,
}

/// The membership is bound to the process occupying the lane at registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Membership {
    pub pane: String,
    pub pid: i64,
    pub start: String,
    pub scope: ProjectScope,
}

/// Create the additive store in the same database as tasks.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lane_project_membership (\
        pane_id TEXT PRIMARY KEY NOT NULL, pane_pid INTEGER NOT NULL, proc_start TEXT NOT NULL,\
        scope_kind TEXT NOT NULL CHECK(scope_kind IN ('project', 'global')), project TEXT,\
        CHECK ((scope_kind = 'global' AND project IS NULL) OR\
               (scope_kind = 'project' AND project IS NOT NULL AND length(project) > 0)))",
    )
}

/// Replace one explicitly authorized binding, preserving the caller's transaction boundary.
pub fn set(conn: &Connection, membership: &Membership) -> rusqlite::Result<()> {
    let (kind, project) = match &membership.scope {
        ProjectScope::Project(project)
            if !project.trim().is_empty() && project == project.trim() =>
        {
            ("project", Some(project.as_str()))
        }
        ProjectScope::Global => ("global", None),
        _ => {
            return Err(rusqlite::Error::InvalidParameterName(
                "invalid project".into(),
            ))
        }
    };
    if membership.pane.trim().is_empty() || membership.pid <= 0 || membership.start.is_empty() {
        return Err(rusqlite::Error::InvalidParameterName(
            "invalid pane identity".into(),
        ));
    }
    conn.execute(
        "INSERT INTO lane_project_membership (pane_id,pane_pid,proc_start,scope_kind,project)\
         VALUES (?1,?2,?3,?4,?5) ON CONFLICT(pane_id) DO UPDATE SET\
         pane_pid=excluded.pane_pid,proc_start=excluded.proc_start,\
         scope_kind=excluded.scope_kind,project=excluded.project",
        params![
            membership.pane,
            membership.pid,
            membership.start,
            kind,
            project
        ],
    )?;
    Ok(())
}

/// Return a matching live binding; absent and stale bindings both fail closed.
pub fn get(
    conn: &Connection,
    pane: &str,
    pid: i64,
    start: &str,
) -> rusqlite::Result<Option<Membership>> {
    conn.query_row(
        "SELECT scope_kind, project FROM lane_project_membership WHERE pane_id=?1 AND pane_pid=?2 AND proc_start=?3",
        params![pane, pid, start],
        |row| {
            let kind: String = row.get(0)?;
            let project: Option<String> = row.get(1)?;
            let scope = match (kind.as_str(), project) {
                ("global", None) => ProjectScope::Global,
                ("project", Some(name)) if !name.is_empty() => ProjectScope::Project(name),
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            Ok(Membership { pane: pane.to_owned(), pid, start: start.to_owned(), scope })
        },
    ).optional()
}

/// Whether an entry exists, irrespective of whether its process binding is still live.
pub fn contains(conn: &Connection, pane: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM lane_project_membership WHERE pane_id=?1)",
        [pane],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_survive_reopen_and_stale_identity_is_not_membership() {
        let dir = crate::testsupport::dir("lane-project-store");
        let path = dir.path().join("tasks.db");
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        for (pane, scope) in [
            ("%1", ProjectScope::Project("dunkingsheep".into())),
            ("%2", ProjectScope::Project("poolrad".into())),
            ("%3", ProjectScope::Global),
            ("%4", ProjectScope::Project("global".into())),
        ] {
            set(
                &conn,
                &Membership {
                    pane: pane.into(),
                    pid: 42,
                    start: "123".into(),
                    scope,
                },
            )
            .unwrap();
        }
        drop(conn);
        let conn = Connection::open(path).unwrap();
        assert_eq!(
            get(&conn, "%1", 42, "123").unwrap().unwrap().scope,
            ProjectScope::Project("dunkingsheep".into())
        );
        assert_eq!(
            get(&conn, "%2", 42, "123").unwrap().unwrap().scope,
            ProjectScope::Project("poolrad".into())
        );
        assert_eq!(
            get(&conn, "%3", 42, "123").unwrap().unwrap().scope,
            ProjectScope::Global
        );
        assert_eq!(
            get(&conn, "%4", 42, "123").unwrap().unwrap().scope,
            ProjectScope::Project("global".into())
        );
        assert!(get(&conn, "%1", 43, "123").unwrap().is_none());
        assert!(get(&conn, "%1", 42, "124").unwrap().is_none());
        assert!(get(&conn, "%missing", 42, "123").unwrap().is_none());
    }
}
