//! Opt-in thread pages. Legacy conversation projections remain in owner_queries.
use super::{owner_queries::raw_row, refuse, Store};
use crate::{db::map_sqlite, error::DunkError, owner::ConversationFilter};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub(super) const SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS message_threads_by_view
 ON message_threads(conversation_kind,thread_version,sender_kind,message_id);
CREATE INDEX IF NOT EXISTS message_thread_routes_recipient
 ON message_thread_routes(recipient_kind,message_id);

";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadPageMode {
    Heads,
    Messages,
}
impl ThreadPageMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Heads => "heads",
            Self::Messages => "messages",
        }
    }
}

#[derive(Clone)]
pub struct ThreadPageOptions {
    pub mode: ThreadPageMode,
    pub filter: ConversationFilter,
    pub limit: i64,
    pub root_id: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    mode: String,
    filter: String,
    limit: i64,
    root_id: Option<String>,
    fence: i64,
    view_revision: i64,
    digest: Option<String>,
    anchors: Vec<Anchor>,
    before_at_bits: u64,
    before_id: i64,
}

// Exact SQLite REAL bits avoid a JSON decimal round-trip at the keyset boundary.
type Anchor = (String, String, u64);

fn timestamp_bits(connection: &Connection, id: &str) -> Result<u64, DunkError> {
    let at: f64 = connection
        .query_row("SELECT created_at FROM messages WHERE id=?", [id], |r| {
            r.get(0)
        })
        .map_err(map_sqlite)?;
    if !at.is_finite() {
        return Err(refuse("invalid thread timestamp"));
    }
    Ok(at.to_bits())
}

// Called ONLY for Load older. First-page polling never scans/hashes this metadata.
// The first continuation verifies the already returned prefix before establishing
// its digest; later continuations compare that digest under the same ID fence.
fn snapshot(
    connection: &Connection,
    prefix: &str,
    fence: i64,
    root: Option<&str>,
    bootstrap: bool,
    limit: usize,
) -> Result<(String, Vec<Anchor>), DunkError> {
    let sql = format!("{prefix} SELECT thread_key,id,created_at FROM visible ORDER BY CAST(SUBSTR(id,2) AS INTEGER)");
    let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
    let mut rows = if let Some(root) = root {
        statement.query(params![fence, root])
    } else {
        statement.query(params![fence])
    }
    .map_err(map_sqlite)?;
    let mut hash = Sha256::new();
    let mut latest: HashMap<String, Anchor> = HashMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let key: String = row.get(0).map_err(map_sqlite)?;
        let id: String = row.get(1).map_err(map_sqlite)?;
        let at: f64 = row.get(2).map_err(map_sqlite)?;
        number(&key)?;
        number(&id)?;
        if !at.is_finite() {
            return Err(refuse("invalid thread timestamp"));
        }
        for field in [&key, &id] {
            hash.update((field.len() as u64).to_le_bytes());
            hash.update(field.as_bytes());
        }
        hash.update(at.to_bits().to_le_bytes());
        if bootstrap {
            let anchor = (key.clone(), id, at.to_bits());
            let group = if root.is_some() {
                anchor.1.clone()
            } else {
                key
            };
            let replace = latest
                .get(&group)
                .is_none_or(|old| anchor_order(&anchor, old).is_gt());
            if replace {
                latest.insert(group, anchor);
            }
        }
    }
    let mut anchors: Vec<_> = latest.into_values().collect();
    anchors.sort_by(|a, b| anchor_order(b, a));
    anchors.truncate(limit);
    Ok((format!("{:x}", hash.finalize()), anchors))
}

fn anchor_order(a: &Anchor, b: &Anchor) -> std::cmp::Ordering {
    f64::from_bits(a.2)
        .total_cmp(&f64::from_bits(b.2))
        .then_with(|| {
            number(&a.1)
                .unwrap_or_default()
                .cmp(&number(&b.1).unwrap_or_default())
        })
}

fn reload(options: &ThreadPageOptions, fence: i64, view_revision: i64) -> Value {
    let mut result = json!({"mailbox":"owner","thread_mode":options.mode.name(),"source_filter":options.filter.name(),"include_internal":false,
        "view_revision":view_revision,"fence_id":fence,"next_cursor":null,"reload_from_top":true,"message":"List changed, reload from top."});
    match options.mode {
        ThreadPageMode::Heads => result["threads"] = json!([]),
        ThreadPageMode::Messages => {
            result["root_id"] = json!(options.root_id);
            result["messages"] = json!([]);
        }
    }
    result
}

fn number(id: &str) -> Result<i64, DunkError> {
    if !crate::inbox::valid_message_id(id) {
        return Err(refuse("invalid thread message id"));
    }
    id[1..]
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| refuse("invalid thread message id"))
}

fn eligible(filter: ConversationFilter) -> Result<&'static str, DunkError> {
    match filter {
        ConversationFilter::HumanThreads => Ok(
            "SELECT message_id FROM message_threads WHERE thread_version=1 AND conversation_kind='human'
             UNION ALL SELECT message_id FROM message_threads
             WHERE thread_version=1 AND conversation_kind='vp' AND sender_kind='agent'
             AND message_id IN (SELECT message_id FROM message_thread_routes WHERE recipient_kind='human')",
        ),
        ConversationFilter::VpThreads => Ok(
            "SELECT message_id FROM message_threads WHERE thread_version=1 AND conversation_kind='vp'",
        ),
        ConversationFilter::Historical => Ok(
            "SELECT id AS message_id FROM messages WHERE NOT EXISTS
             (SELECT 1 FROM message_threads t WHERE t.message_id=messages.id AND t.thread_version=1
             AND t.conversation_kind IN ('human','vp'))",
        ),
        _ => Err(refuse("thread pages require human_threads, vp_threads or historical")),
    }
}

fn visible(filter: ConversationFilter, detail: bool) -> Result<String, DunkError> {
    let ids = if detail {
        // Detail starts from the existing root index, not every visible thread.
        match filter {
            ConversationFilter::HumanThreads => "SELECT t.message_id FROM message_threads t
             WHERE t.root_id=?2 AND t.thread_version=1 AND (t.conversation_kind='human' OR
             (t.conversation_kind='vp' AND t.sender_kind='agent' AND EXISTS
             (SELECT 1 FROM message_thread_routes r WHERE r.message_id=t.message_id AND r.recipient_kind='human')))",
            ConversationFilter::VpThreads => "SELECT message_id FROM message_threads
             WHERE root_id=?2 AND thread_version=1 AND conversation_kind='vp'",
            ConversationFilter::Historical => "SELECT id AS message_id FROM messages WHERE id=?2 AND NOT EXISTS
             (SELECT 1 FROM message_threads t WHERE t.message_id=messages.id AND t.thread_version=1
             AND t.conversation_kind IN ('human','vp'))",
            _ => return Err(refuse("invalid thread detail view")),
        }
    } else {
        eligible(filter)?
    };
    let key = if filter == ConversationFilter::Historical {
        "messages.id"
    } else {
        "t.root_id"
    };
    Ok(format!(
        "WITH eligible AS MATERIALIZED ({}), visible AS MATERIALIZED (
         SELECT messages.id,messages.created_at,messages.state,{key} AS thread_key
         FROM eligible JOIN messages ON messages.id=eligible.message_id
         LEFT JOIN message_threads t ON t.message_id=messages.id
         WHERE messages.state!='cancelled' AND CAST(SUBSTR(messages.id,2) AS INTEGER)<=?1)",
        ids
    ))
}

const HEADER: &str = "messages.id,messages.mailbox,messages.mailbox_label,messages.sender,
 messages.subject,SUBSTR(messages.body,1,240) AS preview,messages.state,messages.created_at,
 messages.read_at,messages.acked_at,messages.implicit_ack,messages.transport,messages.reply_to,
 (SELECT audience FROM message_audiences WHERE message_id=messages.id) AS audience,
 t.thread_version,t.parent_id,t.root_id,t.conversation_kind,t.sender_kind,
 r.sender_kind AS sender_route_kind,r.sender_mailbox AS sender_route_mailbox,
 r.recipient_kind AS recipient_route_kind,r.recipient_mailbox AS recipient_route_mailbox,
 COALESCE((SELECT MAX(unresolved_parent) FROM message_thread_refs WHERE message_id=messages.id),0) AS unresolved_parent";

fn annotate(
    connection: &Connection,
    rows: &mut [Value],
    historical: bool,
) -> Result<(), DunkError> {
    let answered: Vec<Value> = rows
        .iter()
        .filter_map(|r| r.get("reply_to"))
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .cloned()
        .collect();
    for row in rows {
        let implicit = row["implicit_ack"]
            .as_bool()
            .unwrap_or_else(|| row["implicit_ack"].as_i64().is_some_and(|v| v != 0));
        row["answered"] = json!(
            answered.contains(&row["id"])
                || row["acked_at"].as_f64().is_some_and(|v| v != 0.0) && !implicit
        );
        row["implicit_ack"] = json!(implicit);
        // Preserve the legacy senderless-reply ambiguity marker; never invent human origin.
        if row["sender"]
            .as_str()
            .is_none_or(|sender| sender.is_empty())
        {
            let ambiguous: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages a JOIN messages q ON q.id=a.reply_to
                 WHERE a.id=? AND a.mailbox='owner' AND q.mailbox='owner'
                 AND (q.reply_to IS NULL OR q.reply_to='') AND (q.sender IS NULL OR q.sender='')
                 AND q.transport IN ('web','telegram'))",
                    [row["id"].as_str().unwrap_or_default()],
                    |r| r.get(0),
                )
                .map_err(map_sqlite)?;
            if ambiguous {
                row["ambiguous_owner_reply"] = Value::Bool(true);
            }
        }
        if historical {
            let fields = [
                "thread_version",
                "parent_id",
                "root_id",
                "conversation_kind",
                "sender_kind",
            ];
            let recorded: serde_json::Map<String, Value> = fields
                .iter()
                .map(|key| ((*key).to_owned(), row[*key].clone()))
                .collect();
            row["historical_reason"] = json!(if recorded["thread_version"].is_null()
                && recorded["conversation_kind"].is_null()
            {
                "prior_version"
            } else if recorded["thread_version"] == 1 && recorded["conversation_kind"] == "other" {
                "other"
            } else {
                "unknown"
            });
            row["historical_lineage"] = Value::Object(recorded);
            for field in fields {
                row[field] = Value::Null;
            }
        }
    }
    Ok(())
}

fn revisions(connection: &Connection) -> Result<(i64, i64), DunkError> {
    connection.query_row(
        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM counters WHERE name='thread_view_revision'),0),
         COALESCE((SELECT CAST(value AS INTEGER) FROM counters WHERE name='message'),
                  (SELECT MAX(CAST(SUBSTR(id,2) AS INTEGER)) FROM messages),0)",
        [], |r| Ok((r.get(0)?,r.get(1)?)),
    ).map_err(map_sqlite)
}

impl Store {
    pub(crate) fn conversation_thread_page(
        &self,
        options: &ThreadPageOptions,
    ) -> Result<Value, DunkError> {
        if !(1..=100).contains(&options.limit) {
            return Err(refuse("thread page limit must be 1..100"));
        }
        let prefix = visible(options.filter, options.mode == ThreadPageMode::Messages)?;
        match (options.mode, options.root_id.as_deref()) {
            (ThreadPageMode::Heads, None) => {}
            (ThreadPageMode::Messages, Some(root)) => {
                number(root)?;
            }
            _ => {
                return Err(refuse(
                    "thread detail requires root_id; thread heads forbid it",
                ))
            }
        }
        let cursor: Option<Cursor> = options
            .cursor
            .as_deref()
            .map(|raw| {
                if raw.len() > 16384 {
                    return Err(refuse("invalid thread page cursor"));
                }
                serde_json::from_str(raw).map_err(|_| refuse("invalid thread page cursor"))
            })
            .transpose()?;
        self.readers.with(|connection| {
            // One snapshot for revisions, ID fence, membership, counts and the selected rows.
            let tx = connection.unchecked_transaction().map_err(map_sqlite)?;
            let (view_revision,current_fence)=revisions(&tx)?;
            if view_revision<0 || current_fence<0 {
                return Err(refuse("thread page metadata unavailable"));
            }
            if let Some(c)=&cursor {
                if c.version!=1 || c.mode!=options.mode.name() || c.filter!=options.filter.name()
                    || c.limit!=options.limit || c.root_id!=options.root_id
                    || c.fence<0 || c.fence>current_fence || c.before_id<=0 || c.before_id>c.fence
                    || !f64::from_bits(c.before_at_bits).is_finite()
                    || c.anchors.len()>options.limit as usize
                    || c.digest.as_ref().is_some_and(|d|d.len()!=64 || !d.bytes().all(|b|b.is_ascii_hexdigit()))
                    || (c.digest.is_none() && c.anchors.len()!=options.limit as usize)
                    || (c.digest.is_some() && !c.anchors.is_empty())
                    || c.anchors.iter().any(|a|number(&a.0).is_err() || number(&a.1).is_err() || !f64::from_bits(a.2).is_finite()) {
                    return Err(refuse("thread page cursor does not match this query"));
                }

            }
            let fence=cursor.as_ref().map_or(current_fence,|c|c.fence);
            let mut digest=None;
            if let Some(c)=&cursor {
                if c.view_revision!=view_revision {return Ok(reload(options,fence,view_revision));}
                let (current,anchors)=snapshot(&tx,&prefix,fence,options.root_id.as_deref(),c.digest.is_none(),options.limit as usize)?;
                if c.digest.as_ref().is_some_and(|old|old!=&current) || (c.digest.is_none() && c.anchors!=anchors) {
                    return Ok(reload(options,fence,view_revision));
                }
                digest=Some(current);
            }
            let before_at=cursor.as_ref().map(|c|f64::from_bits(c.before_at_bits));
            let before_id=cursor.as_ref().map(|c|c.before_id);
            let mut rows = match options.mode {
                ThreadPageMode::Heads => {
                    let sql=format!("{prefix}, ranked AS (
                     SELECT *,COUNT(*) OVER(PARTITION BY thread_key) AS message_count,
                     SUM(state='queued') OVER(PARTITION BY thread_key) AS unread_count,
                     ROW_NUMBER() OVER(PARTITION BY thread_key ORDER BY created_at DESC,CAST(SUBSTR(id,2) AS INTEGER) DESC) AS rank
                     FROM visible)
                     SELECT {HEADER},ranked.thread_key,ranked.message_count,ranked.unread_count
                     FROM ranked JOIN messages ON messages.id=ranked.id
                     LEFT JOIN message_threads t ON t.message_id=messages.id
                     LEFT JOIN message_thread_routes r ON r.message_id=messages.id
                     WHERE rank=1 AND (?2 IS NULL OR ranked.created_at<?2 OR (ranked.created_at=?2 AND CAST(SUBSTR(ranked.id,2) AS INTEGER)<?3))
                     ORDER BY ranked.created_at DESC,CAST(SUBSTR(ranked.id,2) AS INTEGER) DESC LIMIT ?4");
                    let mut stmt=tx.prepare(&sql).map_err(map_sqlite)?;
                    let selected=stmt.query_map(params![fence,before_at,before_id,options.limit+1],raw_row).map_err(map_sqlite)?;
                    selected.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)?
                },
                ThreadPageMode::Messages => {
                    let root=options.root_id.as_deref().unwrap_or_default();
                    let found:bool=tx.query_row(&format!("{prefix} SELECT EXISTS(SELECT 1 FROM visible WHERE thread_key=?2)"),params![fence,root],|r|r.get(0)).map_err(map_sqlite)?;
                    if !found {return Err(refuse("thread is unavailable in this view"));}
                    let columns=super::threads::COLUMNS.replacen('*',"messages.*",1);
                    let sql=format!("{prefix}, page AS (
                     SELECT id FROM visible WHERE thread_key=?2
                     AND (?3 IS NULL OR created_at<?3 OR (created_at=?3 AND CAST(SUBSTR(id,2) AS INTEGER)<?4))
                     ORDER BY created_at DESC,CAST(SUBSTR(id,2) AS INTEGER) DESC LIMIT ?5)
                     SELECT {columns} FROM page JOIN messages ON messages.id=page.id
                     ORDER BY messages.created_at DESC,CAST(SUBSTR(messages.id,2) AS INTEGER) DESC");
                    let mut stmt=tx.prepare(&sql).map_err(map_sqlite)?;
                    let selected=stmt.query_map(params![fence,root,before_at,before_id,options.limit+1],raw_row).map_err(map_sqlite)?;
                    selected.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)?
                }
            };
            let has_more=rows.len()>options.limit as usize;
            rows.truncate(options.limit as usize);
            let next_cursor=if has_more {
                let last=rows.last().ok_or_else(||refuse("missing thread cursor boundary"))?;
                let id=last["id"].as_str().ok_or_else(||refuse("invalid thread row"))?;
                let before_at_bits=timestamp_bits(&tx,id)?;
                let mut anchors=Vec::new();
                if cursor.is_none() {
                    for row in &rows {
                        let row_id=row["id"].as_str().ok_or_else(||refuse("invalid thread row"))?;
                        let key=if options.mode==ThreadPageMode::Heads {row["thread_key"].as_str()} else {options.root_id.as_deref()}.ok_or_else(||refuse("missing thread key"))?;
                        anchors.push((key.into(),row_id.into(),timestamp_bits(&tx,row_id)?));
                    }
                }
                Some(serde_json::to_string(&Cursor{version:1,mode:options.mode.name().into(),filter:options.filter.name().into(),limit:options.limit,root_id:options.root_id.clone(),fence,view_revision,digest,anchors,before_at_bits,before_id:number(id)?}).map_err(DunkError::from)?)
            } else {None};
            annotate(&tx,&mut rows,options.filter==ConversationFilter::Historical)?;
            let mut result=json!({"mailbox":"owner","thread_mode":options.mode.name(),"source_filter":options.filter.name(),"include_internal":false,
                "view_revision":view_revision,"reload_from_top":false,"fence_id":fence,"next_cursor":next_cursor});
            match options.mode {
                ThreadPageMode::Heads => {
                    let mut heads=Vec::with_capacity(rows.len());
                    for mut row in rows {
                        let obj=row.as_object_mut().ok_or_else(||refuse("invalid thread head"))?;
                        let key=obj.remove("thread_key").ok_or_else(||refuse("missing thread key"))?;
                        number(key.as_str().ok_or_else(||refuse("invalid thread key"))?)?;
                        let count=obj.remove("message_count").unwrap_or(Value::Null);
                        let unread=obj.remove("unread_count").unwrap_or(Value::Null);
                        heads.push(json!({"thread_id":key,"latest":row,"message_count":count,"unread_count":unread}));
                    }
                    result["threads"]=json!(heads);
                },
                ThreadPageMode::Messages => {
                    rows.reverse();
                    result["root_id"]=json!(options.root_id);
                    result["messages"]=json!(rows);
                }
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(result)
        })
    }
}

#[cfg(test)]
#[path = "thread_pages_tests.rs"]
mod tests;
