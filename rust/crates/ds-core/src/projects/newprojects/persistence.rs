use std::path::Path;

use rusqlite::{params, OptionalExtension};

use crate::db::Writer;
use crate::error::DunkError;
use crate::projects::{Event, Project};

/// A project-owned store. Its path must come from approved project configuration; this type
/// never derives a path from a project name or writes into the project directory.
pub struct ProjectStore {
    writer: Writer,
}

impl std::fmt::Debug for ProjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectStore")
            .field("path", &self.writer.path())
            .finish()
    }
}

impl ProjectStore {
    pub async fn open(path: &Path) -> Result<Self, DunkError> {
        let writer = Writer::spawn(path)?;
        writer
            .call(|connection| {
                connection
                    .execute_batch(
                        "CREATE TABLE IF NOT EXISTS project_state (
                            name TEXT PRIMARY KEY NOT NULL,
                            payload TEXT NOT NULL
                         );
                         CREATE TABLE IF NOT EXISTS project_events (
                            project_name TEXT NOT NULL,
                            sequence INTEGER NOT NULL,
                            step TEXT NOT NULL,
                            ok INTEGER NOT NULL,
                            detail TEXT NOT NULL,
                            PRIMARY KEY(project_name, sequence)
                         );",
                    )
                    .map_err(map_sqlite)?;
                Ok(())
            })
            .await?;
        Ok(Self { writer })
    }

    pub async fn load(&self) -> Result<Vec<Project>, DunkError> {
        self.writer
            .call(|connection| {
                let mut statement = connection
                    .prepare("SELECT payload FROM project_state ORDER BY name")
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(map_sqlite)?;
                let mut projects = Vec::new();
                for row in rows {
                    let payload = row.map_err(map_sqlite)?;
                    projects.push(serde_json::from_str(&payload).map_err(|error| {
                        DunkError::Refused(format!("invalid persisted project state: {error}"))
                    })?);
                }
                Ok(projects)
            })
            .await
    }

    /// State and its event append commit together. The per-project sequence makes retries
    /// idempotent, while the awaited writer reply makes disk failure visible to the caller.
    pub async fn save(
        &self,
        project: Project,
        event: Option<(i64, Event)>,
    ) -> Result<(), DunkError> {
        let payload = serde_json::to_string(&project)?;
        self.writer
            .call(move |connection| {
                let tx = connection.transaction().map_err(map_sqlite)?;
                tx.execute(
                    "INSERT INTO project_state(name, payload) VALUES (?1, ?2)
                     ON CONFLICT(name) DO UPDATE SET payload=excluded.payload",
                    params![project.name, payload],
                )
                .map_err(map_sqlite)?;
                if let Some((sequence, event)) = event {
                    let exists: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM project_events WHERE project_name=?1 AND sequence=?2",
                            params![project.name, sequence],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(map_sqlite)?;
                    if exists.is_none() {
                        tx.execute(
                            "INSERT INTO project_events(project_name, sequence, step, ok, detail)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![project.name, sequence, event.step, event.ok, event.detail],
                        )
                        .map_err(map_sqlite)?;
                    }
                }
                tx.commit().map_err(map_sqlite)
            })
            .await
    }

    pub async fn events(&self, project: String) -> Result<Vec<Event>, DunkError> {
        self.writer
            .call(move |connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT step, ok, detail FROM project_events
                         WHERE project_name=?1 ORDER BY sequence",
                    )
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map([project], |row| {
                        Ok(Event {
                            step: row.get(0)?,
                            ok: row.get(1)?,
                            detail: row.get(2)?,
                        })
                    })
                    .map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
            })
            .await
    }

    pub async fn shutdown(&self) {
        self.writer.shutdown().await;
    }
}

fn map_sqlite(error: rusqlite::Error) -> DunkError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::DatabaseBusy
                || code.code == rusqlite::ErrorCode::DatabaseLocked =>
        {
            DunkError::Busy
        }
        other => DunkError::Refused(format!("project store: {other}")),
    }
}
