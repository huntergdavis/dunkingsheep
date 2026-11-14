//! Opt-in summary history. Uses only SELECTs and never materializes message bodies.
use super::{mailbox_where, map_sqlite, DunkError, ReadPool};
use rusqlite::{types::Value as SqlValue, OptionalExtension};
use serde_json::{json, Value};

pub fn list_page(
    pool: &ReadPool,
    mailbox: &str,
    unread_only: bool,
    limit: i64,
    before: Option<&str>,
    now: f64,
    include_agent: Option<bool>,
) -> Result<Value, DunkError> {
    if !(1..=500).contains(&limit) {
        return Err(DunkError::Refused(
            "paged inbox limit must be 1..500".into(),
        ));
    }
    if before.is_some_and(|id| id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control))
    {
        return Err(DunkError::Refused("invalid inbox cursor".into()));
    }
    let (mailbox_condition, mailbox_params) = mailbox_where(mailbox, "mailbox");
    pool.with(|connection| {
        let mut params = mailbox_params
            .iter()
            .cloned()
            .map(SqlValue::Text)
            .collect::<Vec<_>>();
        let mut condition = format!("{mailbox_condition} AND state != 'cancelled'");
        if let Some(id) = before {
            let mut cursor_params = params.clone();
            cursor_params.push(SqlValue::Text(id.into()));
            let at: Option<f64> = connection.query_row(
                &format!("SELECT created_at FROM messages WHERE {mailbox_condition} AND id = ?"),
                rusqlite::params_from_iter(cursor_params.iter()), |row| row.get(0)
            ).optional().map_err(map_sqlite)?;
            let at = at.ok_or_else(|| DunkError::Refused("inbox cursor unavailable; refresh the mailbox".into()))?;
            condition.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
            params.extend([SqlValue::Real(at), SqlValue::Real(at), SqlValue::Text(id.into())]);
        }
        if let Some(include) = include_agent {
            condition.push_str(if include {
                " AND NOT EXISTS (SELECT 1 FROM message_audiences a WHERE a.message_id = messages.id AND (a.audience IS NULL OR a.audience != 'agent'))"
            } else {
                " AND NOT EXISTS (SELECT 1 FROM message_audiences a WHERE a.message_id = messages.id)"
            });
        }
        if unread_only {
            condition.push_str(" AND state = 'queued'");
        }
        params.push(SqlValue::Integer(limit + 1));
        let sql = format!(
            "SELECT id, mailbox, mailbox_label, sender, subject, state, created_at, \
             length(CAST(body AS BLOB)) AS body_bytes, transport, reply_to, \
             (SELECT COUNT(*) FROM attachments a WHERE a.message_id = messages.id) AS attachment_count, \
             (SELECT audience FROM message_audiences a WHERE a.message_id = messages.id) AS audience \
             FROM messages WHERE {condition} ORDER BY created_at DESC, id DESC LIMIT ?"
        );
        let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
        let mut messages = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                let state: String = row.get("state")?;
                let created_at: f64 = row.get("created_at")?;
                let name: String = row.get("mailbox")?;
                let destination: Option<String> = row.get("mailbox_label")?;
                Ok(json!({
                    "message_id": row.get::<_, String>("id")?,
                    "mailbox": name,
                    "destination": destination.unwrap_or_else(|| name.clone()),
                    "sender": row.get::<_, Option<String>>("sender")?,
                    "subject": row.get::<_, Option<String>>("subject")?,
                    "state": state,
                    "created_at": created_at,
                    "body_bytes": row.get::<_, i64>("body_bytes")?,
                    "transport": row.get::<_, Option<String>>("transport")?,
                    "reply_to": row.get::<_, Option<String>>("reply_to")?,
                    "attachment_count": row.get::<_, i64>("attachment_count")?,
                    "audience": row.get::<_, Option<String>>("audience")?,
                    "waiting_s": if state == "queued" { Some((now-created_at) as i64) } else { None }
                }))
            })
            .map_err(map_sqlite)?
            .collect::<Result<Vec<_>,_>>()
            .map_err(map_sqlite)?;
        let more = messages.len() > limit as usize;
        messages.truncate(limit as usize);
        let next_before = more.then(|| messages.last().unwrap()["message_id"].clone());
        let mut page = json!({"mailbox":mailbox,"messages":messages,"order":"newest_first","next_before":next_before});
        if let Some(include) = include_agent {
            page["include_agent"] = json!(include);
        }
        Ok(page)
    })
}
