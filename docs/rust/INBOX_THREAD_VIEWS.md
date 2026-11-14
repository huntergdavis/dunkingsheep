# Inbox views and the new-write cutover

Tasks t5011 (reader/UI) and t5010 (writer/threads) ship in **one daemon and desk release**. Do not deploy the reader alone with empty new views. The UI offers only **Your inbox**, **VP**, and **Historical / prior versions**; there is no audience toggle.

The read-only 2026-11-14 metadata snapshot explains the former toggle failure: the current `codex-rust-owner` mailbox had 3,291 pane-sender rows without an audience tag versus 428 tagged `agent`. The root `owner` mailbox also mixed recorded web/Telegram human ingress and replies with null-transport agent rows. Absence of `audience=agent` is not evidence of a human message. The evidence is metadata only; message text is not a classifier.

## Additive contract

`message_threads` stores `message_id`, `thread_version=1`, nullable `parent_id`, `root_id`, `conversation_kind` (`human`, `vp`, `other`), and authenticated `sender_kind` (`human`, `agent`, `system`). The old `messages` table and its rows are unchanged. New root IDs reference the allocated message itself. The writer validates an explicit parent, copies its root and routing scope, and commits the message and metadata atomically. Clients cannot choose root IDs or trusted origin kinds. Human participation in a new VP thread promotes its versioned records to the human scope atomically; legacy records are never backfilled.

The writer slice must stamp web, Telegram and inbox-send origins from authenticated operations and canonical routing. A pane with the compatibility role `owner` can be the VP agent; role or display name alone does not establish human origin. Telegram external reply references must resolve to stored internal IDs; never infer lineage from text or timing. Legacy `reply_to` is not presented as newly verified lineage.

## Read contract

The existing paged `owner_conversation` API accepts `source_filter=human_threads|vp_threads|historical`. Old selectors remain compatible. New selectors require `include_internal=false` (or omission); this is compatibility metadata, not a UI control. They all retain the existing owner/service authorization and owner-mailbox/lane envelope.

Human and VP reads require a versioned sidecar row with the matching recorded `conversation_kind`. Historical reads require no sidecar row and include prior messages regardless of their optional audience tag. Filtering precedes LIMIT and cursor resolution. A cursor from another view is refused. Browsing never marks, acknowledges, or changes a message.

The web projection and UI require confirmed view scope and explicit lineage fields. Historical fields are present and null; new fields include a canonical root, optional canonical parent, version and origin kind. Missing or mismatched metadata fails closed instead of displaying an unscoped response. New-message display roles follow recorded `sender_kind`; historical content and prior reply references remain unchanged and honestly labelled.

## Schema and rollback audit

DDL adds one table, two indexes and a delete trigger, with no ALTER, UPDATE or backfill of legacy messages. The trigger removes sidecar rows when an old writer deletes a message. Missing retained ancestors remain explicit IDs; they are not silently reattached. Unit coverage opens the archived DB fixture, proves every prior schema definition unchanged, exercises old SELECT/DELETE SQL, and checks idempotent reopening.

Before release, capture the actual schema delta and run the pinned old daemon/desk binaries against a **private fixture** with the sidecar present. Record the old-binary compatibility receipt, independent artifact review, activation authorization and rollback pins. The SQL/schema unit check alone is not a claim that the old binary has been exercised. Preserve historical browsing and attachment access; t5014 will add the reviewed mailbox archive lifecycle without changing these lineage rules.
