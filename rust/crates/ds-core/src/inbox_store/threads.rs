//! Versioned lineage for new messages. Legacy message rows are never backfilled.
//! The writer slice owns validation/insertion; view reads use only these recorded facts.

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS message_threads (
    message_id TEXT PRIMARY KEY,
    thread_version INTEGER NOT NULL CHECK(thread_version = 1),
    parent_id TEXT,
    root_id TEXT NOT NULL,
    conversation_kind TEXT NOT NULL CHECK(conversation_kind IN ('human', 'vp', 'other')),
    sender_kind TEXT NOT NULL CHECK(sender_kind IN ('human', 'agent', 'system'))
);
CREATE TABLE IF NOT EXISTS message_thread_routes (
    message_id TEXT PRIMARY KEY,
    sender_kind TEXT NOT NULL CHECK(sender_kind IN ('human','vp','lane','none')),
    sender_mailbox TEXT,
    recipient_kind TEXT NOT NULL CHECK(recipient_kind IN ('human','vp','lane','none')),
    recipient_mailbox TEXT
);
CREATE TRIGGER IF NOT EXISTS message_thread_routes_delete AFTER DELETE ON messages
BEGIN DELETE FROM message_thread_routes WHERE message_id = OLD.id; END;
CREATE TABLE IF NOT EXISTS message_thread_refs (
    transport TEXT NOT NULL,
    channel TEXT NOT NULL,
    external_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    parent_external_id TEXT,
    unresolved_parent INTEGER NOT NULL CHECK(unresolved_parent IN (0,1)),
    PRIMARY KEY(transport, channel, external_id)
);
CREATE INDEX IF NOT EXISTS message_thread_refs_message ON message_thread_refs(message_id);
CREATE TRIGGER IF NOT EXISTS message_thread_refs_delete AFTER DELETE ON messages
BEGIN DELETE FROM message_thread_refs WHERE message_id = OLD.id; END;
CREATE INDEX IF NOT EXISTS message_threads_root ON message_threads(root_id, message_id);
CREATE INDEX IF NOT EXISTS message_threads_parent ON message_threads(parent_id);
CREATE TRIGGER IF NOT EXISTS message_threads_delete AFTER DELETE ON messages
BEGIN DELETE FROM message_threads WHERE message_id = OLD.id; END;
";

/// Correlated projections leave legacy wire responses untouched; used only by the new views.
pub(super) const COLUMNS: &str = "
    *, (SELECT audience FROM message_audiences WHERE message_id = messages.id) AS audience,
    (SELECT thread_version FROM message_threads WHERE message_id = messages.id) AS thread_version,
    (SELECT parent_id FROM message_threads WHERE message_id = messages.id) AS parent_id,
    (SELECT root_id FROM message_threads WHERE message_id = messages.id) AS root_id,
    (SELECT conversation_kind FROM message_threads WHERE message_id = messages.id) AS conversation_kind,
    (SELECT sender_kind FROM message_threads WHERE message_id = messages.id) AS sender_kind,
    COALESCE((SELECT value FROM counters WHERE name = 'thread_view_revision'), 0) AS view_revision,
    COALESCE((SELECT MAX(unresolved_parent) FROM message_thread_refs WHERE message_id = messages.id), 0) AS unresolved_parent,
    (SELECT sender_kind FROM message_thread_routes WHERE message_id = messages.id) AS sender_route_kind,
    (SELECT sender_mailbox FROM message_thread_routes WHERE message_id = messages.id) AS sender_route_mailbox,
    (SELECT recipient_kind FROM message_thread_routes WHERE message_id = messages.id) AS recipient_route_kind,
    (SELECT recipient_mailbox FROM message_thread_routes WHERE message_id = messages.id) AS recipient_route_mailbox
";

use crate::{db::map_sqlite, error::DunkError, inbox::normalise_mailbox};
use rusqlite::{OptionalExtension, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadKind {
    Human,
    Vp,
    Other,
}
impl ThreadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Vp => "vp",
            Self::Other => "other",
        }
    }
    fn parse(value: &str) -> Result<Self, DunkError> {
        match value {
            "human" => Ok(Self::Human),
            "vp" => Ok(Self::Vp),
            "other" => Ok(Self::Other),
            _ => Err(refuse("invalid stored thread scope")),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSender {
    Human,
    Agent,
    System,
}
impl ThreadSender {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// A canonical reply address captured at write time, independent of reusable pane IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadRoute {
    Human,
    Vp,
    Lane(String),
    None,
}
impl ThreadRoute {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Vp => "vp",
            Self::Lane(_) => "lane",
            Self::None => "none",
        }
    }
    pub fn mailbox(&self) -> Option<&str> {
        match self {
            Self::Lane(mailbox) => Some(mailbox),
            _ => None,
        }
    }
    fn from_parts(kind: &str, mailbox: Option<String>) -> Result<Self, DunkError> {
        match (kind, mailbox) {
            ("human", _) => Ok(Self::Human),
            ("vp", _) => Ok(Self::Vp),
            ("lane", Some(mailbox)) if !mailbox.trim().is_empty() => Ok(Self::Lane(mailbox)),
            ("none", _) => Ok(Self::None),
            _ => Err(refuse("invalid recorded reply route")),
        }
    }
}

/// Authenticated transport coordinates, never a body/subject-based match.
#[derive(Debug, Clone)]
pub struct ThreadTransportRef {
    pub transport: String,
    pub channel: String,
    pub external_id: String,
    pub parent_external_id: Option<String>,
}

/// Supplied by trusted daemon operations, not deserialized from inbox-send arguments.
#[derive(Debug, Clone)]
pub struct ThreadWrite {
    pub kind: ThreadKind,
    pub sender: ThreadSender,
    pub sender_route: ThreadRoute,
    pub recipient_route: ThreadRoute,
    /// Human ingress replies derive the peer atomically from the recorded parent routes.
    pub route_reply: bool,
    pub parent_id: Option<String>,
    /// None is the existing privileged mailbox-read grant. Ordinary lanes name only their own.
    pub allowed_parent_mailboxes: Option<Vec<String>>,
    pub dedupe_external_id: bool,
    pub transport_ref: Option<ThreadTransportRef>,
}
impl ThreadWrite {
    pub fn root(kind: ThreadKind, sender: ThreadSender) -> Self {
        Self {
            kind,
            sender,
            sender_route: ThreadRoute::None,
            recipient_route: ThreadRoute::None,
            route_reply: false,
            parent_id: None,
            allowed_parent_mailboxes: None,
            dedupe_external_id: false,
            transport_ref: None,
        }
    }
}
fn refuse(message: &str) -> DunkError {
    DunkError::Refused(message.to_owned())
}

pub(super) fn existing(
    tx: &Transaction<'_>,
    mailbox: &str,
    external_id: Option<&str>,
    transport: Option<&str>,
    context: &ThreadWrite,
) -> Result<Option<String>, DunkError> {
    if let Some(reference) = &context.transport_ref {
        let found = tx.query_row("SELECT r.message_id FROM message_thread_refs r JOIN messages m ON m.id=r.message_id WHERE r.transport=? AND r.channel=? AND r.external_id=?",
            rusqlite::params![reference.transport,reference.channel,reference.external_id], |r|r.get(0)).optional().map_err(map_sqlite)?;
        if found.is_some() {
            return Ok(found);
        }
    }
    if context.dedupe_external_id {
        if let Some(external_id) = external_id.filter(|id| !id.is_empty()) {
            if context.sender == ThreadSender::Human {
                let found=tx.query_row("SELECT m.id FROM messages m JOIN message_threads t ON t.message_id=m.id WHERE m.external_id=? AND m.transport IS ? AND t.sender_kind='human' ORDER BY m.created_at LIMIT 1",rusqlite::params![external_id,transport],|r|r.get(0)).optional().map_err(map_sqlite)?;
                if found.is_some() {
                    return Ok(found);
                }
                // A redelivered pre-cutover update keeps its original unversioned row.
                return tx.query_row("SELECT m.id FROM messages m WHERE m.mailbox=? AND m.external_id=? AND m.transport IS ? AND NOT EXISTS(SELECT 1 FROM message_threads t WHERE t.message_id=m.id) ORDER BY m.created_at LIMIT 1",rusqlite::params![mailbox,external_id,transport],|r|r.get(0)).optional().map_err(map_sqlite);
            }
            return tx.query_row("SELECT id FROM messages WHERE mailbox=? AND external_id=? ORDER BY created_at LIMIT 1", rusqlite::params![mailbox,external_id], |r|r.get(0)).optional().map_err(map_sqlite);
        }
    }
    Ok(None)
}

pub(super) struct Prepared {
    pub parent: Option<String>,
    sender_route: ThreadRoute,
    recipient_route: ThreadRoute,
    route_reply: bool,
    pub(super) parent_transport: Option<String>,
    root: String,
    kind: ThreadKind,
    sender: ThreadSender,
    unresolved_external_parent: bool,
}
pub(super) fn prepare(
    tx: &Transaction<'_>,
    id: &str,
    context: &ThreadWrite,
) -> Result<Prepared, DunkError> {
    let mut parent = context.parent_id.clone();
    let mut unresolved = false;
    if let Some(reference) = &context.transport_ref {
        for value in [
            &reference.transport,
            &reference.channel,
            &reference.external_id,
        ]
        .into_iter()
        .chain(reference.parent_external_id.iter())
        {
            if value.trim().is_empty() || value.len() > 160 || value.chars().any(char::is_control) {
                return Err(refuse("invalid transport message reference"));
            }
        }
        if let Some(external_parent) = &reference.parent_external_id {
            if parent.is_some() {
                return Err(refuse("choose an internal or transport parent, not both"));
            }
            parent=tx.query_row("SELECT r.message_id FROM message_thread_refs r JOIN messages m ON m.id=r.message_id WHERE r.transport=? AND r.channel=? AND r.external_id=?",
                rusqlite::params![reference.transport,reference.channel,external_parent],|r|r.get(0)).optional().map_err(map_sqlite)?;
            unresolved = parent.is_none();
        }
    }
    let mut result = Prepared {
        parent: None,
        root: id.to_owned(),
        parent_transport: None,
        sender_route: context.sender_route.clone(),
        recipient_route: context.recipient_route.clone(),
        route_reply: context.route_reply,
        kind: context.kind,
        sender: context.sender,
        unresolved_external_parent: unresolved,
    };
    if let Some(parent) = parent {
        if !crate::inbox::valid_message_id(&parent) {
            return Err(refuse("parent is unavailable or not readable"));
        }
        let destination: Option<String> = tx
            .query_row(
                "SELECT mailbox FROM messages WHERE id=? AND state!='cancelled'",
                [&parent],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sqlite)?;
        let Some(destination) = destination else {
            return Err(refuse("parent is unavailable or not readable"));
        };
        if context
            .allowed_parent_mailboxes
            .as_ref()
            .is_some_and(|boxes| {
                !boxes
                    .iter()
                    .any(|key| normalise_mailbox(key) == normalise_mailbox(&destination))
            })
        {
            return Err(refuse("parent is unavailable or not readable"));
        }
        let metadata:Option<(String,String)>=tx.query_row("SELECT root_id,conversation_kind FROM message_threads WHERE message_id=? AND thread_version=1",[&parent],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(map_sqlite)?;
        if let Some((root, kind)) = metadata {
            if !crate::inbox::valid_message_id(&root) {
                return Err(refuse("stored thread root is invalid"));
            }
            if context.route_reply {
                if context.sender != ThreadSender::Human
                    || context.sender_route != ThreadRoute::Human
                {
                    return Err(refuse("automatic reply routing requires human ingress"));
                }
                let routes:Option<(String,Option<String>,String,Option<String>)>=tx.query_row("SELECT sender_kind,sender_mailbox,recipient_kind,recipient_mailbox FROM message_thread_routes WHERE message_id=?",[&parent],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(map_sqlite)?;
                let Some((sender, sender_box, recipient, recipient_box)) = routes else {
                    return Err(refuse(
                        "this message has no verified reply address; start a new conversation",
                    ));
                };
                let sender = ThreadRoute::from_parts(&sender, sender_box)?;
                let recipient = ThreadRoute::from_parts(&recipient, recipient_box)?;
                result.recipient_route = if sender == context.sender_route {
                    recipient
                } else {
                    sender
                };
                if result.recipient_route == ThreadRoute::None {
                    return Err(refuse(
                        "this sender cannot receive replies; start a new conversation",
                    ));
                }
            }
            let inherited = ThreadKind::parse(&kind)?;
            result.kind = if context.sender == ThreadSender::Human {
                ThreadKind::Human
            } else if inherited == ThreadKind::Other && context.kind == ThreadKind::Vp {
                ThreadKind::Vp
            } else {
                inherited
            };
            result.parent_transport = tx
                .query_row(
                    "SELECT transport FROM messages WHERE id=?",
                    [&parent],
                    |r| r.get(0),
                )
                .map_err(map_sqlite)?;
            result.root = root;
            result.parent = Some(parent);
            if result.kind != inherited {
                tx.execute("UPDATE message_threads SET conversation_kind=? WHERE root_id=? AND thread_version=1",rusqlite::params![result.kind.as_str(),result.root]).map_err(map_sqlite)?;
                tx.execute("INSERT INTO counters(name,value) VALUES('thread_view_revision',1) ON CONFLICT(name) DO UPDATE SET value=CAST(value AS INTEGER)+1",[]).map_err(map_sqlite)?;
            }
        } else if context
            .transport_ref
            .as_ref()
            .is_some_and(|r| r.parent_external_id.is_some())
        {
            result.unresolved_external_parent = true;
        } else {
            return Err(refuse(
                "this message is from a prior version; start a new conversation",
            ));
        }
    }
    Ok(result)
}
pub(super) fn record(
    tx: &Transaction<'_>,
    id: &str,
    prepared: &Prepared,
    context: &ThreadWrite,
) -> Result<(), DunkError> {
    tx.execute("INSERT INTO message_threads(message_id,thread_version,parent_id,root_id,conversation_kind,sender_kind) VALUES(?,1,?,?,?,?)",
        rusqlite::params![id,prepared.parent,prepared.root,prepared.kind.as_str(),prepared.sender.as_str()]).map_err(map_sqlite)?;
    tx.execute("INSERT INTO message_thread_routes(message_id,sender_kind,sender_mailbox,recipient_kind,recipient_mailbox) VALUES(?,?,?,?,?)",
        rusqlite::params![id,prepared.sender_route.kind(),prepared.sender_route.mailbox(),prepared.recipient_route.kind(),prepared.recipient_route.mailbox()]).map_err(map_sqlite)?;
    if let Some(reference) = &context.transport_ref {
        tx.execute("INSERT INTO message_thread_refs(transport,channel,external_id,message_id,parent_external_id,unresolved_parent) VALUES(?,?,?,?,?,?)",
            rusqlite::params![reference.transport,reference.channel,reference.external_id,id,reference.parent_external_id,prepared.unresolved_external_parent]).map_err(map_sqlite)?;
    }
    Ok(())
}

impl Prepared {
    pub(super) fn delivery_mailbox(&self, default: &str) -> String {
        if !self.route_reply {
            return default.to_owned();
        }
        match &self.recipient_route {
            ThreadRoute::Lane(mailbox) => mailbox.clone(),
            ThreadRoute::Human | ThreadRoute::Vp => "owner".into(),
            ThreadRoute::None => default.to_owned(),
        }
    }
    // Incoming human replies must not look like outbound replies to an old Telegram bridge.
    pub(super) fn legacy_parent(&self) -> Option<String> {
        if self.sender == ThreadSender::Human {
            None
        } else {
            self.parent.clone()
        }
    }
}

impl super::Store {
    pub fn thread_info(&self, id: &str) -> Result<Option<serde_json::Value>, DunkError> {
        self.readers.with(|db|db.query_row(
            "SELECT thread_version,parent_id,root_id,conversation_kind,sender_kind,COALESCE((SELECT MAX(unresolved_parent) FROM message_thread_refs r WHERE r.message_id=t.message_id),0),(SELECT sender_kind FROM message_thread_routes WHERE message_id=t.message_id),(SELECT sender_mailbox FROM message_thread_routes WHERE message_id=t.message_id),(SELECT recipient_kind FROM message_thread_routes WHERE message_id=t.message_id),(SELECT recipient_mailbox FROM message_thread_routes WHERE message_id=t.message_id) FROM message_threads t WHERE message_id=?",[id],|r|Ok(serde_json::json!({
                "thread_version":r.get::<_,i64>(0)?,"parent_id":r.get::<_,Option<String>>(1)?,"root_id":r.get::<_,String>(2)?,"conversation_kind":r.get::<_,String>(3)?,"sender_kind":r.get::<_,String>(4)?,"unresolved_parent":r.get::<_,bool>(5)?,"sender_route_kind":r.get::<_,Option<String>>(6)?,"sender_route_mailbox":r.get::<_,Option<String>>(7)?,"recipient_route_kind":r.get::<_,Option<String>>(8)?,"recipient_route_mailbox":r.get::<_,Option<String>>(9)?
            }))).optional().map_err(map_sqlite))
    }
    pub fn thread_view_revision(&self) -> Result<u64, DunkError> {
        self.readers.with(|db| {
            let n: i64 = db
                .query_row(
                    "SELECT value FROM counters WHERE name='thread_view_revision'",
                    [],
                    |r| r.get(0),
                )
                .optional()
                .map_err(map_sqlite)?
                .unwrap_or(0);
            u64::try_from(n).map_err(|_| refuse("invalid thread view revision"))
        })
    }
}

/// Called inside the delivery-receipt writer transaction.
pub(super) fn record_transport(
    tx: &Transaction<'_>,
    id: &str,
    reference: &ThreadTransportRef,
) -> Result<(), DunkError> {
    for value in [
        &reference.transport,
        &reference.channel,
        &reference.external_id,
    ] {
        if value.trim().is_empty() || value.len() > 160 || value.chars().any(char::is_control) {
            return Err(refuse("invalid transport message reference"));
        }
    }
    let versioned: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM message_threads WHERE message_id=? AND thread_version=1)",
            [id],
            |r| r.get(0),
        )
        .map_err(map_sqlite)?;
    if versioned {
        let existing:Option<String>=tx.query_row("SELECT message_id FROM message_thread_refs WHERE transport=? AND channel=? AND external_id=?",rusqlite::params![reference.transport,reference.channel,reference.external_id],|r|r.get(0)).optional().map_err(map_sqlite)?;
        if existing.as_ref().is_some_and(|old| old != id) {
            return Err(refuse("transport reference belongs to another message"));
        }
        tx.execute("INSERT OR IGNORE INTO message_thread_refs(transport,channel,external_id,message_id,parent_external_id,unresolved_parent) VALUES(?,?,?,?,NULL,0)",rusqlite::params![reference.transport,reference.channel,reference.external_id,id]).map_err(map_sqlite)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "thread_write_tests.rs"]
mod tests;
