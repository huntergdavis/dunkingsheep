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
    (SELECT sender_kind FROM message_threads WHERE message_id = messages.id) AS sender_kind
";
