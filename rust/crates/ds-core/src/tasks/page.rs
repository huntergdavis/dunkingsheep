//! Opt-in read-only task pages. Legacy priority ordering and slice semantics stay in wire.rs.
use super::{
    decode_list, decorate_conn, pane_view::decorated_wire, validate_kind, validate_priority,
    validate_status, visible_scopes, Tasks,
};
use crate::{db::map_sqlite, error::DunkError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Default)]
pub struct TaskPageOptions {
    pub scope: Option<String>,
    pub project: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assignee: Option<String>,
    pub kind: Option<String>,
    pub include_done: bool,
    pub needs_owner: bool,
    pub page_size: i64,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    scopes: Vec<String>,
    #[serde(default)]
    project: Option<String>,
    pane: Option<String>,
    statuses: Vec<String>,
    priority: Option<String>,
    assignee: Option<String>,
    kind: Option<String>,
    needs_owner: bool,
    page_size: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    filter: Filter,
    after: String,
}

fn refused(message: &str) -> DunkError {
    DunkError::Refused(message.into())
}

impl Tasks {
    pub async fn list_page(
        &self,
        options: TaskPageOptions,
        pane: Option<&str>,
    ) -> Result<Value, DunkError> {
        if !(1..=100).contains(&options.page_size) {
            return Err(refused("task page_size must be between 1 and 100"));
        }
        let statuses = match options.status.as_deref().filter(|s| !s.is_empty()) {
            Some("active") => vec!["open".into(), "in_progress".into(), "blocked".into()],
            Some("closed") => vec!["done".into(), "cancelled".into()],
            Some(status) => vec![validate_status(status)?],
            None if !options.include_done => {
                vec!["open".into(), "in_progress".into(), "blocked".into()]
            }
            None => vec![],
        };
        let filter = Filter {
            scopes: visible_scopes(options.scope.as_deref(), pane)?,
            project: options.project,
            pane: pane.map(str::to_owned),
            statuses,
            priority: options
                .priority
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(validate_priority)
                .transpose()?,
            assignee: options
                .assignee
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_owned()),
            kind: options
                .kind
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(validate_kind)
                .transpose()?,
            needs_owner: options.needs_owner,
            page_size: options.page_size,
        };
        let after = match options.cursor {
            Some(raw) => {
                if raw.len() > 8192 {
                    return Err(refused("invalid task page cursor"));
                }
                let cursor: Cursor =
                    serde_json::from_str(&raw).map_err(|_| refused("invalid task page cursor"))?;
                if cursor.version != 1 || cursor.filter != filter || cursor.after.is_empty() {
                    return Err(refused("task page cursor does not match this query"));
                }
                Some(cursor.after)
            }
            None => None,
        };
        self.writer
            .call(move |connection| {
                let tx = connection.transaction().map_err(map_sqlite)?;
                let now = crate::clock::now_s();
                let mut sql = format!(
                    "SELECT id, assignees FROM tasks WHERE scope IN ({})",
                    vec!["?"; filter.scopes.len()].join(",")
                );
                let mut binds = filter.scopes.clone();
                if !filter.statuses.is_empty() {
                    sql.push_str(&format!(
                        " AND status IN ({})",
                        vec!["?"; filter.statuses.len()].join(",")
                    ));
                    binds.extend(filter.statuses.iter().cloned());
                }
                for (column, value) in [("priority", &filter.priority), ("kind", &filter.kind)] {
                    if let Some(value) = value {
                        sql.push_str(&format!(" AND {column}=?"));
                        binds.push(value.clone());
                    }
                }
                if let Some(project) = &filter.project {
                    sql.push_str(" AND id IN (SELECT task_id FROM planner_task_projects WHERE project=?)");
                    binds.push(project.clone());
                }
                if filter.needs_owner {
                    sql.push_str(" AND status='blocked' AND block_reason LIKE 'needs-owner:%'");
                }
                sql.push_str(" ORDER BY id COLLATE BINARY");
                // Scan only IDs/assignees for exact totals and the legacy assignee decoding.
                // Full task bodies/dependencies/leases are decorated only for the bounded page.
                let mut statement = tx.prepare(&sql).map_err(map_sqlite)?;
                let rows = statement
                    .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                    })
                    .map_err(map_sqlite)?;
                let mut total = 0usize;
                let mut selected = Vec::new();
                for row in rows {
                    let (id, assignees) = row.map_err(map_sqlite)?;
                    if filter
                        .assignee
                        .as_ref()
                        .is_some_and(|a| !decode_list(assignees.as_deref()).contains(a))
                    {
                        continue;
                    }
                    total += 1;
                    if after.as_ref().is_none_or(|after| id > *after)
                        && selected.len() <= filter.page_size as usize
                    {
                        selected.push(id);
                    }
                }
                let has_more = selected.len() > filter.page_size as usize;
                selected.truncate(filter.page_size as usize);
                let next_cursor = if has_more {
                    Some(
                        serde_json::to_string(&Cursor {
                            version: 1,
                            filter: filter.clone(),
                            after: selected.last().expect("nonempty page").clone(),
                        })
                        .map_err(|_| refused("cannot encode task page cursor"))?,
                    )
                } else {
                    None
                };
                let mut tasks = Vec::with_capacity(selected.len());
                for id in selected {
                    let decorated = decorate_conn(&tx, &id, filter.pane.as_deref(), now)?;
                    tasks.push(decorated_wire(&tx, &decorated, now)?);
                }
                Ok(
                    json!({"paging_version":1,"order":"id_asc","tasks":tasks,"total":total,
                "project":filter.project,"page_size":filter.page_size,"next_cursor":next_cursor,"checked_at":now}),
                )
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> TaskPageOptions {
        TaskPageOptions {
            page_size: 37,
            include_done: true,
            status: Some("closed".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn project_filter_is_applied_before_paging_and_bound_to_cursor() {
        let tasks = crate::tasks::wire::tests::fixture().await;
        tasks.writer.call(|conn| {
            for (id, project) in [("t800", "alpha"), ("t801", "beta"), ("t802", "alpha")] {
                conn.execute("INSERT INTO tasks(id,scope,title,status,priority,created_at,updated_at) VALUES(?,'global','scoped','open','high',10,11)", [id]).map_err(map_sqlite)?;
                conn.execute("INSERT INTO planner_task_projects(task_id,project) VALUES(?,?)", [id,project]).map_err(map_sqlite)?;
            }
            Ok(())
        }).await.unwrap();
        let query = || TaskPageOptions {
            scope: Some("global".into()),
            project: Some("alpha".into()),
            page_size: 1,
            ..Default::default()
        };
        let first = tasks.list_page(query(), Some("%4")).await.unwrap();
        assert_eq!(first["total"], 2);
        assert_eq!(first["tasks"][0]["id"], "t800");
        let mut next = query();
        next.cursor = first["next_cursor"].as_str().map(str::to_owned);
        let second = tasks.list_page(next, Some("%4")).await.unwrap();
        assert_eq!(second["tasks"][0]["id"], "t802");
        assert!(second["next_cursor"].is_null());
        let mut wrong = query();
        wrong.project = Some("beta".into());
        wrong.cursor = first["next_cursor"].as_str().map(str::to_owned);
        assert!(tasks.list_page(wrong, Some("%4")).await.is_err());
    }

    #[tokio::test]
    async fn task_pages_cover_more_than_200_once_with_filters_and_no_writes() {
        let tasks = crate::tasks::wire::tests::fixture().await;
        tasks.writer.call(|conn| {
            let tx = conn.transaction().map_err(map_sqlite)?;
            for number in 1000..1300 {
                tx.execute(r#"INSERT INTO tasks(id,scope,title,status,priority,assignees,created_at,updated_at) VALUES(?,'global','fixture','done','high','["%4"]',10,11)"#, [format!("t{number}")]).map_err(map_sqlite)?;
            }
            tx.commit().map_err(map_sqlite)
        }).await.unwrap();
        let events = tasks.events("t2").await;
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let mut query = options();
            query.cursor = cursor;
            let page = tasks.list_page(query, Some("%4")).await.unwrap();
            assert_eq!(page["total"], 302);
            assert_eq!(page["paging_version"], 1);
            let rows = page["tasks"].as_array().unwrap();
            assert!(rows.len() <= 37);
            seen.extend(
                rows.iter()
                    .map(|row| row["id"].as_str().unwrap().to_owned()),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 302);
        assert!(
            seen.windows(2).all(|ids| ids[0] < ids[1]),
            "stable binary ID order without duplicates"
        );
        let mut query = options();
        query.priority = Some("high".into());
        query.assignee = Some("%4".into());
        assert_eq!(
            tasks.list_page(query, Some("%4")).await.unwrap()["total"],
            300
        );
        let mut query = options();
        query.status = Some("active".into());
        let active = tasks.list_page(query, Some("%4")).await.unwrap();
        let rows = active["tasks"].as_array().unwrap();
        assert!(rows.iter().any(|r| r["id"] == "t5"));
        assert!(!rows.iter().any(|r| r["scope"] == "%99"));
        assert_eq!(
            tasks.events("t2").await,
            events,
            "paging adds no task events"
        );
        let mut query = options();
        query.priority = Some("urgent".into());
        let empty = tasks.list_page(query, None).await.unwrap();
        assert_eq!(empty["total"], 0);
        assert!(empty["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn task_page_cursors_bind_filters_caller_and_size_and_reject_bad_bounds() {
        let tasks = crate::tasks::wire::tests::fixture().await;
        let mut query = options();
        query.page_size = 1;
        let page = tasks.list_page(query, Some("%4")).await.unwrap();
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        for change in 0..8 {
            let mut query = options();
            query.page_size = 1;
            query.cursor = Some(cursor.clone());
            let mut pane = Some("%4");
            match change {
                0 => query.status = Some("open".into()),
                1 => query.priority = Some("low".into()),
                2 => query.assignee = Some("%4".into()),
                3 => query.scope = Some("global".into()),
                4 => query.kind = Some("review".into()),
                5 => query.needs_owner = true,
                6 => query.page_size = 2,
                _ => pane = Some("%99"),
            }
            assert!(tasks
                .list_page(query, pane)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match"));
        }
        let mut modified: Value = serde_json::from_str(&cursor).unwrap();
        modified["version"] = json!(2);
        let mut query = options();
        query.page_size = 1;
        query.cursor = Some(modified.to_string());
        assert!(tasks.list_page(query, Some("%4")).await.is_err());
        // A deleted prior boundary still permits forward progress by its stable ID.
        tasks
            .writer
            .call(|conn| {
                conn.execute("DELETE FROM tasks WHERE id='t7'", [])
                    .map_err(map_sqlite)
            })
            .await
            .unwrap();
        let mut query = options();
        query.page_size = 1;
        query.cursor = Some(cursor);
        let remaining = tasks.list_page(query, Some("%4")).await.unwrap();
        assert_eq!(remaining["tasks"][0]["id"], "t8");
        assert!(remaining["next_cursor"].is_null());
        for size in [-1, 0, 101, i64::MAX] {
            let mut query = options();
            query.page_size = size;
            assert!(tasks.list_page(query, None).await.is_err());
        }
        for cursor in ["", "[]", "{}", "not-json"] {
            let mut query = options();
            query.cursor = Some(cursor.into());
            assert!(tasks.list_page(query, None).await.is_err());
        }
        let mut query = options();
        query.cursor = Some("x".repeat(8193));
        assert!(tasks.list_page(query, None).await.is_err());
    }
}
