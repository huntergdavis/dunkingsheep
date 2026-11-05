//! Source-backed history for the desk operations metrics endpoint.
//!
//! This reader does not implement collection, summary, board refresh, or startup.
//! The caller supplies the existing operations database path and an explicit clock.
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::{types::ValueRef, Connection, OpenFlags, ToSql};
use serde_json::{json, Map, Number, Value};
use std::path::Path;

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

/// Query the configured existing database. Values outside 1..=168 are rejected.
/// This explicitly does not open or initialize the live database by default.
pub fn history(path: &Path, now: DateTime<Utc>, hours: u16) -> rusqlite::Result<Value> {
    if !(1..=168).contains(&hours) {
        return Err(rusqlite::Error::InvalidParameterName(
            "history window must be 1 to 168 hours".into(),
        ));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
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
    }

    #[test]
    fn missing_database_invalid_window_and_missing_schema_fail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sqlite3");
        let now = Utc::now();
        assert!(history(&path, now, 0).is_err());
        assert!(history(&path, now, 169).is_err());
        assert!(history(&path, now, 24).is_err());
        assert!(!path.exists());
        drop(Connection::open(&path).unwrap());
        assert!(history(&path, now, 24).is_err());
    }
}
