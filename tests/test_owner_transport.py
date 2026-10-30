"""t594: one owner mailbox, two transports - ingest, outbox and delivery receipts.

FLAT_FLEET.md 7.2: an owner question records where it arrived; a reply goes back
the same way; Telegram carries answers to Telegram questions and nothing else.
These are the daemon-side halves a bridge needs, so the bridge can be a transport
instead of a second store.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox, InboxError  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER, DESK, WORKER = "%95", "%96", "%99"


class OwnerTransportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock = Flock(
            herdr=FakeHerdr(),
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False,
            restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "roles.json"),
                        tokens_path=os.path.join(self.tmp.name, "tokens.json")),
        )
        self.flock.roles.register(OWNER, role="owner", by=OWNER)
        self.flock.roles.register(DESK, role="desk", by=OWNER)
        self.flock.roles.register(WORKER, role="worker", by=OWNER)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def ask(self, body="are we green?", transport="telegram", external_id=None):
        return self.flock.owner_ingest(body, transport, external_id=external_id,
                                       self_pane_id=DESK)["message"]

    def test_an_ingested_question_records_its_transport(self):
        question = self.ask(transport="telegram", external_id="telegram:1")
        self.assertEqual("owner", question["mailbox"])
        self.assertEqual("telegram", question["transport"])
        self.assertEqual("telegram:1", question["external_id"])
        self.assertIsNone(question["reply_to"])

    def test_a_redelivered_update_does_not_become_a_second_question(self):
        first = self.ask(external_id="telegram:7")
        again = self.flock.owner_ingest("are we green?", "telegram",
                                        external_id="telegram:7",
                                        self_pane_id=DESK)
        self.assertTrue(again["duplicate"])
        self.assertEqual(first["message_id"], again["message"]["message_id"])
        rows = self.flock.inbox.conversation("owner")
        self.assertEqual(1, len(rows))

    def test_the_outbox_holds_replies_for_their_own_transport_only(self):
        telegram_q = self.ask("telegram question", transport="telegram")
        web_q = self.ask("web question", transport="web")
        self.flock.owner_reply(telegram_q["message_id"], "telegram answer", self_pane_id=DESK)
        self.flock.owner_reply(web_q["message_id"], "web answer", self_pane_id=DESK)
        telegram = self.flock.owner_outbox("telegram", self_pane_id=DESK)
        web = self.flock.owner_outbox("web", self_pane_id=DESK)
        self.assertEqual(["telegram answer"], [m["body"] for m in telegram["messages"]])
        self.assertEqual(["web answer"], [m["body"] for m in web["messages"]])

    def test_a_web_question_produces_nothing_for_telegram(self):
        """The acceptance line: Telegram must not echo the web conversation."""
        web_q = self.ask("asked on the web", transport="web")
        self.flock.owner_reply(web_q["message_id"], "answered on the web", self_pane_id=DESK)
        self.assertEqual([], self.flock.owner_outbox("telegram",
                                                     self_pane_id=DESK)["messages"])

    def test_an_answered_telegram_question_is_visible_on_the_web(self):
        """The web is the complete record: it sees the Telegram exchange too."""
        q = self.ask("asked on telegram", transport="telegram")
        self.flock.owner_reply(q["message_id"], "answered on telegram", self_pane_id=DESK)
        conversation = self.flock.owner_conversation(self_pane_id=OWNER)
        bodies = [row["body"] for row in conversation["messages"]]
        self.assertIn("asked on telegram", bodies)
        self.assertIn("answered on telegram", bodies)
        transports = {row["body"]: row["transport"]
                      for row in conversation["messages"]}
        self.assertEqual("telegram", transports["answered on telegram"])

    def test_a_lane_question_is_never_in_the_owner_outbox(self):
        """Lane mail has no transport, so it cannot leak onto Telegram."""
        self.flock.inbox_send("w2:p1", "lane to lane", notify=False,
                              self_pane_id=DESK)
        self.assertEqual([], self.flock.owner_outbox("telegram",
                                                     self_pane_id=DESK)["messages"])

    def test_delivery_writes_one_idempotent_receipt(self):
        q = self.ask(transport="telegram")
        reply = self.flock.owner_reply(q["message_id"], "the answer", self_pane_id=DESK)
        message_id = reply["reply"]["message_id"]
        self.assertEqual([message_id],
                         [m["message_id"] for m in self.flock.owner_outbox(
                             "telegram", self_pane_id=DESK)["messages"]])
        first = self.flock.owner_delivered(message_id, "telegram",
                                           external_id="telegram:99",
                                           self_pane_id=DESK)
        self.assertIsNotNone(first["delivered_at"])
        # Drained: the outbox no longer offers it.
        self.assertEqual([], self.flock.owner_outbox("telegram",
                                                     self_pane_id=DESK)["messages"])
        second = self.flock.owner_delivered(message_id, "telegram",
                                            self_pane_id=DESK)
        self.assertEqual(first["delivered_at"], second["delivered_at"],
                         "a retry must not rewrite the first receipt")

    def test_a_receipt_must_match_the_row_transport(self):
        q = self.ask(transport="telegram")
        reply = self.flock.owner_reply(q["message_id"], "the answer", self_pane_id=DESK)
        with self.assertRaises(DunkError):
            self.flock.owner_delivered(reply["reply"]["message_id"], "web",
                                       self_pane_id=DESK)

    def test_a_worker_lane_may_not_ingest_read_or_confirm(self):
        """A lane is not a transport: the owner mailbox stays the desk's and the owner's."""
        with self.assertRaises(DunkError):
            self.flock.owner_ingest("sneaky", "telegram", self_pane_id=WORKER)
        with self.assertRaises(DunkError):
            self.flock.owner_outbox("telegram", self_pane_id=WORKER)
        q = self.ask(transport="telegram")
        with self.assertRaises(DunkError):
            self.flock.owner_delivered(q["message_id"], "telegram", self_pane_id=WORKER)

    def test_ingest_needs_a_real_transport(self):
        with self.assertRaises(DunkError):
            self.flock.owner_ingest("no transport", None, self_pane_id=DESK)
        with self.assertRaises(DunkError):
            self.flock.owner_ingest("bad transport", "carrier-pigeon",
                                    self_pane_id=DESK)

    def test_allowlist_matches_the_verbs(self):
        """The allowlist is the enforcement: system bridges in, lanes out."""
        import dunk_roles
        for verb in ("owner_ingest", "owner_outbox", "owner_delivered"):
            self.assertTrue(dunk_roles.allowed("system", verb), verb)
            self.assertTrue(dunk_roles.allowed("owner", verb), verb)
            self.assertFalse(dunk_roles.allowed("worker", verb), verb)
            self.assertFalse(dunk_roles.allowed("reviewer", verb), verb)
            self.assertFalse(dunk_roles.allowed("anonymous", verb), verb)
        self.assertTrue(dunk_roles.allowed("desk", "owner_outbox"))
        self.assertFalse(dunk_roles.allowed("desk", "owner_ingest"))
        # The web IS a transport for the same mailbox (FLAT_FLEET 4.3: the owner's
        # transport may call everything but the fleet-destructive verbs), so it
        # may ingest and confirm. Lanes may not, and that is the guard that matters.
        self.assertTrue(dunk_roles.allowed("web", "owner_ingest"))
        self.assertTrue(dunk_roles.allowed("web", "owner_delivered"))


if __name__ == "__main__":
    unittest.main()
