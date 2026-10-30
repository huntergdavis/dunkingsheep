#!/usr/bin/env python3
"""Copy the owner/agent message store into inbox.db as mailbox `owner` (FLAT_FLEET.md 7.2).

`vp-of-agentry/data/messages.sqlite3` is a second inbox with its own schema
(`role=owner|agent`, `answered_at`, attachments on disk). It becomes one
mailbox in `inbox.db`, so owner-reply latency is the same metric as lane-read
latency and the watchdog needs no special case.

Two rules, both deliberate:

* **Copy, never move.** The source database is opened read-only and left exactly
  as it was. If the migration is wrong the old store is still there, and a
  rollback is `git checkout`-free.
* **Idempotent.** Every migrated row keeps its provenance in the message id
  (`m<source-id>` is not used: ids are the daemon's own counter, so instead the
  original id is recorded in the body-free `reply_to` chain and in a
  `migrated_from` table). Re-running reports what it skipped and changes nothing.

Usage:
    python3 tools/migrate_owner_mailbox.py --dry-run
    python3 tools/migrate_owner_mailbox.py --source PATH --inbox PATH --blobs DIR
"""
from __future__ import annotations

import argparse
import json
import os
import sqlite3
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import dunk_inbox  # noqa: E402

DEFAULT_SOURCE = Path("/home/hunter/workspace/vp-of-agentry/data/messages.sqlite3")
OWNER_MAILBOX = "owner"
# `channel` in the source is a routing name (cos, poolrad, operations). The
# transport is what the merge introduces: a row that came from the web desk is
# `web`, a Telegram row is `telegram`, everything else was the agent's own report
# channel. `direct_transport` is the source's own record when it has one.
TRANSPORT_BY_DIRECT = {"web": "web", "telegram": "telegram"}


def _iso_to_epoch(value):
    """'2026-10-29T22:45:26.455Z' -> unix seconds; None stays None."""
    if not value:
        return None
    import calendar
    import time as _time
    for fmt in ("%Y-%m-%dT%H:%M:%S.%fZ", "%Y-%m-%dT%H:%M:%SZ"):
        try:
            return float(calendar.timegm(_time.strptime(str(value), fmt)))
        except (TypeError, ValueError):
            continue
    return None


def transport_of(row):
    direct = (row.get("direct_transport") or "").strip().lower()
    if direct in TRANSPORT_BY_DIRECT:
        return TRANSPORT_BY_DIRECT[direct]
    # An owner question with no recorded transport was typed at the desk.
    return "web" if row.get("role") == "owner" else "report"


def read_source(path):
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    try:
        rows = [dict(row) for row in connection.execute(
            "SELECT * FROM messages ORDER BY id")]
        attachments = [dict(row) for row in connection.execute(
            "SELECT * FROM attachments ORDER BY created_at")]
    finally:
        connection.close()
    return rows, attachments


def _already_migrated(inbox_path):
    """Source ids a previous run copied, read without creating the database."""
    if not os.path.exists(inbox_path):
        return set()
    connection = sqlite3.connect(f"file:{inbox_path}?mode=ro", uri=True)
    try:
        return {row[0] for row in connection.execute(
            "SELECT source_id FROM migrated_from WHERE mailbox = ?", (OWNER_MAILBOX,))}
    except sqlite3.Error:
        return set()          # no migrated_from table yet: nothing was migrated
    finally:
        connection.close()


def migrate(source, inbox_path, blob_dir, dry_run=False):
    rows, attachments = read_source(source)
    inbox = (None if dry_run
             else dunk_inbox.Inbox(path=str(inbox_path), blob_dir=str(blob_dir)))
    report = {"source": str(source), "inbox": str(inbox_path), "rows": len(rows),
              "owner": sum(1 for r in rows if r["role"] == "owner"),
              "agent": sum(1 for r in rows if r["role"] != "owner"),
              "copied": [], "skipped": [], "attachments": 0, "errors": []}
    if dry_run:
        done = _already_migrated(inbox_path)
    else:
        with inbox._lock:
            inbox.db.execute(
                "CREATE TABLE IF NOT EXISTS migrated_from ("
                " mailbox TEXT NOT NULL, source_id TEXT NOT NULL,"
                " message_id TEXT NOT NULL, migrated_at REAL NOT NULL,"
                " PRIMARY KEY (mailbox, source_id))")
            inbox.db.commit()
        done = _already_migrated(inbox_path)
    # Source ids that map to the new ids, so reply/answer links survive.
    id_map = {}
    for row in rows:
        source_id = str(row["id"])
        if source_id in done:
            report["skipped"].append(source_id)
            id_map[source_id] = None
            continue
        body = row.get("body") or ""
        if not body.strip():
            report["skipped"].append(source_id)
            continue
        answer_of = row.get("answer_id") or row.get("reply_to")
        reply_to = id_map.get(str(answer_of)) if answer_of else None
        if dry_run:
            report["copied"].append({"source_id": source_id, "role": row["role"],
                                     "transport": transport_of(row)})
            id_map[source_id] = f"(dry:{source_id})"
            continue
        try:
            message = inbox.send(
                OWNER_MAILBOX, body,
                subject=(f"[{row['channel']}]" if row.get("channel") else None),
                sender=("owner" if row["role"] == "owner" else "agent"),
                mailbox_label=OWNER_MAILBOX, transport=transport_of(row),
                reply_to=reply_to)
        except dunk_inbox.InboxError as error:
            report["errors"].append(f"{source_id}: {error}")
            continue
        id_map[source_id] = message["message_id"]
        with inbox._lock:
            inbox.db.execute(
                "INSERT OR REPLACE INTO migrated_from (mailbox, source_id, message_id,"
                " migrated_at) VALUES (?,?,?,?)",
                (OWNER_MAILBOX, source_id, message["message_id"], __import__("time").time()))
            # created_at comes from the source so the latency baseline survives;
            # a copy stamped with "now" would erase the history it exists for.
            created = _iso_to_epoch(row.get("created_at"))
            if created:
                inbox.db.execute("UPDATE messages SET created_at = ? WHERE id = ?",
                                 (created, message["message_id"]))
            inbox.db.commit()
        report["copied"].append({"source_id": source_id, "message_id": message["message_id"],
                                 "role": row["role"], "transport": transport_of(row)})
        if row.get("answered_at"):
            inbox.ack([message["message_id"]], mailbox=OWNER_MAILBOX)
    if not dry_run:
        # Attachments are copied for every mapped message, including ones a
        # previous run migrated before it stopped: a run that dies mid-way must
        # finish its own work on the next pass, not skip it forever.
        for row in inbox.db.execute(
                "SELECT source_id, message_id FROM migrated_from WHERE mailbox = ?",
                (OWNER_MAILBOX,)):
            id_map[str(row[0])] = row[1]
        per_message = {}
        for item in attachments:
            path = item.get("path")
            new_id = id_map.get(str(item.get("message_id")))
            if not path or not new_id or not os.path.exists(path):
                continue
            per_message[new_id] = per_message.get(new_id, 0) + 1
            attachment_id = f"{new_id}-migrated-{per_message[new_id]}"
            try:
                with inbox._lock:
                    stored = inbox._store_blob(path)
                    cursor = inbox.db.execute(
                        "INSERT OR IGNORE INTO attachments (id, message_id, filename,"
                        " media_type, size, sha256, created_at)"
                        " VALUES (?,?,?,?,?,?,?)",
                        (attachment_id, new_id, item.get("filename"),
                         item.get("mime"), stored["size"], stored["sha256"],
                         __import__("time").time()))
                    inbox.db.commit()
                report["attachments"] += cursor.rowcount
            except (OSError, dunk_inbox.InboxError) as error:
                report["errors"].append(f"attachment {path}: {error}")
    report["migrated_total"] = len([c for c in report["copied"] if c.get("message_id")])
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", default=str(DEFAULT_SOURCE))
    parser.add_argument("--inbox", default=dunk_inbox.INBOX_DB)
    parser.add_argument("--blobs", default=dunk_inbox.BLOB_DIR)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    report = migrate(args.source, args.inbox, args.blobs, dry_run=args.dry_run)
    # A summary, never a truncated JSON document: half a document is not a report.
    summary = {key: value for key, value in report.items()
               if key not in ("copied", "skipped")}
    summary["copied"] = len(report["copied"])
    summary["skipped"] = len(report["skipped"])
    summary["transports"] = {name: sum(1 for row in report["copied"]
                                       if row.get("transport") == name)
                             for name in dunk_inbox.TRANSPORTS}
    print(json.dumps(summary, indent=1))
    return 0 if not report["errors"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
