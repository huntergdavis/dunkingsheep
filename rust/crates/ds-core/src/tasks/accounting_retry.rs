//! Provenance for retrying auto-merge on an accounting PR without recreating it.
use super::Tasks;
use crate::{db, error::DunkError};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

impl Tasks {
    /// Only an accepted, completed body's stored accounting link and the latest review-sync
    /// accounting event may supply the exact head. Missing/ambiguous provenance holds.
    pub async fn accounting_review(
        &self,
        repo: &str,
        number: i64,
    ) -> Result<Option<(String, String)>, DunkError> {
        let repo = repo.to_owned();
        self.writer.call(move |conn| {
            let ids = {
                let mut stmt = conn.prepare("SELECT id FROM tasks WHERE kind='review' AND scope='global' AND repo=?1 AND accounting_pr=?2 AND status='done' AND verdict='accept' ORDER BY id LIMIT 2").map_err(db::map_sqlite)?;
                let rows = stmt.query_map(params![repo, number], |row| row.get::<_, String>(0)).map_err(db::map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)?
            };
            if ids.len() != 1 { return Ok(None); }
            let id = &ids[0];
            let meta: Option<String> = conn.query_row("SELECT meta FROM task_events WHERE task_id=?1 AND event='accounting' AND actor='review-sync' ORDER BY id DESC LIMIT 1", params![id], |row| row.get(0)).optional().map_err(db::map_sqlite)?.flatten();
            let Some(meta) = meta.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) else { return Ok(None); };
            if meta["accounting_pr"].as_i64() != Some(number) || meta["review"] != "approved_by_author_app" { return Ok(None); }
            Ok(meta["accounting_head"].as_str().filter(|head| !head.is_empty())
                .map(|head| (id.clone(), head.to_owned())))
        }).await
    }
}
