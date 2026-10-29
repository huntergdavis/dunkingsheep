#!/usr/bin/env python3
"""
Per-agent inboxes: machine-to-machine messaging that never types.

Everything else in Dunking Sheep delivers by typing into a terminal's input
box, which is the human's channel. That transport is unreliable by nature:
Enter does not always submit, long pastes strand, and a message in flight can
collide with someone mid-sentence. Months of this module's history is in
docs/DESIGN.md.

An inbox sidesteps all of it. A message is a row in a SQLite database owned by
the daemon, addressed to a mailbox, and the recipient agent reads it when it
chooses through the same CLI / socket / MCP surfaces as everything else.
Nothing is typed except, optionally, a short fixed nudge telling the recipient
it has mail - and a handful of characters is the one case that never fails.

Properties the typing path cannot offer:

- durable: messages survive a daemon restart (the typing outbox is in memory
  and has lost messages to restarts twice).
- ordered per mailbox, with read state, so nothing is delivered twice.
- unbounded size: no paste limits, no chunking.
- attachments: arbitrary files, stored by content hash.
- a stable message id, queryable for its state at any time, by anyone.

Mailboxes are keyed by whatever uniquely identifies one pane: its own label
first, since that is a name someone chose, then its tab label, then its pane
id. Never the agent type - keying on that pooled every Codex pane into one
mailbox. Mail can be left for an agent that is not running.
"""

import hashlib
import json
import os
import re
import shutil
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import threading
import time
from contextlib import nullcontext

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
INBOX_DB = os.path.join(CONFIG_DIR, "inbox.db")
BLOB_DIR = os.path.join(CONFIG_DIR, "attachments")

# Read or acknowledged messages are kept this long so a recipient can look
# back, then purged. Unread mail is never purged by age.
RETENTION_S = 7 * 24 * 3600
# Per-attachment and total blob ceilings, so a runaway agent cannot fill the
# disk. Both are generous for text, diffs, logs and screenshots.
MAX_ATTACHMENT_BYTES = 64 * 1024 * 1024
MAX_BLOB_BYTES = 2 * 1024 * 1024 * 1024
MAX_BODY_BYTES = 8 * 1024 * 1024

# Message states. `queued` means sitting unread in the mailbox; the typing
# pipeline's "delivering/sent" has no analogue here because there is no
# delivery step to fail - the write either happened or it did not.
STATES = ("queued", "read", "acked", "cancelled", "archived",
          "undeliverable")
# Where an owner question arrived, so the answer goes back the same way
# (FLAT_FLEET.md 7.2, owner 2026-10-29: "reply on the transport the question
# arrived on"). `report` is the reporter's channel: the web shows it, Telegram
# does not. A lane message has no transport at all.
TRANSPORTS = ("web", "telegram", "report")


def validate_transport(transport):
    """None (a lane message) or one of TRANSPORTS."""
    if transport in (None, ""):
        return None
    transport = str(transport).strip().lower()
    if transport not in TRANSPORTS:
        raise InboxError(f"transport must be one of {', '.join(TRANSPORTS)}; "
                         f"got {transport!r}")
    return transport
# A mailbox key that is itself a pane id (older versions stored
# dead-pane mail this way, with pane_id NULL).
# The identity of last resort: a mailbox named after the pane. Both backends
# spell pane ids differently (w8:p8, %4) and both are legitimate here.
PANE_MAILBOX_RE = re.compile(r"^(?:w\d+:p\d+|%\d+)$")

SCHEMA = """
CREATE TABLE IF NOT EXISTS messages (
    id            TEXT PRIMARY KEY,
    mailbox       TEXT NOT NULL,
    mailbox_label TEXT,
    pane_id       TEXT,
    sender        TEXT,
    subject       TEXT,
    body          TEXT NOT NULL,
    state         TEXT NOT NULL,
    created_at    REAL NOT NULL,
    read_at       REAL,
    acked_at      REAL,
    archived_at   REAL,
    cancelled_at  REAL,
    reason        TEXT,
    notify_state  TEXT,
    notify_reason TEXT,
    digest_id     TEXT,
    digest_updated_at REAL,
    -- FLAT_FLEET.md 7.2: one owner mailbox, two transports. A question records
    -- where it arrived (web|telegram|report) so the reply goes back the same way;
    -- a reply carries reply_to = the question's id, which is also its ack link.
    transport     TEXT,
    reply_to      TEXT
);
CREATE INDEX IF NOT EXISTS messages_by_mailbox
    ON messages (mailbox, state, created_at);
CREATE INDEX IF NOT EXISTS messages_by_state
    ON messages (state, pane_id);
CREATE TABLE IF NOT EXISTS attachments (
    id          TEXT PRIMARY KEY,
    message_id  TEXT NOT NULL,
    filename    TEXT,
    media_type  TEXT,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_by_message ON attachments (message_id);
CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS deliveries (
    mailbox       TEXT PRIMARY KEY,
    message_ids   TEXT NOT NULL,
    delivered_at  REAL NOT NULL
);
"""


_PANE_SUFFIX = re.compile(r"\s*\(\s*(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\d+)\s*\)\s*$")


def normalise_mailbox(name):
    """'vp of agentry (w9:pH)' and 'vp of agentry' are ONE mailbox.

    The table carries both spellings: a channel that resolves with its pane id
    appends it, and the same mailbox addressed by name does not. Comparing the
    raw strings made the ack guard refuse the VP's own mail - a guard that
    refuses valid writes teaches people to work around it, which is worse than
    the bug it prevents. Only a trailing pane-id parenthetical is stripped:
    a label that merely contains brackets stays intact.
    """
    if name is None:
        return None
    text = str(name).strip()
    stripped = _PANE_SUFFIX.sub("", text).strip()
    return stripped or text


def mailbox_where(mailbox, column="mailbox"):
    """SQL that matches either spelling the table may hold for one mailbox."""
    plain = normalise_mailbox(mailbox)
    escaped = plain.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
    return (f"({column} = ? OR {column} LIKE ? ESCAPE '\\')",
            [plain, escaped + " (%"])


class InboxError(ValueError):
    """A caller mistake (unknown id, missing file, too big). Safe to show."""


def _now():
    return time.time()


class Inbox:
    """The message store. Knows nothing about panes, agents or herdr; callers
    resolve a target to a mailbox key and hand it over."""

    def __init__(self, path=INBOX_DB, blob_dir=BLOB_DIR, retention_s=RETENTION_S):
        self.path = path
        self.blob_dir = blob_dir
        self.retention_s = retention_s
        self._lock = threading.RLock()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        os.makedirs(self.blob_dir, exist_ok=True)
        self.db = sqlite_connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            columns = {row[1] for row in self.db.execute("PRAGMA table_info(messages)")}
            if "archived_at" not in columns:
                self.db.execute("ALTER TABLE messages ADD COLUMN archived_at REAL")
            if "digest_id" not in columns:
                self.db.execute("ALTER TABLE messages ADD COLUMN digest_id TEXT")
            if "digest_updated_at" not in columns:
                self.db.execute("ALTER TABLE messages ADD COLUMN digest_updated_at REAL")
            for name in ("transport", "reply_to"):
                if name not in columns:
                    self.db.execute(f"ALTER TABLE messages ADD COLUMN {name} TEXT")
            self.db.execute(
                "CREATE INDEX IF NOT EXISTS messages_by_transport "
                "ON messages (mailbox, transport, created_at)")
            for name, kind in (("notify_at", "REAL"), ("notify_transport_id", "TEXT"),
                               ("notify_attempt", "INTEGER")):
                if name not in columns:
                    self.db.execute(f"ALTER TABLE messages ADD COLUMN {name} {kind}")
            # Created after the migration: on an existing DB the column is new,
            # and a CREATE INDEX inside SCHEMA ran before ALTER TABLE.
            try:
                self.db.execute(
                    "CREATE UNIQUE INDEX IF NOT EXISTS messages_by_digest "
                    "ON messages (digest_id) WHERE digest_id IS NOT NULL")
            except sqlite3.OperationalError as exc:
                if "digest_id" not in str(exc):
                    raise
            # Acked mail is read mail. Older rows could be acked without a
            # read_at, which made boards that count NULLs report drained mail
            # as unread; backfill them once.
            self.db.execute(
                "UPDATE messages SET read_at = COALESCE(read_at, acked_at, created_at) "
                "WHERE state = 'acked' AND read_at IS NULL")
            self.db.commit()

    def close(self):
        with self._lock:
            self.db.close()

    # -- ids ----------------------------------------------------------------

    def _next_id(self):
        """Message ids are stable, never reused, and survive restarts: the
        counter lives in the database beside the messages."""
        row = self.db.execute(
            "SELECT value FROM counters WHERE name = 'message'").fetchone()
        number = (row["value"] if row else 0) + 1
        self.db.execute(
            "INSERT INTO counters (name, value) VALUES ('message', ?) "
            "ON CONFLICT(name) DO UPDATE SET value = ?", (number, number))
        return f"i{number}"

    # -- writing ------------------------------------------------------------

    def ensure_next_job(self, message_id, mailbox, pane_id, body, subject):
        """Materialise a task-store outbox intent once, including after a crash.

        The id was committed with the lease. Never replace a receipt or change
        the recipient of an existing message when replaying that intent.
        """
        if not str(message_id).startswith("nj-"):
            raise InboxError("next-job message id must start with nj-")
        with self._lock:
            self.db.execute(
                "INSERT OR IGNORE INTO messages "
                "(id,mailbox,mailbox_label,pane_id,sender,subject,body,state,created_at) "
                "VALUES (?,?,?,?,?,?,?,'queued',?)",
                (message_id, mailbox, mailbox, pane_id, "next-job", subject, body, _now()))
            self.db.commit()
            result = self.status(message_id)
            if result["pane_id"] != pane_id or result["mailbox"] != mailbox:
                raise InboxError("next-job recipient changed")
            return result

    def send(self, mailbox, body, subject=None, sender=None, mailbox_label=None,
             pane_id=None, attachments=(), transport=None, reply_to=None):
        """Put a message in a mailbox. Returns its row.

        `transport` records where an owner question arrived; `reply_to` links a
        reply to the question it answers (the same row is also acked, which is
        what makes "answered" one fact instead of two).
        """
        mailbox = str(mailbox or "").strip()
        if not mailbox:
            raise InboxError("a mailbox is required")
        transport = validate_transport(transport)
        body = "" if body is None else str(body)
        if len(body.encode("utf-8")) > MAX_BODY_BYTES:
            raise InboxError(f"body is larger than {MAX_BODY_BYTES} bytes")
        if not body.strip() and not attachments:
            raise InboxError("a message needs a body or an attachment")
        stored = [self._store_blob(path) for path in attachments or ()]
        with self._lock:
            message_id = self._next_id()
            self.db.execute(
                "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, "
                "subject, body, state, created_at, transport, reply_to) "
                "VALUES (?,?,?,?,?,?,?,'queued',?,?,?)",
                (message_id, mailbox, mailbox_label or mailbox, pane_id, sender,
                 subject, body, _now(), transport, reply_to))
            for index, blob in enumerate(stored, 1):
                # The attachment ROW id is per message; the blob itself stays
                # content-addressed and shared. Re-attaching the same file to
                # another message used to collide on attachments.id.
                attachment_id = f"{message_id}-{index}"
                self.db.execute(
                    "INSERT INTO attachments (id, message_id, filename, media_type, "
                    "size, sha256, created_at) VALUES (?,?,?,?,?,?,?)",
                    (attachment_id, message_id, blob["filename"], blob["media_type"],
                     blob["size"], blob["sha256"], _now()))
            self.db.commit()
        return self.get(message_id)

    def upsert_digest(self, mailbox, digest_id, body, subject=None, sender=None,
                      mailbox_label=None, pane_id=None):
        """One message per digest id, updated in place.

        A PM publishes a digest every window; it must not become a stream. The
        stable id means the VP's inbox holds one row per project: unread again
        after an update, with the previous body replaced.
        """
        body = "" if body is None else str(body)
        if not body.strip():
            raise InboxError("a digest needs a body")
        if len(body.encode("utf-8")) > MAX_BODY_BYTES:
            raise InboxError(f"body is larger than {MAX_BODY_BYTES} bytes")
        with self._lock:
            existing = self.db.execute(
                "SELECT * FROM messages WHERE digest_id = ?", (digest_id,)).fetchone()
            if existing is not None:
                self.db.execute(
                    "UPDATE messages SET mailbox=?, mailbox_label=?, pane_id=?, "
                    "sender=?, subject=?, body=?, state='queued', read_at=NULL, "
                    "acked_at=NULL, archived_at=NULL, cancelled_at=NULL, reason=NULL, "
                    "notify_state=NULL, notify_reason=NULL, digest_updated_at=? "
                    "WHERE digest_id=?",
                    (mailbox, mailbox_label or mailbox, pane_id, sender, subject, body,
                     _now(), digest_id))
                self.db.commit()
                return self.get(existing["id"])
            message_id = self._next_id()
            self.db.execute(
                "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, "
                "subject, body, state, created_at, digest_id, digest_updated_at) "
                "VALUES (?,?,?,?,?,?,?,'queued',?,?,?)",
                (message_id, mailbox, mailbox_label or mailbox, pane_id, sender,
                 subject, body, _now(), digest_id, _now()))
            self.db.commit()
        return self.get(message_id)

    def rename_mailbox(self, old, new):
        """Move every message from one mailbox key to another. Returns how
        many moved. Used to retire a mailbox that turned out to be shared."""
        with self._lock:
            cursor = self.db.execute(
                "UPDATE messages SET mailbox = ?, mailbox_label = ? WHERE mailbox = ?",
                (new, new, old))
            self.db.commit()
            return cursor.rowcount

    def record_notify(self, message_id, state, reason=None, *, transport_id=None, attempt=None):
        with self._lock:
            self.db.execute(
                "UPDATE messages SET notify_state = ?, notify_reason = ?, notify_at = ?, "
                "notify_transport_id = COALESCE(?, notify_transport_id), "
                "notify_attempt = COALESCE(?, notify_attempt) WHERE id = ?",
                (state, reason, _now(), transport_id, attempt, message_id))
            self.db.commit()

    # -- reading ------------------------------------------------------------

    def count(self, mailbox, lane_panes=None):
        where, params = mailbox_where(mailbox)
        with self._lock:
            row = self.db.execute(
                "SELECT COUNT(*) AS total, "
                "SUM(state = 'queued') AS unread, "
                "SUM(state = 'archived') AS archived, "
                "SUM(state = 'undeliverable') AS undeliverable, "
                "MIN(CASE WHEN state = 'queued' THEN created_at END) AS oldest "
                f"FROM messages WHERE {where} AND state != 'cancelled'",
                params).fetchone()
            blobs = self.db.execute(
                "SELECT COUNT(*) AS n FROM attachments a JOIN messages m "
                f"ON m.id = a.message_id WHERE m.mailbox IN "
                f"(SELECT mailbox FROM messages WHERE {where}) AND m.state = 'queued'",
                params).fetchone()
            from_lanes = 0
            if lane_panes:
                marks = ",".join("?" for _ in lane_panes)
                from_lanes = self.db.execute(
                    f"SELECT COUNT(*) FROM messages WHERE {where} AND state = 'queued' "
                    f"AND sender IN ({marks})", (*params, *lane_panes)).fetchone()[0]
        unread = int(row["unread"] or 0)
        return {
            "mailbox": mailbox,
            "unread": unread,
            "total": int(row["total"] or 0),
            "attachments_unread": int(blobs["n"] or 0),
            "archived": int(row["archived"] or 0),
            "undeliverable": int(row["undeliverable"] or 0),
            "oldest_unread_s": int(_now() - row["oldest"]) if row["oldest"] else None,
            "from_lanes": int(from_lanes),
        }

    def list(self, mailbox, unread_only=True, limit=50):
        """Summaries, oldest first. No bodies."""
        condition, params = mailbox_where(mailbox)
        where = f"{condition} AND state != 'cancelled'"
        if unread_only:
            where += " AND state = 'queued'"
        with self._lock:
            rows = self.db.execute(
                f"SELECT * FROM messages WHERE {where} ORDER BY created_at LIMIT ?",
                (*params, int(limit))).fetchall()
            return [self._summary(row) for row in rows]

    def replies_to(self, message_id):
        """Rows answering `message_id`, oldest first."""
        with self._lock:
            return [dict(row) for row in self.db.execute(
                "SELECT * FROM messages WHERE reply_to = ? ORDER BY created_at",
                (str(message_id),)).fetchall()]

    def conversation(self, mailbox, limit=50, before=None):
        """The mailbox as a conversation: questions and replies, oldest first.

        A row with `reply_to` is an answer; a row nothing points at is a question
        still waiting. `answered` is computed from the rows themselves, so a
        mailbox cannot disagree with itself about whether something was answered
        (FLAT_FLEET.md 7.2).
        """
        condition, params = mailbox_where(mailbox)
        where = f"{condition} AND state != 'cancelled'"
        if before is not None:
            where += " AND created_at < ?"
            params = (*params, float(before))
        with self._lock:
            rows = self.db.execute(
                f"SELECT * FROM messages WHERE {where} ORDER BY created_at DESC LIMIT ?",
                (*params, int(limit))).fetchall()
            answered = {row["reply_to"] for row in rows if row["reply_to"]}
            out = []
            for row in reversed(rows):
                item = dict(row)
                item["answered"] = item["id"] in answered or bool(item["acked_at"])
                out.append(item)
            return out

    def read(self, mailbox, limit=10, peek=False, mark=True):
        """Full messages in queue order. Marks them read unless `peek` or `mark=False`.

        `mark=False` exists so a caller can BUILD ITS OUTPUT FIRST and mark only once
        the output exists: marking before rendering meant a reader that crashed while
        displaying a batch had already consumed it, and pi-review-1 issued a verdict on
        a head whose withdrawal message it never saw (VP, t93). The mark is the caller's
        commit, so the caller decides when it is safe.
        """
        with self._lock:
            rows = self.db.execute(
                "SELECT * FROM messages WHERE mailbox = ? AND state = 'queued' "
                "ORDER BY created_at LIMIT ?", (mailbox, int(limit))).fetchall()
            messages = [self._full(row) for row in rows]
            if not peek and mark and rows:
                now = _now()
                self.db.executemany(
                    "UPDATE messages SET state = 'read', read_at = ? "
                    "WHERE id = ? AND state = 'queued'",
                    [(now, row["id"]) for row in rows])
                self.db.commit()
                for message in messages:
                    message["state"] = "read"
                    message["read_at"] = now
        return messages

    def mark_read(self, message_ids, now=None):
        """Commit the read for ids a caller has already rendered.

        Idempotent and narrow: only rows still 'queued' move, so a second call (or a
        concurrent reader) cannot re-stamp a message that was already consumed.
        """
        ids = [str(i) for i in (message_ids or [])]
        if not ids:
            return 0
        now = _now() if now is None else now
        with self._lock:
            before = self.db.total_changes
            self.db.executemany(
                "UPDATE messages SET state = 'read', read_at = ? "
                "WHERE id = ? AND state = 'queued'", [(now, i) for i in ids])
            self.db.commit()
            return self.db.total_changes - before

    def record_delivery(self, mailbox, message_ids, now=None):
        """Remember the latest delivered batch for a mailbox: the recovery buffer.

        Bounded to ONE batch per mailbox: a newer read overwrites the receipt,
        so the buffer never grows into a history. An empty batch records
        nothing and leaves any existing receipt alone - an empty read delivered
        nothing new, so there is nothing new to recover.
        """
        ids = [str(i) for i in (message_ids or []) if str(i).strip()]
        if not ids:
            return 0
        now = _now() if now is None else now
        with self._lock:
            self.db.execute(
                "INSERT INTO deliveries (mailbox, message_ids, delivered_at) "
                "VALUES (?,?,?) ON CONFLICT(mailbox) DO UPDATE SET "
                "message_ids = excluded.message_ids, "
                "delivered_at = excluded.delivered_at",
                (mailbox, json.dumps(ids), now))
            self.db.commit()
            return len(ids)

    def pending_delivery(self, mailbox):
        """The receipt for a mailbox, or None when nothing is recoverable."""
        with self._lock:
            row = self.db.execute(
                "SELECT mailbox, message_ids, delivered_at FROM deliveries "
                "WHERE mailbox = ?", (mailbox,)).fetchone()
        if row is None:
            return None
        return {"mailbox": row["mailbox"],
                "message_ids": list(json.loads(row["message_ids"])),
                "delivered_at": row["delivered_at"]}

    def recover_delivery(self, mailbox):
        """Replay the receipt batch for display, whatever its read state.

        Only ids that still exist come back, in queue order; ids that vanished
        (an explicit delete) shrink the receipt instead of haunting it. When
        nothing survives, the receipt is dropped and the error says so: a
        receipt that replays nothing forever is the unbounded loop this buffer
        exists to avoid.
        """
        receipt = self.pending_delivery(mailbox)
        if receipt is None:
            # No receipt is the normal case (nothing delivered, or everything
            # already acked): like an empty read, this answers empty rather
            # than erroring, so polling callers and the smoke suite get a
            # result, not a refusal.
            return []
        with self._lock:
            rows = self.db.execute(
                "SELECT * FROM messages WHERE id IN (%s) ORDER BY created_at" %
                ",".join("?" for _ in receipt["message_ids"]),
                tuple(receipt["message_ids"])).fetchall()
            survivors = [row["id"] for row in rows]
            if set(survivors) != set(receipt["message_ids"]):
                if survivors:
                    self.db.execute(
                        "UPDATE deliveries SET message_ids = ? WHERE mailbox = ?",
                        (json.dumps(survivors), mailbox))
                else:
                    self.db.execute(
                        "DELETE FROM deliveries WHERE mailbox = ?", (mailbox,))
                self.db.commit()
            messages = [self._full(row) for row in rows]
        return messages

    def note_acked(self, mailbox):
        """Shrink the receipt to ids that are still not acked.

        Called after an ack: a fully acked receipt is deleted, a partially
        acked one keeps only the remainder. Returns the ids still pending.
        """
        receipt = self.pending_delivery(mailbox)
        if receipt is None:
            return []
        with self._lock:
            rows = self.db.execute(
                "SELECT id, state FROM messages WHERE id IN (%s)" %
                ",".join("?" for _ in receipt["message_ids"]),
                tuple(receipt["message_ids"])).fetchall()
            remaining = [row["id"] for row in rows if row["state"] != "acked"]
            if remaining:
                self.db.execute(
                    "UPDATE deliveries SET message_ids = ? WHERE mailbox = ?",
                    (json.dumps(remaining), mailbox))
            else:
                self.db.execute(
                    "DELETE FROM deliveries WHERE mailbox = ?", (mailbox,))
            self.db.commit()
            return remaining

    def get(self, message_id, with_body=True):
        with self._lock:
            row = self.db.execute(
                "SELECT * FROM messages WHERE id = ?", (message_id,)).fetchone()
        if row is None:
            raise InboxError(f"no message with id {message_id!r}")
        return self._full(row) if with_body else self._summary(row)

    def status(self, message_id):
        """State of one message, by id, with no message text in it.

        This is what a supervisor polls: it says where a message is and why,
        and deliberately carries neither body nor subject so a status board can
        show it without leaking the contents."""
        with self._lock:
            row = self.db.execute(
                "SELECT * FROM messages WHERE id = ?", (message_id,)).fetchone()
            if row is None:
                raise InboxError(f"no message with id {message_id!r}")
            blobs = self.db.execute(
                "SELECT COUNT(*) AS n, COALESCE(SUM(size), 0) AS bytes "
                "FROM attachments WHERE message_id = ?", (message_id,)).fetchone()
        return {
            "message_id": row["id"],
            "state": row["state"],
            "mailbox": row["mailbox"],
            "destination": row["mailbox_label"] or row["mailbox"],
            "pane_id": row["pane_id"],
            "sender": row["sender"],
            "created_at": row["created_at"],
            "read_at": row["read_at"],
            "acked_at": row["acked_at"],
            "archived_at": row["archived_at"],
            "cancelled_at": row["cancelled_at"],
            "waiting_s": (int(_now() - row["created_at"])
                          if row["state"] == "queued" else None),
            "body_bytes": len((row["body"] or "").encode("utf-8")),
            "attachments": int(blobs["n"] or 0),
            "attachment_bytes": int(blobs["bytes"] or 0),
            "reason": row["reason"],
            "notify": {"state": row["notify_state"], "reason": row["notify_reason"],
                       "at": row["notify_at"], "transport_id": row["notify_transport_id"],
                       "attempt": row["notify_attempt"]},
        }

    def stats(self):
        with self._lock:
            rows = self.db.execute(
                "SELECT mailbox, mailbox_label, COUNT(*) AS total, "
                "SUM(state = 'queued') AS unread, "
                "SUM(state = 'archived') AS archived, "
                "SUM(state = 'undeliverable') AS undeliverable, "
                "MIN(CASE WHEN state = 'queued' THEN created_at END) AS oldest "
                "FROM messages WHERE state != 'cancelled' "
                "GROUP BY mailbox ORDER BY unread DESC, mailbox").fetchall()
        return [{
            "mailbox": row["mailbox"],
            "destination": row["mailbox_label"] or row["mailbox"],
            "unread": int(row["unread"] or 0),
            "total": int(row["total"] or 0),
            "archived": int(row["archived"] or 0),
            "undeliverable": int(row["undeliverable"] or 0),
            "oldest_unread_s": int(_now() - row["oldest"]) if row["oldest"] else None,
        } for row in rows]

    # -- retiring -----------------------------------------------------------

    def ack(self, message_ids, mailbox=None):
        # Ack implies read: stamp read_at so a board that counts NULLs as
        # unread cannot report drained mail as waiting. It is scoped to the
        # caller's mailbox: an id from another mailbox is refused, never
        # silently marked read in that mailbox by someone else's ack.
        return self._transition(message_ids, "acked", "acked_at",
                                allowed=("queued", "read"), read_stamp=True,
                                mailbox=mailbox)

    def cancel(self, message_ids):
        return self._transition(message_ids, "cancelled", "cancelled_at",
                                allowed=("queued",))

    def archive(self, mailbox, before=None, states=None):
        """Bulk-retire a mailbox without deleting anything: mark messages read
        and archived (counted separately from unread). Default states are
        queued and acked; pass read too to drain the whole mailbox."""
        mailbox = str(mailbox or "").strip()
        if not mailbox:
            raise InboxError("a mailbox is required")
        if states is None:
            states = ("queued", "acked")
        if isinstance(states, str):
            states = [part.strip() for part in states.split(",") if part.strip()]
        allowed = [state for state in states
                   if state in STATES and state not in
                   ("cancelled", "archived", "undeliverable")]
        if not allowed:
            raise InboxError("state must be one of queued, read, acked")
        now = _now()
        placeholders = ",".join("?" for _ in allowed)
        condition, box_params = mailbox_where(mailbox)
        sql = (f"UPDATE messages SET state = 'archived', archived_at = ?, "
               f"read_at = COALESCE(read_at, ?) WHERE {condition} "
               f"AND state IN ({placeholders})")
        params = [now, now, *box_params, *allowed]
        if before is not None:
            sql += " AND created_at < ?"
            params.append(float(before))
        with self._lock:
            cursor = self.db.execute(sql, params)
            self.db.commit()
            archived = cursor.rowcount
        return {"mailbox": mailbox, "archived": archived,
                "states": allowed, "before": before}

    def mark_undeliverable(self, live_panes):
        """A queued message whose resolved pane id no longer exists is
        undeliverable, not silently queued. Nothing is deleted."""
        live = {str(pane) for pane in (live_panes or ()) if pane}
        if not live:
            return {"undeliverable": 0, "panes": []}
        changed = 0
        with self._lock:
            rows = self.db.execute(
                "SELECT DISTINCT pane_id FROM messages "
                "WHERE state = 'queued' AND pane_id IS NOT NULL").fetchall()
            dead_panes = sorted({row["pane_id"] for row in rows
                                 if row["pane_id"] not in live})
            if dead_panes:
                placeholders = ",".join("?" for _ in dead_panes)
                cursor = self.db.execute(
                    f"UPDATE messages SET state = 'undeliverable', reason = ? "
                    f"WHERE state = 'queued' AND pane_id IN ({placeholders})",
                    ("target pane no longer exists", *dead_panes))
                changed += cursor.rowcount
            # Older mail to a dead pane id was stored under the pane id as the
            # mailbox key with pane_id NULL; retire those too.
            rows = self.db.execute(
                "SELECT DISTINCT mailbox FROM messages WHERE state = 'queued'").fetchall()
            dead_mailboxes = sorted({
                row["mailbox"] for row in rows
                if PANE_MAILBOX_RE.match(str(row["mailbox"] or ""))
                and row["mailbox"] not in live})
            if dead_mailboxes:
                placeholders = ",".join("?" for _ in dead_mailboxes)
                cursor = self.db.execute(
                    f"UPDATE messages SET state = 'undeliverable', reason = ? "
                    f"WHERE state = 'queued' AND mailbox IN ({placeholders})",
                    ("target pane no longer exists", *dead_mailboxes))
                changed += cursor.rowcount
            self.db.commit()
        return {"undeliverable": changed, "panes": dead_panes,
                "mailboxes": dead_mailboxes}

    def auto_archive_undeliverable(self, age_s=86400, now=None):
        """Archive undeliverable mail whose target has been gone for `age_s`.

        Undeliverable mail can never be delivered and pages the watchdog until
        someone archives it by hand. After 24 h the reason is recorded and the
        state moves to archived; the watchdog's digest counts it daily instead
        of alerting per mailbox.
        """
        now = time.time() if now is None else float(now)
        cutoff = now - float(age_s)
        with self._lock:
            cursor = self.db.execute(
                "UPDATE messages SET state = 'archived', archived_at = ?, "
                "reason = COALESCE(reason, 'undeliverable older than 24h') "
                "WHERE state = 'undeliverable' AND created_at < ? "
                "AND archived_at IS NULL",
                (now, cutoff))
            self.db.commit()
        return int(cursor.rowcount or 0)

    def archive_retired(self, mailboxes, age_s=86400, now=None, reason=None,
                        authorize=None, identity_guard=None):
        """Archive mail addressed to a RETIRED pane's mailbox, keeping every row.

        Retention is not 'keep alerting': a retired pane's mail is the record of what
        was tried, and 24 messages sat 42 hours old in a state that was neither
        delivered nor archived. The rows stay; the state moves on.
        """
        if not mailboxes:
            return 0
        now = time.time() if now is None else float(now)
        cutoff = now - float(age_s)
        with self._lock, (identity_guard() if identity_guard else nullcontext()):
            owned_transaction = False
            try:
                if authorize is not None:
                    if self.db.in_transaction:
                        return 0  # never commit or discard somebody else's transaction
                    self.db.execute("BEGIN IMMEDIATE")
                    owned_transaction = True
                    mailboxes = set(mailboxes) & set(authorize())
                    if not mailboxes:
                        self.db.rollback()
                        return 0
                marks = ",".join("?" for _ in mailboxes)
                cursor = self.db.execute(
                f"UPDATE messages SET state = 'archived', archived_at = ?, "
                f"read_at = COALESCE(read_at, ?), "
                f"reason = COALESCE(reason, ?) "
                f"WHERE mailbox IN ({marks}) AND state IN ('queued','undeliverable') "
                f"AND created_at < ? AND archived_at IS NULL",
                    (now, now, reason or "mailbox of a retired pane", *mailboxes, cutoff))
                self.db.commit()
            except Exception:
                if owned_transaction:
                    self.db.rollback()
                raise
        return int(cursor.rowcount or 0)

    def _transition(self, message_ids, state, stamp, allowed, read_stamp=False,
                    mailbox=None):
        ids = [message_ids] if isinstance(message_ids, str) else list(message_ids)
        ids = [str(i).strip() for i in ids if str(i).strip()]
        if not ids:
            raise InboxError("at least one message id is required")
        changed, skipped = [], []
        with self._lock:
            for message_id in ids:
                row = self.db.execute(
                    "SELECT state, mailbox FROM messages WHERE id = ?", (message_id,)).fetchone()
                if row is None:
                    raise InboxError(f"no message with id {message_id!r}")
                if mailbox and normalise_mailbox(row["mailbox"]) != normalise_mailbox(mailbox):
                    # A write not scoped to its owner is a blast radius: the VP
                    # acked a sent copy and the recipient lane lost its unread.
                    raise InboxError(
                        f"message {message_id} is not in mailbox {mailbox!r}; "
                        "an ack only applies to the caller's own mailbox")
                if row["state"] not in allowed:
                    skipped.append({"message_id": message_id, "state": row["state"]})
                    continue
                now = _now()
                if read_stamp:
                    self.db.execute(
                        f"UPDATE messages SET state = ?, {stamp} = ?, "
                        f"read_at = COALESCE(read_at, ?) WHERE id = ?",
                        (state, now, now, message_id))
                else:
                    self.db.execute(
                        f"UPDATE messages SET state = ?, {stamp} = ? WHERE id = ?",
                        (state, now, message_id))
                changed.append(message_id)
            self.db.commit()
        return {"state": state, "changed": changed, "unchanged": skipped}

    def delete(self, message_ids):
        ids = [message_ids] if isinstance(message_ids, str) else list(message_ids)
        ids = [str(i).strip() for i in ids if str(i).strip()]
        if not ids:
            raise InboxError("at least one message id is required")
        with self._lock:
            for message_id in ids:
                row = self.db.execute(
                    "SELECT id FROM messages WHERE id = ?", (message_id,)).fetchone()
                if row is None:
                    raise InboxError(f"no message with id {message_id!r}")
            self.db.executemany("DELETE FROM attachments WHERE message_id = ?",
                                [(i,) for i in ids])
            self.db.executemany("DELETE FROM messages WHERE id = ?", [(i,) for i in ids])
            self.db.commit()
        self._collect_blobs()
        return {"deleted": ids}

    def purge(self, older_than_s=None):
        """Drop read, acknowledged and cancelled mail past its retention.
        Unread mail is never purged by age."""
        cutoff = _now() - (self.retention_s if older_than_s is None else older_than_s)
        with self._lock:
            rows = self.db.execute(
                "SELECT id FROM messages WHERE state != 'queued' AND "
                "COALESCE(acked_at, cancelled_at, read_at, archived_at, created_at) < ?",
                (cutoff,)).fetchall()
            ids = [row["id"] for row in rows]
            if ids:
                self.db.executemany("DELETE FROM attachments WHERE message_id = ?",
                                    [(i,) for i in ids])
                self.db.executemany("DELETE FROM messages WHERE id = ?",
                                    [(i,) for i in ids])
                self.db.commit()
        if ids:
            self._collect_blobs()
        return {"purged": len(ids)}

    # -- attachments --------------------------------------------------------

    def _store_blob(self, path):
        path = os.path.expanduser(str(path))
        if not os.path.isfile(path):
            raise InboxError(f"no such file to attach: {path}")
        size = os.path.getsize(path)
        if size > MAX_ATTACHMENT_BYTES:
            raise InboxError(
                f"{os.path.basename(path)} is {size} bytes, over the "
                f"{MAX_ATTACHMENT_BYTES} byte attachment limit")
        digest = hashlib.sha256()
        with open(path, "rb") as handle:
            for chunk in iter(lambda: handle.read(1 << 20), b""):
                digest.update(chunk)
        sha = digest.hexdigest()
        blob_path = os.path.join(self.blob_dir, sha)
        if not os.path.exists(blob_path):
            if self._blob_bytes() + size > MAX_BLOB_BYTES:
                raise InboxError("attachment store is full; delete some mail first")
            shutil.copyfile(path, blob_path)
        return {"sha256": sha, "filename": os.path.basename(path), "size": size,
                "media_type": _media_type(path)}

    def fetch(self, attachment_id, dest_dir):
        """Copy an attachment out of the store. Returns the written path."""
        with self._lock:
            row = self.db.execute(
                "SELECT * FROM attachments WHERE id = ?", (attachment_id,)).fetchone()
        if row is None:
            raise InboxError(f"no attachment with id {attachment_id!r}")
        blob_path = os.path.join(self.blob_dir, row["sha256"])
        if not os.path.exists(blob_path):
            raise InboxError(f"attachment {attachment_id} is missing from the store")
        dest_dir = os.path.expanduser(dest_dir)
        os.makedirs(dest_dir, exist_ok=True)
        target = os.path.join(dest_dir, row["filename"] or row["sha256"])
        shutil.copyfile(blob_path, target)
        return {"attachment_id": attachment_id, "path": target,
                "filename": row["filename"], "size": row["size"],
                "media_type": row["media_type"], "message_id": row["message_id"]}

    def _blob_bytes(self):
        total = 0
        for name in os.listdir(self.blob_dir):
            try:
                total += os.path.getsize(os.path.join(self.blob_dir, name))
            except OSError:
                pass
        return total

    def _collect_blobs(self):
        """Drop blobs no message references any more."""
        with self._lock:
            keep = {row["sha256"] for row in
                    self.db.execute("SELECT DISTINCT sha256 FROM attachments")}
        for name in os.listdir(self.blob_dir):
            if name not in keep:
                try:
                    os.remove(os.path.join(self.blob_dir, name))
                except OSError:
                    pass

    # -- shaping ------------------------------------------------------------

    def _attachments(self, message_id):
        rows = self.db.execute(
            "SELECT id, filename, media_type, size FROM attachments "
            "WHERE message_id = ? ORDER BY rowid", (message_id,)).fetchall()
        return [dict(row) for row in rows]

    def _summary(self, row):
        blobs = self._attachments(row["id"])
        return {
            "message_id": row["id"],
            "mailbox": row["mailbox"],
            "destination": row["mailbox_label"] or row["mailbox"],
            "sender": row["sender"],
            "subject": row["subject"],
            "state": row["state"],
            "created_at": row["created_at"],
            "waiting_s": (int(_now() - row["created_at"])
                          if row["state"] == "queued" else None),
            "body_bytes": len((row["body"] or "").encode("utf-8")),
            "digest_id": row["digest_id"] if "digest_id" in row.keys() else None,
            # FLAT_FLEET.md 7.2: where an owner question arrived, and which
            # question a reply answers. Every message carries both, lane mail
            # with transport=None and reply_to=None.
            "transport": row["transport"] if "transport" in row.keys() else None,
            "reply_to": row["reply_to"] if "reply_to" in row.keys() else None,
            "attachments": blobs,
        }

    def _full(self, row):
        message = self._summary(row)
        message["body"] = row["body"]
        message["read_at"] = row["read_at"]
        return message


def _media_type(path):
    import mimetypes
    guess, _encoding = mimetypes.guess_type(path)
    return guess or "application/octet-stream"
