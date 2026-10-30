"""The owner mailbox: one mailbox, two transports (FLAT_FLEET.md 7.2)."""

import json
import os
import shutil
import sqlite3
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
import dunk_inbox  # noqa: E402
import dunk_roles  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox, InboxError  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))
import migrate_owner_mailbox as migration  # noqa: E402

OWNER, DESK, WORKER = "%95", "%96", "%99"


class TransportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def test_a_question_records_its_transport_and_a_reply_links_to_it(self):
        question = self.inbox.send("owner", "are we green?", transport="telegram")
        self.assertEqual("telegram", question["transport"])
        self.assertIsNone(question["reply_to"])
        reply = self.inbox.send("owner", "yes", transport="telegram",
                                reply_to=question["message_id"])
        self.assertEqual(question["message_id"], reply["reply_to"])
        self.assertEqual([reply["message_id"]],
                         [row["id"] for row in self.inbox.replies_to(question["message_id"])])

    def test_lane_mail_has_no_transport(self):
        message = self.inbox.send("pi-worker-3", "a report")
        self.assertIsNone(message["transport"])
        self.assertIsNone(message["reply_to"])

    def test_an_unknown_transport_is_refused(self):
        with self.assertRaisesRegex(InboxError, "transport must be one of"):
            self.inbox.send("owner", "hello", transport="carrier-pigeon")

    def test_conversation_reports_which_questions_are_waiting(self):
        asked = self.inbox.send("owner", "first?", transport="web")
        self.inbox.send("owner", "second?", transport="web")
        self.inbox.send("owner", "answer to the first", transport="web",
                        reply_to=asked["message_id"])
        rows = self.inbox.conversation("owner")
        self.assertEqual(3, len(rows))
        by_body = {row["body"]: row for row in rows}
        self.assertTrue(by_body["first?"]["answered"])
        self.assertFalse(by_body["second?"]["answered"])


class OwnerMailboxTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.roles = Roles(path=os.path.join(self.tmp.name, "roles.json"),
                           tokens_path=os.path.join(self.tmp.name, "tokens.json"))
        self.flock = Flock(
            herdr=FakeHerdr(), state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False, restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=self.roles)
        self.roles.register(OWNER, "owner")
        self.roles.register(DESK, "desk", provider="pi", label="desk")
        self.roles.register(WORKER, "worker", provider="pi", label="pi-worker-3")

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None):
        self.flock.clear_caller()
        try:
            return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)
        finally:
            self.flock.clear_caller()

    def ask(self, transport="telegram", body="is the fleet green?"):
        return self.call("inbox_send", {"to": "owner", "body": body}, pane=OWNER)

    def test_a_reply_lands_on_the_same_transport_and_acks_the_question(self):
        question = self.ask(transport="telegram")
        self.flock.inbox.db.execute(
            "UPDATE messages SET transport = 'telegram' WHERE id = ?",
            (question["message_id"],))
        self.flock.inbox.db.commit()
        out = self.call("owner_reply",
                        {"message_id": question["message_id"], "body": "yes, all green"},
                        pane=DESK)
        self.assertEqual("telegram", out["transport"])
        self.assertEqual(question["message_id"], out["reply"]["reply_to"])
        self.assertEqual("telegram", out["reply"]["transport"])
        self.assertEqual("acked", self.flock.inbox.get(question["message_id"])["state"])

    def test_one_answer_per_question(self):
        question = self.ask()
        self.call("owner_reply", {"message_id": question["message_id"], "body": "first"},
                  pane=DESK)
        with self.assertRaisesRegex(DunkError, "already answered"):
            self.call("owner_reply", {"message_id": question["message_id"], "body": "second"},
                      pane=DESK)

    def test_a_reply_to_a_reply_is_refused(self):
        question = self.ask()
        out = self.call("owner_reply",
                        {"message_id": question["message_id"], "body": "answer"}, pane=DESK)
        with self.assertRaisesRegex(DunkError, "is itself a reply"):
            self.call("owner_reply",
                      {"message_id": out["reply"]["message_id"], "body": "answer to the answer"},
                      pane=DESK)

    def test_a_lane_message_is_not_an_owner_question(self):
        lane_mail = self.flock.inbox.send("pi-worker-3", "hi", sender=DESK)
        with self.assertRaisesRegex(DunkError, "not an owner message"):
            self.call("owner_reply",
                      {"message_id": lane_mail["message_id"], "body": "nope"}, pane=DESK)

    def test_a_worker_may_not_read_or_answer_the_owner_mailbox(self):
        question = self.ask()
        refusal = "not permitted for role worker|the desk's and the owner's"
        with self.assertRaisesRegex(DunkError, refusal):
            self.call("owner_conversation", {}, pane=WORKER)
        with self.assertRaisesRegex(DunkError, refusal):
            self.call("owner_reply",
                      {"message_id": question["message_id"], "body": "nope"}, pane=WORKER)

    def test_the_conversation_shows_the_question_and_its_answer(self):
        question = self.ask(body="what is the checkpoint?")
        self.call("owner_reply",
                  {"message_id": question["message_id"], "body": "289 strict"}, pane=DESK)
        out = self.call("owner_conversation", {}, pane=DESK)
        bodies = [row["body"] for row in out["messages"]]
        self.assertEqual(["what is the checkpoint?", "289 strict"], bodies)
        self.assertEqual([], out["waiting"])

    def test_the_allowlist_matches_the_brief(self):
        for command in ("owner_conversation", "owner_reply"):
            self.assertTrue(dunk_roles.allowed("desk", command), command)
            self.assertTrue(dunk_roles.allowed("web", command), command)
            self.assertTrue(dunk_roles.allowed("owner", command), command)
            self.assertFalse(dunk_roles.allowed("worker", command), command)
            self.assertFalse(dunk_roles.allowed("reviewer", command), command)
            self.assertFalse(dunk_roles.allowed("anonymous", command), command)
        self.assertTrue(dunk_roles.allowed("desk", "owner_conversation"))


class MigrationTests(unittest.TestCase):
    """The 2,4xx historical rows move BY COPY, with a test on a copy."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.source = os.path.join(self.tmp.name, "messages.sqlite3")
        self.inbox = os.path.join(self.tmp.name, "inbox.db")
        self.blobs = os.path.join(self.tmp.name, "blobs")
        os.makedirs(self.blobs)
        self.attachment = os.path.join(self.tmp.name, "shot.png")
        with open(self.attachment, "wb") as handle:
            handle.write(b"\x89PNG\r\n\x1a\n fake screenshot")
        db = sqlite3.connect(self.source)
        db.executescript("""
            CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
                channel TEXT NOT NULL, role TEXT NOT NULL, body TEXT NOT NULL,
                delivery TEXT NOT NULL DEFAULT 'posted',
                created_at TEXT NOT NULL, direct_transport TEXT,
                answer_id INTEGER, answered_at TEXT, delivered_at TEXT);
            CREATE TABLE attachments (id TEXT PRIMARY KEY, filename TEXT NOT NULL,
                mime TEXT NOT NULL, size INTEGER NOT NULL, path TEXT NOT NULL,
                message_id INTEGER, created_at TEXT NOT NULL);
        """)
        rows = [
            ("cos", "owner", "is the fleet green?", "2026-10-28T10:00:00.000Z", "telegram", None, None),
            ("cos", "agent", "yes: 3 lanes idle", "2026-10-28T10:00:30.000Z", None, None, None),
            ("poolrad", "owner", "what is the checkpoint?", "2026-10-28T11:00:00.000Z", None, 1, None),
            ("poolrad", "agent", "289 strict", "2026-10-28T11:00:05.000Z", None, None, None),
        ]
        for channel, role, body, created, direct, answer_id, answered in rows:
            db.execute("INSERT INTO messages (channel, role, body, created_at,"
                       " direct_transport, answer_id, answered_at) VALUES (?,?,?,?,?,?,?)",
                       (channel, role, body, created, direct, answer_id, answered))
        db.execute("INSERT INTO attachments (id, filename, mime, size, path, message_id,"
                   " created_at) VALUES ('a1','shot.png','image/png',23,?,1,"
                   " '2026-10-28T10:00:01.000Z')", (self.attachment,))
        db.commit()
        db.close()

    def tearDown(self):
        self.tmp.cleanup()

    def test_rows_are_copied_with_their_transport_and_their_time(self):
        report = migration.migrate(self.source, self.inbox, self.blobs)
        self.assertEqual([], report["errors"])
        self.assertEqual(4, report["migrated_total"])
        self.assertEqual({"web": 1, "telegram": 1, "report": 2},
                         {name: sum(1 for row in report["copied"]
                                    if row["transport"] == name)
                          for name in dunk_inbox.TRANSPORTS})
        connection = sqlite3.connect(self.inbox)
        connection.row_factory = sqlite3.Row
        rows = [dict(row) for row in connection.execute(
            "SELECT * FROM messages ORDER BY created_at")]
        self.assertEqual({"owner"}, {row["mailbox"] for row in rows})
        self.assertEqual("telegram", rows[0]["transport"])
        self.assertEqual(1793181600.0, rows[0]["created_at"], "the source timestamp survives")
        self.assertEqual(1, len(connection.execute(
            "SELECT * FROM attachments").fetchall()))
        connection.close()

    def test_the_source_store_is_never_modified(self):
        before = open(self.source, "rb").read()
        migration.migrate(self.source, self.inbox, self.blobs)
        self.assertEqual(before, open(self.source, "rb").read())

    def test_a_second_run_copies_nothing_and_says_so(self):
        migration.migrate(self.source, self.inbox, self.blobs)
        report = migration.migrate(self.source, self.inbox, self.blobs)
        self.assertEqual(0, report["migrated_total"])
        self.assertEqual(4, len(report["skipped"]))
        connection = sqlite3.connect(self.inbox)
        self.assertEqual(4, connection.execute("SELECT count(*) FROM messages").fetchone()[0])
        connection.close()

    def test_a_dry_run_writes_nothing(self):
        report = migration.migrate(self.source, self.inbox, self.blobs, dry_run=True)
        self.assertEqual(4, len(report["copied"]))
        self.assertFalse(os.path.exists(self.inbox), "a dry run creates no database")


if __name__ == "__main__":
    unittest.main()
