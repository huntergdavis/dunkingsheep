//! Source-backed history for the desk operations metrics endpoint.
//!
//! This reader does not implement collection, summary, board refresh, or startup.
//! The caller supplies the existing operations database path and an explicit clock.
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{types::ValueRef, Connection, ToSql};
use serde_json::{json, Map, Number, Value};
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

fn io_error(error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    if !columns.iter().any(|name| name == column) {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}

fn connect(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(io_error)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
    conn.execute_batch(include_str!("ops_schema.sql"))?;
    for (table, column, declaration) in [
        (
            "collection_runs",
            "accounting_state",
            "TEXT NOT NULL DEFAULT 'unavailable'",
        ),
        (
            "accounting_snapshots",
            "source_signature",
            "TEXT NOT NULL DEFAULT ''",
        ),
        (
            "collection_runs",
            "agent_messages_state",
            "TEXT NOT NULL DEFAULT 'unavailable'",
        ),
        ("pane_samples", "agent_name", "TEXT"),
        ("pane_samples", "lane_name", "TEXT"),
        ("agent_message_events", "sender_alias", "TEXT"),
        ("agent_message_events", "mailbox_alias", "TEXT"),
        ("agent_message_events", "message_bytes", "INTEGER"),
        ("agent_message_events", "duration_ms", "INTEGER"),
        ("agent_message_events", "attempts", "INTEGER"),
    ] {
        add_column_if_missing(&conn, table, column, declaration)?;
    }
    Ok(conn)
}

fn rows(conn: &Connection, sql: &str, args: &[&dyn ToSql]) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let rows = stmt.query_map(args, |row| {
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => {
                    Value::Number(Number::from_f64(value).ok_or_else(|| {
                        rusqlite::Error::InvalidColumnType(
                            index,
                            name.clone(),
                            rusqlite::types::Type::Real,
                        )
                    })?)
                }
                ValueRef::Text(value) => Value::String(
                    std::str::from_utf8(value)
                        .map_err(|_| {
                            rusqlite::Error::InvalidColumnType(
                                index,
                                name.clone(),
                                rusqlite::types::Type::Text,
                            )
                        })?
                        .to_owned(),
                ),
                ValueRef::Blob(_) => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        index,
                        name.clone(),
                        rusqlite::types::Type::Blob,
                    ))
                }
            };
            object.insert(name.clone(), value);
        }
        Ok(Value::Object(object))
    })?;
    rows.collect()
}

/// Query the explicitly configured database. Values outside 1..=168 are rejected.
pub fn history(path: &Path, now: DateTime<Utc>, hours: u16) -> rusqlite::Result<Value> {
    if !(1..=168).contains(&hours) {
        return Err(rusqlite::Error::InvalidParameterName(
            "history window must be 1 to 168 hours".into(),
        ));
    }
    let conn = connect(path)?;
    let generated = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let start =
        (now - Duration::hours(i64::from(hours))).to_rfc3339_opts(SecondsFormat::Secs, true);
    let project = "poolrad";
    let goals = rows(&conn, "SELECT goal_key,observed_at,value,unit,evidence_ref FROM goal_observations WHERE project=? AND goal_key IN ('strict_fullc','verified_game_identities') AND status='verified' AND observed_at>=? ORDER BY observed_at,id", &[&project, &start])?;
    let coders = rows(&conn, "SELECT r.observed_at, COUNT(p.pane_id) AS tracked, SUM(CASE WHEN p.agent_status='working' THEN 1 ELSE 0 END) AS working FROM collection_runs r JOIN pane_samples p ON p.run_id=r.id WHERE r.observed_at>=? AND r.panes_state='ok' AND p.project=? AND p.role='coder' GROUP BY r.id ORDER BY r.observed_at", &[&start, &project])?;
    let merges = rows(&conn, "SELECT number,merged_at FROM pr_latest WHERE project=? AND merged_at>=? ORDER BY merged_at,number", &[&project, &start])?;
    let accounting = rows(&conn, "SELECT observed_at,strict_full_c,verified_game_identities,ceiling,bound_kind,commit_oid FROM accounting_snapshots WHERE project=? AND observed_at>=? ORDER BY observed_at,id", &[&project, &start])?;
    Ok(
        json!({"generated_at":generated,"window_start":start,"window_hours":hours,"goal_observations":goals,"accounting_samples":accounting,"coder_samples":coders,"pr_merges":merges,"limits":{"coder_samples":"Pane presence, not verified productive work; gaps mean no successful sample.","pr_merges":"Recent visible PRs only; merges are not deployments or strict C credit.","dora":"Deployment frequency, change failure rate and recovery time unavailable without deployment and incident records."}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn actual_sqlite_history_preserves_filter_order_and_nulls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE goal_observations(id INTEGER, project TEXT, goal_key TEXT, observed_at TEXT, value REAL, unit TEXT, evidence_ref TEXT, status TEXT);
CREATE TABLE collection_runs(id INTEGER, observed_at TEXT, panes_state TEXT);
CREATE TABLE pane_samples(run_id INTEGER, pane_id TEXT, agent_status TEXT, project TEXT, role TEXT);
CREATE TABLE pr_latest(project TEXT, number INTEGER, merged_at TEXT);
CREATE TABLE accounting_snapshots(id INTEGER, project TEXT, observed_at TEXT, strict_full_c INTEGER, verified_game_identities INTEGER, ceiling INTEGER, bound_kind TEXT, commit_oid TEXT);
INSERT INTO goal_observations VALUES(1,'poolrad','strict_fullc','2026-01-02T11:00:00Z',42.5,'pct',NULL,'verified');
INSERT INTO goal_observations VALUES(2,'poolrad','strict_fullc','2026-01-02T11:30:00Z',99,'pct',NULL,'unverified');
INSERT INTO goal_observations VALUES(3,'other','strict_fullc','2026-01-02T11:00:00Z',99,'pct',NULL,'verified');
INSERT INTO collection_runs VALUES(1,'2026-01-02T11:15:00Z','ok');
INSERT INTO pane_samples VALUES(1,'a','working','poolrad','coder'),(1,'b','idle','poolrad','coder'),(1,'c','working','poolrad','reviewer');
INSERT INTO pr_latest VALUES('poolrad',12,'2026-01-02T10:00:00Z'),('poolrad',3,'2026-01-02T10:00:00Z'),('other',1,'2026-01-02T10:00:00Z');
INSERT INTO accounting_snapshots VALUES(1,'poolrad','2026-01-02T09:00:00Z',1,2,NULL,'upper','abc');").unwrap();
        drop(conn);
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap();
        let result = history(&path, now, 24).unwrap();
        assert_eq!(result["generated_at"], "2026-01-02T12:00:00Z");
        assert_eq!(result["window_start"], "2026-01-01T12:00:00Z");
        assert_eq!(
            result["goal_observations"],
            json!([{"goal_key":"strict_fullc","observed_at":"2026-01-02T11:00:00Z","value":42.5,"unit":"pct","evidence_ref":null}])
        );
        assert_eq!(
            result["coder_samples"],
            json!([{"observed_at":"2026-01-02T11:15:00Z","tracked":2,"working":1}])
        );
        assert_eq!(
            result["pr_merges"],
            json!([{"number":3,"merged_at":"2026-01-02T10:00:00Z"},{"number":12,"merged_at":"2026-01-02T10:00:00Z"}])
        );
        assert_eq!(result["accounting_samples"][0]["ceiling"], Value::Null);
        let reopened = Connection::open(&path).unwrap();
        let mut stmt = reopened
            .prepare("PRAGMA table_info(collection_runs)")
            .unwrap();
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.contains(&"accounting_state".to_owned()));
        assert!(columns.contains(&"agent_messages_state".to_owned()));
        assert_eq!(
            reopened
                .query_row("SELECT COUNT(*) FROM collection_runs", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn missing_database_is_initialized_privately_and_invalid_window_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sqlite3");
        let now = Utc::now();
        assert!(history(&path, now, 0).is_err());
        assert!(history(&path, now, 169).is_err());
        assert!(!path.exists());
        let result = history(&path, now, 24).unwrap();
        assert_eq!(result["goal_observations"], json!([]));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(history(&path, now, 24).unwrap(), result);
    }
}
