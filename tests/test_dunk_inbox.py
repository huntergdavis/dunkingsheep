"""Per-agent inboxes: the store, the Flock methods, and three-surface parity."""

import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_api  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox, InboxError  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class InboxStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def test_send_read_and_count_in_queue_order(self):
        for n in range(3):
            self.inbox.send("writer", f"message {n}", sender="pm")
        count = self.inbox.count("writer")
        self.assertEqual((3, 3), (count["unread"], count["total"]))
        first = self.inbox.read("writer", limit=2)
        self.assertEqual(["message 0", "message 1"], [m["body"] for m in first])
        self.assertEqual(1, self.inbox.count("writer")["unread"])
        self.assertEqual(["message 2"], [m["body"] for m in self.inbox.read("writer")])
        self.assertEqual(0, self.inbox.count("writer")["unread"])

    def test_peek_does_not_consume(self):
        self.inbox.send("writer", "still here")
        self.assertEqual(["still here"],
                         [m["body"] for m in self.inbox.read("writer", peek=True)])
        self.assertEqual(1, self.inbox.count("writer")["unread"])

    def test_mailboxes_are_separate(self):
        self.inbox.send("a", "for a")
        self.inbox.send("b", "for b")
        self.assertEqual(["for a"], [m["body"] for m in self.inbox.read("a")])
        self.assertEqual(1, self.inbox.count("b")["unread"])

    # -- ack / archive / undeliverable ------------------------------------

    def test_the_same_attachment_can_be_used_by_two_messages(self):
        path = os.path.join(self.tmp.name, "same.txt")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("same payload\n")
        first = self.inbox.send("a", "one", attachments=[path])
        second = self.inbox.send("b", "two", attachments=[path])
        rows = self.inbox.db.execute(
            "SELECT id, message_id, sha256 FROM attachments "
            "ORDER BY created_at").fetchall()
        self.assertEqual(2, len(rows))
        self.assertEqual(2, len({row["id"] for row in rows}), "row ids must be unique")
        self.assertEqual(1, len({row["sha256"] for row in rows}), "blob is shared")
        self.assertEqual({first["message_id"], second["message_id"]},
                         {row["message_id"] for row in rows})
        out = os.path.join(self.tmp.name, "out")
        for row in rows:
            written = self.inbox.fetch(row["id"], out)
            with open(written["path"], "rb") as handle:
                self.assertEqual(b"same payload\n", handle.read())

    def test_duplicate_attachments_in_one_message_get_distinct_ids(self):
        path = os.path.join(self.tmp.name, "dup.txt")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("dup\n")
        sent = self.inbox.send("a", "dup", attachments=[path, path])
        rows = self.inbox.db.execute(
            "SELECT id FROM attachments WHERE message_id = ?",
            (sent["message_id"],)).fetchall()
        self.assertEqual(2, len({row["id"] for row in rows}))

    def test_an_ack_is_scoped_to_the_callers_mailbox(self):
        # The VP acked a list that included his SENT copies; the recipient lane
        # lost its unread. An ack must never write another mailbox.
        self.inbox.send("bob", "from alice", sender="w1:p1")
        message_id = self.inbox.list("bob")[0]["message_id"]
        with self.assertRaises(InboxError):
            self.inbox.ack([message_id], mailbox="alice")
        self.assertEqual(1, self.inbox.count("bob")["unread"])
        # Sender identity grants nothing: the VP could not ack mail he wrote.
        with self.assertRaises(InboxError):
            self.inbox.ack([message_id], mailbox="alice")
        self.assertEqual(1, self.inbox.count("bob")["unread"])
        self.inbox.ack([message_id], mailbox="bob")
        self.assertEqual(0, self.inbox.count("bob")["unread"])

    def test_a_pane_suffixed_mailbox_is_the_same_mailbox(self):
        """The VP's own mail was refused: mailbox 'X (w9:pH)', caller 'X'.

        Both spellings exist in the table (a channel that resolves with its pane
        id appends it), so the comparison has to be on the MAILBOX, not on the
        string. Same for counting: an owner must see one inbox, not two.
        """
        message_id = self.inbox.send("alice", "hello", sender="w1:p1")["message_id"]
        self.inbox.db.execute("UPDATE messages SET mailbox = ? WHERE id = ?",
                              ("alice (w9:pH)", message_id))
        self.inbox.db.commit()
        self.assertEqual(1, self.inbox.count("alice")["unread"],
                         "the plain name must see the suffixed row")
        self.assertEqual(1, self.inbox.count("alice (w9:pH)")["unread"])
        self.inbox.ack([message_id], mailbox="alice")     # the owner, plain name
        self.assertEqual(0, self.inbox.count("alice")["unread"])
        self.assertEqual(0, self.inbox.count("alice (w9:pH)")["unread"])

    def test_a_foreign_mailbox_with_a_pane_suffix_is_still_refused(self):
        """Normalisation must not become a way to clear someone else's mail."""
        message_id = self.inbox.send("bob", "hello", sender="w1:p1")["message_id"]
        self.inbox.db.execute("UPDATE messages SET mailbox = ? WHERE id = ?",
                              ("bob (w9:pH)", message_id))
        self.inbox.db.commit()
        with self.assertRaises(InboxError):
            self.inbox.ack([message_id], mailbox="alice")
        self.assertEqual(1, self.inbox.count("bob")["unread"])

    def test_ack_stamps_read_at(self):
        sent = self.inbox.send("writer", "handled")
        self.inbox.ack([sent["message_id"]])
        status = self.inbox.status(sent["message_id"])
        self.assertEqual("acked", status["state"])
        self.assertIsNotNone(status["read_at"], "ack must stamp read_at")
        self.assertEqual(0, self.inbox.count("writer")["unread"])
        self.assertEqual(0, self.inbox.stats()[0]["unread"])

    def test_archive_is_bulk_and_never_deletes(self):
        for n in range(3):
            self.inbox.send("dead", f"m{n}")
        result = self.inbox.archive("dead")
        self.assertEqual(3, result["archived"])
        count = self.inbox.count("dead")
        self.assertEqual(0, count["unread"])
        self.assertEqual(3, count["archived"])
        self.assertEqual(3, count["total"], "archive must not delete")
        self.assertEqual(3, len(self.inbox.list("dead", unread_only=False)))

    def test_archive_before_only_touches_older_mail(self):
        old = self.inbox.send("dead", "old")
        self.inbox.db.execute("UPDATE messages SET created_at = 1 WHERE id = ?",
                              (old["message_id"],))
        self.inbox.db.commit()
        self.inbox.send("dead", "new")
        result = self.inbox.archive("dead", before=100)
        self.assertEqual(1, result["archived"])
        self.assertEqual(1, self.inbox.count("dead")["unread"])

    def test_a_dead_resolved_pane_marks_mail_undeliverable(self):
        sent = self.inbox.send("w9:p8", "stuck", pane_id="w9:p8")
        result = self.inbox.mark_undeliverable({"w2:p1"})
        self.assertEqual(1, result["undeliverable"])
        self.assertEqual("undeliverable",
                         self.inbox.status(sent["message_id"])["state"])
        count = self.inbox.count("w9:p8")
        self.assertEqual(0, count["unread"])
        self.assertEqual(1, count["undeliverable"])

    def test_old_dead_mailbox_without_pane_id_is_marked_undeliverable(self):
        # Older mail to a dead pane id was stored under the pane id as the
        # mailbox key with pane_id NULL; it must be swept too.
        sent = self.inbox.send("w9:p8", "old stuck", pane_id=None)
        result = self.inbox.mark_undeliverable({"w2:p1"})
        self.assertEqual(1, result["undeliverable"])
        self.assertIn("w9:p8", result["mailboxes"])
        self.assertEqual("undeliverable",
                         self.inbox.status(sent["message_id"])["state"])

    def test_ack_backfill_stamps_read_at_on_reopen(self):
        sent = self.inbox.send("writer", "old ack")
        self.inbox.db.execute(
            "UPDATE messages SET state = 'acked', acked_at = 123, read_at = NULL "
            "WHERE id = ?", (sent["message_id"],))
        self.inbox.db.commit()
        reopened = Inbox(path=self.inbox.path, blob_dir=self.inbox.blob_dir)
        self.addCleanup(reopened.close)
        self.assertEqual(123.0, reopened.status(sent["message_id"])["read_at"])

    # -- the owner's status query ------------------------------------------

    def test_status_reports_every_state_and_never_the_text(self):
        message = self.inbox.send("writer", "secret body text", subject="secret subject",
                                  sender="pm")
        mid = message["message_id"]
        status = self.inbox.status(mid)
        self.assertEqual("queued", status["state"])
        self.assertEqual("writer", status["destination"])
        self.assertEqual("pm", status["sender"])
        self.assertIsNotNone(status["created_at"])
        self.assertIsNone(status["read_at"])
        self.assertEqual(len("secret body text"), status["body_bytes"])
        blob = " ".join(str(v) for v in status.values())
        self.assertNotIn("secret body", blob, "status must not leak the body")
        self.assertNotIn("secret subject", blob, "status must not leak the subject")

        self.inbox.read("writer")
        status = self.inbox.status(mid)
        self.assertEqual("read", status["state"])
        self.assertIsNotNone(status["read_at"])
        self.assertIsNone(status["waiting_s"])

        self.inbox.ack([mid])
        status = self.inbox.status(mid)
        self.assertEqual("acked", status["state"])
        self.assertIsNotNone(status["acked_at"])

    def test_status_of_a_cancelled_message(self):
        mid = self.inbox.send("writer", "never mind")["message_id"]
        self.inbox.cancel([mid])
        status = self.inbox.status(mid)
        self.assertEqual("cancelled", status["state"])
        self.assertIsNotNone(status["cancelled_at"])
        self.assertEqual(0, self.inbox.count("writer")["unread"])

    def test_unknown_ids_are_a_clear_error_everywhere(self):
        for call in (lambda: self.inbox.status("i999"),
                     lambda: self.inbox.get("i999"),
                     lambda: self.inbox.ack(["i999"]),
                     lambda: self.inbox.cancel(["i999"]),
                     lambda: self.inbox.delete(["i999"]),
                     lambda: self.inbox.fetch("nope", self.tmp.name)):
            with self.assertRaises(InboxError) as caught:
                call()
            self.assertIn("i999" if "i999" in str(caught.exception) else "nope",
                          str(caught.exception))

    def test_a_read_message_cannot_be_cancelled(self):
        mid = self.inbox.send("writer", "too late")["message_id"]
        self.inbox.read("writer")
        result = self.inbox.cancel([mid])
        self.assertEqual([], result["changed"])
        self.assertEqual("read", result["unchanged"][0]["state"])

    def test_ids_are_stable_and_survive_a_restart(self):
        mid = self.inbox.send("writer", "durable", subject="keep me")["message_id"]
        path, blobs = self.inbox.path, self.inbox.blob_dir
        self.inbox.close()
        reopened = Inbox(path=path, blob_dir=blobs)
        try:
            self.assertEqual("queued", reopened.status(mid)["state"])
            self.assertEqual("durable", reopened.read("writer")[0]["body"])
            # And the next id does not collide with the one from before.
            self.assertNotEqual(mid, reopened.send("writer", "next")["message_id"])
        finally:
            reopened.close()

    # -- attachments --------------------------------------------------------

    def test_attachments_round_trip(self):
        source = os.path.join(self.tmp.name, "patch.diff")
        with open(source, "w") as handle:
            handle.write("--- a\n+++ b\n")
        message = self.inbox.send("writer", "see attached", attachments=[source])
        blob = message["attachments"][0]
        self.assertEqual("patch.diff", blob["filename"])
        self.assertEqual(12, blob["size"])
        dest = os.path.join(self.tmp.name, "out")
        written = self.inbox.fetch(blob["id"], dest)
        self.assertEqual(open(written["path"]).read(), "--- a\n+++ b\n")
        self.assertEqual(1, self.inbox.count("writer")["attachments_unread"])
        # Deleting the message takes the blob with it.
        self.inbox.delete([message["message_id"]])
        self.assertEqual([], os.listdir(self.inbox.blob_dir))

    def test_attaching_a_missing_file_is_refused(self):
        with self.assertRaisesRegex(InboxError, "no such file"):
            self.inbox.send("writer", "body", attachments=["/no/such/file"])

    def test_an_empty_message_is_refused(self):
        with self.assertRaisesRegex(InboxError, "body or an attachment"):
            self.inbox.send("writer", "   ")

    def test_purge_keeps_unread_mail_and_drops_old_read_mail(self):
        old = self.inbox.send("writer", "old news")["message_id"]
        self.inbox.read("writer")
        self.inbox.send("writer", "still waiting")
        self.assertEqual({"purged": 1}, self.inbox.purge(older_than_s=-1))
        self.assertEqual(1, self.inbox.count("writer")["unread"])
        with self.assertRaises(InboxError):
            self.inbox.status(old)


class InboxThroughTheFlockTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def test_a_mailbox_key_identifies_one_pane_and_is_never_the_agent_type(self):
        key, label, pane = self.flock.resolve_mailbox("codex")
        self.assertEqual("w2:p1", pane)
        self.assertNotEqual("codex", key, "the agent type is not an identity")
        self.assertEqual("Codex Site", key, "the tab label names this one pane")
        self.assertIn("w2:p1", label)

    def test_mail_can_be_left_for_an_agent_that_is_not_running(self):
        key, label, pane = self.flock.resolve_mailbox("not-started-yet")
        self.assertEqual("not-started-yet", key)
        self.assertIsNone(pane)
        sent = self.flock.inbox_send("not-started-yet", "waiting for you", notify=True)
        self.assertEqual(1, self.flock.inbox_count("not-started-yet")["unread"])
        self.assertFalse(sent["notified"], "nothing to nudge")

    def test_send_nudges_the_recipient_with_one_short_line(self):
        self.herdr.set_screen("w2:p1", None)       # unreadable: nudge still queues
        sent = self.flock.inbox_send("codex", "a very long message " * 500,
                                     subject="big", self_pane_id="w1:p2")
        self.assertTrue(sent["notified"])
        # The sender is the raw pane id: unique, stable, and never a
        # sibling pane's mailbox label.
        self.assertEqual("w1:p2",
                         self.flock.inbox.status(sent["message_id"])["sender"])
        nudge = self.flock.list_messages()
        texts = [m["text"] for m in nudge["queued"]] + [m["text"] for m in nudge["recent"]]
        self.assertTrue(any("inbox" in t and len(t) < 200 for t in texts),
                        "the only thing typed is a short nudge")

    def test_self_defaults_to_the_callers_own_mailbox(self):
        self.flock.inbox_send("codex", "for codex")
        self.assertEqual(1, self.flock.inbox_count(self_pane_id="w2:p1")["unread"])
        read = self.flock.inbox_read(self_pane_id="w2:p1")
        self.assertEqual(["for codex"], [m["body"] for m in read["messages"]])

    def test_sending_to_a_dead_pane_id_errors(self):
        with self.assertRaisesRegex(DunkError, "no live pane"):
            self.flock.resolve_mailbox("w9:p8")
        with self.assertRaisesRegex(DunkError, "no live pane"):
            self.flock.inbox_send("w9:p8", "lost")
        self.assertEqual(0, self.inbox.count("w9:p8")["total"],
                        "the failed send must not store a message")

    def test_archive_through_the_flock_drains_unread(self):
        for n in range(2):
            self.flock.inbox_send("dead-mailbox", f"m{n}", notify=False)
        result = self.flock.inbox_archive("dead-mailbox")
        self.assertEqual(2, result["archived"])
        count = self.flock.inbox_count("dead-mailbox")
        self.assertEqual(0, count["unread"])
        self.assertEqual(2, count["archived"])

    def test_counting_without_a_mailbox_or_a_caller_is_a_clear_error(self):
        with self.assertRaisesRegex(DunkError, "no mailbox given"):
            self.flock.inbox_count()

    def test_stats_across_mailboxes(self):
        self.flock.inbox_send("w2:p1", "one", notify=False)
        self.flock.inbox_send("w1:p2", "two", notify=False)
        self.flock.inbox_send("w1:p2", "three", notify=False)
        stats = {m["mailbox"]: m["unread"] for m in self.flock.inbox_stats()["mailboxes"]}
        self.assertEqual({"Codex Site": 1, "Claude Proj": 2}, stats)
        self.assertEqual(3, self.flock.status()["inbox_unread"])


class MailboxIdentityTests(unittest.TestCase):
    """Two agents of the same kind must never share a mailbox."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def test_two_panes_of_the_same_agent_type_get_different_mailboxes(self):
        # The field bug: three Codex panes all resolved to the key 'codex', so
        # a message for one was readable from the others.
        first = self.flock.resolve_mailbox("w1:p2")[0]     # claude
        second = self.flock.resolve_mailbox("w2:p3")[0]    # also claude
        self.assertNotEqual(first, second)
        self.assertNotIn("claude", (first, second), "the agent type is not an identity")
        self.flock.inbox_send("w1:p2", "for the first one", notify=False)
        self.assertEqual(1, self.flock.inbox_count("w1:p2")["unread"])
        self.assertEqual(0, self.flock.inbox_count("w2:p3")["unread"],
                         "the other pane of the same type must not see it")

    def test_every_live_agent_pane_has_its_own_mailbox(self):
        keys = [self.flock.resolve_mailbox(p["pane_id"])[0]
                for p in self.herdr.list_panes() if p.get("agent")]
        self.assertEqual(len(keys), len(set(keys)), f"collision among {keys}")

    def test_the_same_pane_resolves_the_same_way_however_it_is_addressed(self):
        by_id = self.flock.resolve_mailbox("w2:p1")[0]
        by_tab = self.flock.resolve_mailbox("Codex Site")[0]
        by_self = self.flock.resolve_mailbox("self", self_pane_id="w2:p1")[0]
        self.assertEqual({by_id}, {by_tab, by_self})

    def test_a_duplicate_name_is_refused_with_the_candidates(self):
        for pane in self.herdr.panes:
            if pane["pane_id"] in ("w1:p2", "w2:p3"):
                pane["label"] = "codex poolrad"
        with self.assertRaisesRegex(DunkError, "ambiguous") as raised:
            self.flock.resolve_mailbox("codex poolrad")
        message = str(raised.exception)
        self.assertIn("w1:p2", message)
        self.assertIn("w2:p3", message)
        # An exact pane id still wins over the duplicate name.
        self.assertEqual("w1:p2", self.flock.resolve_mailbox("w1:p2")[2])

    def test_sender_is_the_raw_pane_id_not_a_sibling_label(self):
        sent = self.flock.inbox_send("w2:p1", "hi", notify=False,
                                     self_pane_id="wS:p1")
        self.assertEqual("wS:p1",
                         self.flock.inbox.status(sent["message_id"])["sender"])

    def test_undeliverable_mail_older_than_24h_is_auto_archived(self):
        import time
        now = time.time()
        old = self.inbox.send("w1:p2", "old")
        new = self.inbox.send("w1:p2", "new")
        with self.inbox._lock:
            self.inbox.db.execute("UPDATE messages SET state='undeliverable', "
                                  "reason='target pane no longer exists', created_at=? "
                                  "WHERE id=?", (now - 25 * 3600, old["message_id"]))
            self.inbox.db.execute("UPDATE messages SET state='undeliverable', "
                                  "reason='target pane no longer exists', created_at=? "
                                  "WHERE id=?", (now - 2 * 3600, new["message_id"]))
            self.inbox.db.commit()
        self.assertEqual(1, self.inbox.auto_archive_undeliverable(now=now))
        old_row = self.inbox.status(old["message_id"])
        new_row = self.inbox.status(new["message_id"])
        self.assertEqual("archived", old_row["state"])
        self.assertEqual("undeliverable", new_row["state"])
        self.assertEqual("target pane no longer exists", old_row["reason"])

    def test_client_refuses_to_auto_start_a_partial_tmux_backend(self):
        from dunk_client import DaemonUnavailable, FlockClient
        env = {"DUNKINGSHEEP_TERMINAL_BACKEND": "tmux"}
        with mock.patch.dict(os.environ, env, clear=False):
            os.environ.pop("DUNKINGSHEEP_TMUX_SOCKET", None)
            with self.assertRaisesRegex(DaemonUnavailable,
                                        "refusing to auto-start a tmux daemon"):
                FlockClient(socket_path="/tmp/no-such-dsh-partial.sock").start_daemon()

    def test_client_uses_the_exported_pane_id_without_asking_herdr(self):
        from dunk_client import FlockClient
        with mock.patch.dict(os.environ, {"HERDR_PANE_ID": "wR:p4"},
                             clear=False), \
                mock.patch("dunk_client._current_pane_id") as current:
            self.assertEqual("wR:p4", FlockClient().self_pane_id)
            current.assert_not_called()

    def test_client_uses_tmux_pane_when_herdr_is_absent(self):
        from dunk_client import FlockClient
        with mock.patch.dict(os.environ, {"TMUX_PANE": "%3"}, clear=True), \
                mock.patch("dunk_client._current_pane_id") as current:
            self.assertEqual("%3", FlockClient().self_pane_id)
            current.assert_not_called()

    def test_opt_in_verify_reconciles_a_stale_exported_pane(self):
        from dunk_client import FlockClient
        with mock.patch.dict(
                os.environ,
                {"HERDR_PANE_ID": "wR:p4", "DUNKINGSHEEP_VERIFY_SELF": "1"},
                clear=False), \
                mock.patch("dunk_client._current_pane_id", return_value="wS:p1"):
            self.assertEqual("wS:p1", FlockClient().self_pane_id)

    def test_no_exported_pane_falls_back_to_detection(self):
        from dunk_client import FlockClient
        with mock.patch.dict(os.environ, {}, clear=True), \
                mock.patch("dunk_client._current_pane_id", return_value="wS:p1"):
            self.assertEqual("wS:p1", FlockClient().self_pane_id)

    def test_an_agent_type_naming_several_panes_is_refused(self):
        with self.assertRaisesRegex(DunkError, "ambiguous"):
            self.flock.resolve_mailbox("claude")

    def test_a_pane_id_is_used_when_nothing_else_is_unique(self):
        for pane in self.herdr.panes:
            if pane["pane_id"] in ("w1:p2", "w2:p3"):
                pane["label"] = "twins"          # same label on two panes
        for tab in self.herdr.tabs:
            tab["label"] = "same tab name"
        key_a = self.flock.resolve_mailbox("w1:p2")[0]
        key_b = self.flock.resolve_mailbox("w2:p3")[0]
        self.assertEqual({"w1:p2", "w2:p3"}, {key_a, key_b},
                         "fall back to the pane id rather than share a mailbox")

    def test_mail_filed_under_a_shared_mailbox_is_quarantined_not_delivered(self):
        # What an older version left behind: messages filed under 'claude'.
        self.inbox.send("claude", "meant for one of them")
        moved = self.flock._quarantine_shared_mailboxes()
        self.assertEqual([{"was": "claude", "now": "unrouted:claude", "messages": 1}],
                         moved)
        self.assertEqual(0, self.inbox.count("claude")["total"])
        self.assertEqual(1, self.inbox.count("unrouted:claude")["unread"])
        # And no pane resolves to the quarantine, so nobody reads it by accident.
        keys = {self.flock.resolve_mailbox(p["pane_id"])[0]
                for p in self.herdr.list_panes() if p.get("agent")}
        self.assertNotIn("unrouted:claude", keys)


class InboxErrorShapeTests(unittest.TestCase):
    """An unknown id must read like a mistake, not like a crash."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False,
                           inbox=Inbox(path=os.path.join(self.tmp.name, "i.db"),
                                       blob_dir=os.path.join(self.tmp.name, "b")))

    def tearDown(self):
        self.flock.close()
        self.tmp.cleanup()

    def test_unknown_ids_come_back_as_plain_caller_errors(self):
        for command, args in (("inbox_status", {"message_id": "i999"}),
                              ("inbox_ack", {"message_ids": "i999"}),
                              ("inbox_cancel", {"message_ids": "i999"}),
                              ("inbox_delete", {"message_ids": "i999"}),
                              ("inbox_fetch", {"attachment_id": "nope"})):
            with self.assertRaises(DunkError) as caught:
                dunk_api.dispatch(self.flock, command, args, self_pane_id="w1:p2")
            self.assertNotIn("Traceback", str(caught.exception))
            self.assertIn("no ", str(caught.exception))

    def test_a_bad_attachment_path_is_a_caller_error_too(self):
        with self.assertRaisesRegex(DunkError, "no such file"):
            dunk_api.dispatch(self.flock, "inbox_send",
                              {"to": "someone", "body": "hi", "attach": "/no/such/file",
                               "notify": False}, self_pane_id="w1:p2")


class MailboxRoutingTests(unittest.TestCase):
    """A mailbox NAME is a conversation; a pane id is a process.

    On 2026-10-22 a message addressed to 'codex-poolrad2 (wR:p5)' was answered by
    wR:p4 and that lane stood down believing it was the duplicate. The decorated
    address fell through to a mailbox NAME with no pane, so delivery picked
    whichever pane owned the name.
    """

    def setUp(self):
        import dunk_core
        from fakes import FakeHerdr
        self.herdr = FakeHerdr(panes=[
            {"pane_id": "wR:p4", "tab_id": "wR:t4", "workspace_id": "wR",
             "label": "codex-poolrad2", "agent": "codex"},
            {"pane_id": "wR:p5", "tab_id": "wR:t5", "workspace_id": "wR",
             "label": "codex-poolrad2", "agent": "codex"},
        ])
        self.herdr.tabs = [
            {"tab_id": "wR:t4", "label": "codex-poolrad2", "number": 1, "workspace_id": "wR"},
            {"tab_id": "wR:t5", "label": "codex-poolrad2", "number": 2, "workspace_id": "wR"},
        ]
        self.herdr.workspaces = [{"workspace_id": "wR", "label": "poolrad", "number": 1}]
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False)
        self.dunk_core = dunk_core

    def test_a_decorated_address_delivers_to_the_pane_it_names(self):
        for pane in ("wR:p5", "wR:p4"):
            key, _label, resolved = self.flock.resolve_mailbox(f"codex-poolrad2 ({pane})")
            self.assertEqual(pane, resolved)
            self.assertEqual(pane, key, "the key must identify the pane, not the name")

    def test_a_decorated_address_for_a_dead_pane_is_refused(self):
        with self.assertRaises(self.dunk_core.DunkError) as caught:
            self.flock.resolve_mailbox("codex-poolrad2 (wR:p9)")
        self.assertIn("wR:p9", str(caught.exception))
        self.assertIn("not live", str(caught.exception))

    def test_a_bare_name_shared_by_two_panes_is_refused_with_both(self):
        with self.assertRaises(self.dunk_core.AmbiguousTarget) as caught:
            self.flock.resolve_mailbox("codex-poolrad2")
        message = str(caught.exception)
        self.assertIn("wR:p4", message)
        self.assertIn("wR:p5", message)

    def test_a_bare_dead_pane_id_is_not_a_mailbox_even_for_a_letter_workspace(self):
        with self.assertRaises(self.dunk_core.DunkError):
            self.flock.resolve_mailbox("wR:p9")


class SurfaceParityTests(unittest.TestCase):
    """Every registry command must exist on all three surfaces."""

    def test_every_command_is_reachable_from_the_cli(self):
        help_text = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "--help"],
            capture_output=True, text=True, timeout=60).stdout
        inbox_help = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "inbox", "--help"],
            capture_output=True, text=True, timeout=60).stdout
        task_help = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "task", "--help"],
            capture_output=True, text=True, timeout=60).stdout
        superuser_help = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "superuser", "--help"],
            capture_output=True, text=True, timeout=60).stdout
        memory_help = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "memory", "--help"],
            capture_output=True, text=True, timeout=60).stdout
        pm_help = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "pm", "--help"],
            capture_output=True, text=True, timeout=60).stdout
        blob = help_text + inbox_help + task_help + superuser_help + memory_help + pm_help
        # The CLI spells commands as subcommands, sometimes grouped or renamed;
        # this maps each registry command to the words that must appear.
        spelled = {
            "status": "status", "list_dunks": "list", "get_dunk": "get",
            "add_dunk": "add", "update_dunk": "update", "remove_dunk": "remove",
            "start_dunk": "start", "stop_dunk": "stop", "toggle_dunk": "toggle",
            "stop_all": "stop-all", "fire_dunk": "fire", "send_text": "send",
            "send_keys": "send-keys", "flush_box": "flush", "list_messages": "messages",
            "cancel_message": "cancel-message", "list_panes": "panes",
            "read_pane": "read", "list_workspaces": "workspaces",
            "create_workspace": "new-workspace", "create_tab": "new-tab",
            "split_pane": "split", "start_agent": "start-agent", "rename": "rename",
            "focus": "focus", "close": "close", "herdr": "herdr",
            "herdr_help": "herdr-help", "help": "guide", "shutdown": "shutdown",
            "get_message": "messages", "metrics": "metrics",
            "inbox_send": "send", "inbox_count": "count", "inbox_list": "list",
            "inbox_read": "read", "inbox_status": "status", "inbox_ack": "ack",
            "inbox_cancel": "cancel", "inbox_delete": "delete",
            "inbox_stats": "stats", "inbox_fetch": "fetch",
            "inbox_archive": "archive",
            "task_add": "add", "task_list": "list", "task_read": "read",
            "task_update": "update", "task_assign": "assign",
            "task_complete": "complete", "task_block": "block",
            "task_unblock": "unblock", "task_cancel": "cancel",
            "task_delete": "delete", "task_note": "note", "task_next": "next",
            "task_heartbeat": "heartbeat", "task_release": "release",
            "task_follow": "follow", "task_unfollow": "unfollow",
            "task_followers": "followers", "task_stats": "stats",
            "task_metrics": "metrics", "task_reap": "reap",
            "superuser_status": "status", "superuser_add": "add",
            "superuser_remove": "remove", "superuser_recover": "recover",
            "memory_recall": "recall", "memory_context": "context",
            "memory_search": "search", "memory_stats": "stats",
            "memory_status": "status", "memory_ingest": "ingest",
            "memory_backfill": "backfill",
            "health": "health", "recover": "recover", "doctor": "doctor",
            "poll": "poll",
            "turn_start": "turn", "turn_end": "turn",
            "pm_digest": "digest", "pm_routing": "routing",
            "clear_box": "clear-box", "delivery": "delivery", "dispatch": "dispatch",
            "retire_pane": "retired", "api": "api",
            "state_capture": "state", "state_latest": "state",
            "roster": "roster", "message_state": "message-state",
            "reply_stats": "reply-stats", "project_summary": "project-summary",
        }
        missing = [c.name for c in dunk_api.COMMANDS if c.name not in spelled]
        self.assertEqual([], missing,
                         "new registry commands need a CLI subcommand and an entry here")
        for command, word in spelled.items():
            self.assertIn(word, blob, f"{command} has no {word!r} in the CLI help")

    def test_every_inbox_command_is_an_mcp_tool(self):
        tools = {c.name for c in dunk_api.mcp_commands()}
        for command in dunk_api.COMMANDS:
            if command.name.startswith("inbox_"):
                self.assertIn(command.name, tools)
                for prop in command.input_schema()["properties"].values():
                    self.assertIn(prop["type"],
                                  ("string", "number", "integer", "boolean"))


if __name__ == "__main__":
    unittest.main()
