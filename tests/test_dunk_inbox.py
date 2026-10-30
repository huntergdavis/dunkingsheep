"""Per-agent inboxes: the store, the Flock methods, and three-surface parity."""

import os
import subprocess
import time
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_api  # noqa: E402
import dunk_core  # noqa: E402
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

    def test_mail_to_a_retired_mailbox_is_archived_and_kept(self):
        """Retention is not 'keep alerting': the rows stay, the state moves on."""
        old = self.inbox.send("crackdown-codex", "old mail", sender="w9:pH")["message_id"]
        young = self.inbox.send("crackdown-codex", "new mail", sender="w9:pH")["message_id"]
        self.inbox.db.execute("UPDATE messages SET created_at = ? WHERE id = ?",
                              (time.time() - 42 * 3600, old))
        self.inbox.db.commit()
        archived = self.inbox.archive_retired({"crackdown-codex"}, now=time.time())
        self.assertEqual(1, archived, "only the mail older than the age is archived")
        rows = {row["id"]: dict(row) for row in self.inbox.db.execute(
            "SELECT id, state, archived_at, read_at, reason FROM messages")}
        self.assertEqual("archived", rows[old]["state"])
        self.assertIsNotNone(rows[old]["archived_at"])
        self.assertIsNotNone(rows[old]["read_at"], "archived mail must not count as waiting")
        self.assertIn("retired", rows[old]["reason"])
        self.assertEqual("queued", rows[young]["state"], "young mail is left alone")
        self.assertEqual(0, self.inbox.count("crackdown-codex")["unread"] - 1,
                         "the archived row no longer counts as unread")

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

    def test_a_created_pane_is_recorded_as_the_lane_it_was_born_as(self):
        """The write-back a backend flip needs: the pane id of a lane's new pane
        is recorded the moment it exists, under the lane's own mailbox name."""
        made = self.flock.create_workspace(label="restored", cwd="/tmp",
                                          mailbox="dunky pi")
        pane_id = made["pane_id"]
        self.assertTrue(pane_id)
        self.assertEqual("dunky pi",
                         self.flock._row_identity(self.flock._lane_registry()[pane_id])[0])
        key, label, resolved = self.flock.resolve_mailbox("dunky pi")
        self.assertEqual(pane_id, resolved, "mail for the lane must reach its new pane")
        self.assertEqual("dunky pi", key, "the key is the recorded name, not the title")
        self.assertIn(pane_id, label)

    def test_the_registry_beats_a_title_the_agent_rewrote(self):
        """claude rewrites its pane title every turn. A lane found by its title is
        a lane lost on the next turn; the registry row is the record."""
        made = self.flock.create_workspace(label="ports", cwd="/tmp",
                                          mailbox="skies-android")
        pane_id = made["pane_id"]
        for pane in self.herdr.panes:
            if pane["pane_id"] == pane_id:
                pane["terminal_title"] = "✳ Investigating a build failure"
                pane["terminal_title_stripped"] = "✳ Investigating a build failure"
                pane["label"] = "Investigating a build failure"
        key, _, resolved = self.flock.resolve_mailbox("skies-android")
        self.assertEqual(pane_id, resolved)
        self.assertEqual("skies-android", key)

    def test_reading_and_acking_a_dead_name_still_works(self):
        """Regression 2026-10-24: fail-closed leaked into the READ path, so 78
        stranded messages became unreadable and the triage could not run. Only a
        send refuses; a dead name is still a mailbox name."""
        self.inbox.send("w9:pS", "from the void", sender="pm")
        self.inbox.send("PM (par-a) (wW:p1)", "decorated", sender="pm")
        self.inbox.send("w9:pS", "a second one to archive", sender="pm")
        count = self.flock.inbox_count("w9:pS")
        self.assertEqual(2, count["unread"])
        first = [m["body"] for m in self.flock.inbox_read("w9:pS", limit=1)["messages"]]
        self.assertEqual(["from the void"], first)
        self.assertEqual(1, self.flock.inbox_count("PM (par-a) (wW:p1)")["unread"])
        self.assertEqual(1, self.flock.inbox_archive("w9:pS")["archived"],
                         "the unread one is still archivab1e after the read")
        with self.assertRaisesRegex(DunkError, "reason=dead_lane"):
            self.flock.inbox_send("w9:pS", "no", notify=False)

    def test_a_dead_herdr_pane_id_is_not_a_mailbox_either(self):
        """The vp lane's id ends in a letter (w9:pH), so the narrow numeric
        pattern let it through and the send was accepted as a NEW mailbox name -
        the VP's own ruling got filed into the void that way (2026-10-24)."""
        from dunk_core import DunkError as _DunkError
        for dead in ("w9:pH", "wR:p5", "w11:p1"):
            with self.assertRaisesRegex(_DunkError, "dead pane id is not a mailbox"):
                self.flock.resolve_mailbox(dead, strict=True)

    def test_a_dead_tmux_pane_id_is_not_a_mailbox(self):
        """Both backends' ids are identities; neither is allowed to become a
        mailbox name just because its pane is gone."""
        from dunk_core import DunkError as _DunkError
        with self.assertRaisesRegex(_DunkError, "dead pane id is not a mailbox"):
            self.flock.resolve_mailbox("%99", strict=True)

    def test_a_mailbox_named_after_a_live_tmux_pane_resolves(self):
        made = self.flock.create_workspace(label="tmuxish", cwd="/tmp")
        pane_id = made["pane_id"]
        self.assertTrue(pane_id.startswith(("w", "%")))
        key, _, resolved = self.flock.resolve_mailbox(pane_id)
        self.assertEqual(pane_id, resolved)

    def test_a_mailbox_recorded_against_a_dead_pane_does_not_resolve_to_a_live_one(self):
        self.assertEqual((None, None), self.flock._lane_pane("never-recorded"))
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_DIR": self.tmp.name}):
            self.flock._remember_lane("w9:pH", "ghost lane")
            self.assertEqual((None, None), self.flock._lane_pane("ghost lane"))
            key, _, pane = self.flock.resolve_mailbox("ghost lane")
            self.assertEqual("ghost lane", key, "an unstarted agent still gets mail")
            self.assertIsNone(pane)

    def test_a_system_sender_is_not_killed_by_a_fail_closed_refusal(self):
        """The watchdog escalates to a lane that may have been handed over minutes
        ago. Raising took the exception watchdog down twice on 2026-10-24 and the
        fleet ran blind for nine minutes: the alert is stored, marked undeliverable,
        and the sender is told. An AGENT still gets the hard refusal."""
        result = self.flock.inbox_send("nobody-at-all", "alert body", notify=False,
                                       system_sender="watchdog")
        self.assertEqual("undeliverable", result.get("outcome"))
        self.assertIn("unknown_name", result.get("reason") or "")
        self.assertEqual(1, self.inbox.count("nobody-at-all")["total"],
                         "the alert is kept, not dropped")
        with self.assertRaisesRegex(DunkError, "reason=unknown_name"):
            self.flock.inbox_send("nobody-at-all", "agent mail", notify=False)

    def test_an_unknown_name_is_refused_rather_than_becoming_a_mailbox(self):
        """FAIL CLOSED (VP ruling 2026-10-24): a mailbox created as a fallback
        swallowed the message and reported success. Three real targets did that:
        w9:pH, w9:pM and nav-fixer2."""
        from dunk_core import DunkError as _DunkError
        with self.assertRaisesRegex(_DunkError, "reason=unknown_name"):
            self.flock.resolve_mailbox("not-started-yet", strict=True)
        with self.assertRaisesRegex(_DunkError, "reason=unknown_name"):
            self.flock.inbox_send("not-started-yet", "waiting for you", notify=True)
        self.assertEqual(0, self.inbox.count("not-started-yet")["total"],
                         "a refused send must not leave a mailbox behind")

    def test_capability_reads_an_observed_poll_not_a_harness_name(self):
        """An independent review found the first version wrong twice: a manual
        `owner take` conferred capability, and a poller draining EMPTY queues never
        wrote a row so a healthy lane looked dead. Only the poller's own heartbeat
        counts, empty polls included."""
        import dunk_owner
        made = self.flock.create_workspace(label="pi lane", cwd="/tmp",
                                          mailbox="dunky pi lane")["pane_id"]
        for pane in self.herdr.panes:
            if pane["pane_id"] == made:
                pane["agent"] = "pi"
        store = dunk_owner.OwnerPush()
        before = self.flock.delivery_capability("dunky pi lane")
        self.assertFalse(before["drains"], before["drain_evidence"])
        self.assertEqual("guarded", before["send"])
        store.poll(made, by="pi-extension")          # an EMPTY poll still counts
        after = self.flock.delivery_capability("dunky pi lane")
        self.assertTrue(after["drains"], after["drain_evidence"])
        self.assertEqual("owner_push", after["send"])
        # A manual take of nothing must not confer capability.
        import time as _t
        store.db.execute("DELETE FROM owner_poll"); store.db.commit()
        store.take(made, by="manual")
        self.assertFalse(self.flock.delivery_capability("dunky pi lane")["drains"])

    def test_delivery_capability_says_push_or_guarded_per_harness(self):
        """The desk routes on this: pi lanes drain a push queue, claude/codex lanes
        have no drain hook and must get an immediate guarded send."""
        pi = self.flock.create_workspace(label="pi lane", cwd="/tmp",
                                        mailbox="dunky")["pane_id"]
        claude = self.flock.create_workspace(label="claude lane", cwd="/tmp",
                                             mailbox="PM (par-a)")["pane_id"]
        for pane in self.herdr.panes:                 # the fake's agent field
            if pane["pane_id"] == pi:
                pane["agent"] = "pi"
            if pane["pane_id"] == claude:
                pane["agent"] = "claude"
        import dunk_owner
        dunk_owner.OwnerPush().poll(pi, by="pi-extension")   # the harness is polling
        a = self.flock.delivery_capability("dunky")
        self.assertTrue(a["drains"], a["drain_evidence"])
        self.assertEqual("owner_push", a["send"])
        self.assertEqual("dunky", a["mailbox"], "the declared name is what the desk shows")
        b = self.flock.delivery_capability("PM (par-a)")
        self.assertFalse(b["drains"])
        self.assertEqual("guarded", b["send"])
        with self.assertRaisesRegex(DunkError, "reason=dead_lane"):
            self.flock.delivery_capability("%99")

    def test_capability_says_whether_the_desk_credential_covers_this_pane(self):
        """A token existing is not the same as this pane being on its list: the
        credential names panes, so a view that only knows a token exists offers a
        button the daemon refuses (found reviewing the desk's can_type)."""
        # The desk credential became the `web` service token (dunk_roles): a web
        # token exists or it does not, and it covers every pane the owner may type into.
        self.flock.create_workspace(label="guarded", cwd="/tmp", mailbox="guarded lane")
        before = self.flock.delivery_capability("guarded lane")
        self.assertFalse(before["desk_credential"])
        self.assertFalse(before["desk_can_type"])
        from dunk_roles import Roles
        self.flock.roles = Roles(path=os.path.join(self.tmp.name, "roles.json"),
                                 tokens_path=os.path.join(self.tmp.name, "tokens.json"))
        self.flock.roles.issue_token("web", label="desk")
        after = self.flock.delivery_capability("guarded lane")
        self.assertTrue(after["desk_credential"])
        self.assertTrue(after["desk_can_type"])

    def test_two_live_panes_declaring_one_name_are_refused_not_split(self):
        """A restore that duplicates a lane must not split its mail silently."""
        a = self.flock.create_workspace(label="one", cwd="/tmp", mailbox="par-b")["pane_id"]
        b = self.flock.create_workspace(label="two", cwd="/tmp", mailbox="par-b")["pane_id"]
        from dunk_core import AmbiguousTarget as _Ambiguous
        with self.assertRaisesRegex(_Ambiguous, "DECLARED by more than one live pane"):
            self.flock.resolve_mailbox("par-b", strict=True)
        self.flock.lane_set(b, "par-b-2")
        self.assertEqual(a, self.flock.resolve_mailbox("par-b", strict=True)[2])

    def test_direct_delivery_addresses_a_pane_or_a_pinned_name_and_refuses_loudly(self):
        """The desk's call (owner push / keystrokes) asks the same question as the
        inbox: who is this, are they alive. It gets the same tokens."""
        from dunk_core import DunkError as _DunkError
        made = self.flock.create_workspace(label="poolrad-pm", cwd="/tmp",
                                          mailbox="PM (par-a)")
        pane_id = made["pane_id"]
        self.assertEqual(pane_id, self.flock.delivery_target(pane_id)["pane_id"])
        self.assertEqual(pane_id, self.flock.delivery_target("PM (par-a)")["pane_id"],
                         "the pinned name must be as good as the pane id")
        with self.assertRaisesRegex(_DunkError, "reason=dead_lane"):
            self.flock.delivery_target("%99")
        with self.assertRaisesRegex(_DunkError, "reason=unknown_name"):
            self.flock.delivery_target("nobody at all")

    def test_a_declared_lane_name_survives_the_agent_renaming_its_pane(self):
        """Measured 2026-10-24: `new-workspace --as 'PM (par-a)'` was replaced by
        claude's own conversation title minutes later and the lane became
        unaddressable by the name it had been given. A name the agent can rename
        away is not a registration."""
        made = self.flock.create_workspace(label="poolrad-pm", cwd="/tmp",
                                          mailbox="PM (par-a)")
        pane_id = made["pane_id"]
        for pane in self.herdr.panes:            # the agent renames itself
            if pane["pane_id"] == pane_id:
                pane["terminal_title"] = "✳ PR #1188 acceptance review"
                pane["terminal_title_stripped"] = "✳ PR #1188 acceptance review"
                pane["label"] = "✳ PR #1188 acceptance review"
        key, label, resolved = self.flock.resolve_mailbox("PM (par-a)")
        self.assertEqual(pane_id, resolved, "the declared name must still deliver")
        self.assertEqual("PM (par-a)", key)
        row = self.flock._lane_registry()[pane_id]
        self.assertTrue(self.flock._row_identity(row)[1], "the row stays declared")
        self.assertEqual("PM (par-a)", self.flock._row_identity(row)[0])

    def test_an_inferred_name_never_overwrites_a_declared_one(self):
        made = self.flock.create_workspace(label="pm", cwd="/tmp", mailbox="PM (par-a)")
        pane_id = made["pane_id"]
        self.assertFalse(self.flock._remember_lane(pane_id, "✳ some title"))
        self.assertEqual("PM (par-a)", self.flock._row_identity(
            self.flock._lane_registry()[pane_id])[0])

    def test_a_registered_lane_with_no_live_pane_refuses_a_send(self):
        """Three states, three tokens (VP ruling 2026-10-24): a name that was never
        a lane is unknown_name, a lane marked dead at a known time is dead_lane,
        and a lane that exists but is not running on this backend is
        unhoused_lane - the operator does something different for each."""
        from dunk_core import DunkError as _DunkError
        self.flock._remember_lane("w9:pZ", "arriving later")
        with self.assertRaisesRegex(_DunkError, "reason=unhoused_lane"):
            self.flock.inbox_send("arriving later", "waiting for you", notify=True)
        self.assertEqual(0, self.inbox.count("arriving later")["total"])

    def test_a_lane_the_registry_marks_dead_alarms_with_the_date(self):
        from dunk_core import DunkError as _DunkError
        self.flock._remember_lane("w9:pY", "gone lane", declared=True)
        data = self.flock._lane_registry()
        data["w9:pY"].update({"alive": False, "dead_since": "2026-10-24 08:00"})
        self.flock._write_lane_registry(data)
        with self.assertRaisesRegex(_DunkError, "reason=dead_lane"):
            self.flock.resolve_mailbox("gone lane", strict=True)
        with self.assertRaisesRegex(_DunkError, "dead since 2026-10-24 08:00"):
            self.flock.inbox_send("gone lane", "anyone there", notify=True)

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
        # The SEND contract (strict). A read of the same name is allowed: that is
        # where stranded mail lives.
        with self.assertRaisesRegex(DunkError, "no live pane"):
            self.flock.resolve_mailbox("w9:p8", strict=True)
        with self.assertRaisesRegex(DunkError, "no live pane"):
            self.flock.inbox_send("w9:p8", "lost")
        self.assertEqual(0, self.inbox.count("w9:p8")["total"],
                        "the failed send must not store a message")

    def test_archive_through_the_flock_drains_unread(self):
        # A stranded mailbox can still be drained: sends are refused now, reads are not.
        for n in range(2):
            self.inbox.send("dead-mailbox", f"m{n}", sender="pm")
        result = self.flock.inbox_archive("dead-mailbox")
        self.assertEqual(2, result["archived"])
        count = self.flock.inbox_count("dead-mailbox")
        self.assertEqual(0, count["unread"])
        self.assertEqual(2, count["archived"])

    def test_a_stranded_mailbox_can_still_be_read_even_though_sends_are_refused(self):
        """Reads stay permissive on purpose: draining stranded mail is the point."""
        self.inbox.send("stranded lane", "from the void", sender="pm")
        count = self.flock.inbox_count("stranded lane")
        self.assertEqual(1, count["unread"])
        self.assertEqual(["from the void"],
                         [m["body"] for m in self.flock.inbox_read("stranded lane")["messages"]])
        with self.assertRaisesRegex(DunkError, "reason=unknown_name"):
            self.flock.inbox_send("stranded lane", "no", notify=False)

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


class _RecordingMetrics:
    """Metrics stand-in that keeps every event, so a hold can be asserted ABSENT.

    `Flock` calls `.prune()` at construction and `.record(event, **fields)` on every
    interesting path; a real `Metrics` row per event would be a second database to
    read back in a fixture whose whole point is "nothing was held".
    """

    def __init__(self):
        self.events = []

    def record(self, event, **fields):
        self.events.append(dict(fields, event=event))

    def prune(self, *_args, **_kwargs):
        return 0

    def close(self):
        return None


class WorkingLaneAcceptsMailTests(unittest.TestCase):
    """t271: 'working' is INFORMATION, not a hold reason.

    A lane mid-turn queues the message typed at it - claude and pi both do, measured -
    so holding mail behind 'the target reports working' only added latency and put a
    wrong reason in the hold stream (2.58.0 removed it; `_last_block_reason` used to
    be set to "working"). These tests fail on that old rule: the send is held with
    reason 'working' and no nudge is ever typed.

    The hold that DOES remain is a different, measured fact - a harness that LOSES
    text typed during a turn (codex) - and it must say the harness, never the status.
    """

    WORKING_SCREEN = ("│ ✻ Working (55m) · esc to interrupt\n❯\u00a0\n"
                      "\x1b[2mAsk Pi to do anything\x1b[0m\n──────────────\n")
    # A REAL human draft in the same working lane. The dim hint above is a placeholder
    # and reads as an empty box; this is a sentence, and it must still hold.
    WORKING_WITH_DRAFT = "│ ✻ Working (55m) · esc to interrupt\n❯ half a sentence I am typing\n──────────────\n"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox, metrics=self.metrics)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def busy_pane(self, harness):
        """w2:p1 mid-turn on `harness`: a working banner AND a working status."""
        for pane in self.herdr.panes:
            if pane["pane_id"] == "w2:p1":
                pane["agent"] = harness
                pane["agent_status"] = "working"
        self.herdr.screens["w2:p1"] = self.WORKING_SCREEN
        self.herdr.statuses["w2:p1"] = "working"
        return self.herdr.get_pane("w2:p1")

    def held_events(self):
        return [event for event in self.metrics.events if event["event"] == "message.held"]

    def test_a_working_lane_receives_mail_instead_of_being_held(self):
        self.busy_pane("pi")
        sent = self.flock.inbox_send("w2:p1", "a nudge for a lane that is mid-turn",
                                     notify=True, self_pane_id="w1:p1")
        self.assertTrue(sent["notified"])
        self.assertTrue(self.herdr.sent.wait(5), "the nudge to a working lane was never typed")
        self.assertEqual([], self.held_events(),
                         "a working lane must not produce a message.held event")
        self.assertNotIn("working",
                         [event.get("reason") for event in self.held_events()],
                         "reason=working is the rule this task removed")
        self.assertEqual(1, self.flock.inbox_count("w2:p1")["unread"],
                         "the mail is stored whether or not the nudge is typed")

    def test_the_working_status_is_deliverable_and_not_a_gate(self):
        pane = self.busy_pane("pi")
        state = self.flock.delivery_state("w2:p1", "working", pane=pane)
        self.assertEqual("working", state["state"])
        self.assertTrue(state["deliverable"],
                        "working must carry deliverable=true; a gate would starve the lane")
        self.assertIsNone(self.flock.send_blocker("w2:p1"),
                          "pi queues text typed during a turn, so nothing may hold")
        state, reason = dunk_core.classify_delivery(self.WORKING_SCREEN, "working")
        self.assertEqual("working", state)
        self.assertNotIn(reason, ("blocked", "dead"),
                         "the classification is information about the pane, not a refusal")

    def test_codex_native_queue_removes_the_old_working_hold(self):
        self.busy_pane("codex")
        self.assertIsNone(self.flock.send_blocker("w2:p1"))

    def test_a_human_draft_in_a_working_lane_is_still_held(self):
        # t271 removes the STATUS from the hold reasons and nothing else: a person
        # mid-sentence in that box is a separate, still-valid hold. The draft guard
        # lives in the send path (not in send_blocker), so this drives a real send.
        self.busy_pane("pi")
        self.herdr.screens["w2:p1"] = self.WORKING_WITH_DRAFT
        self.flock.inbox_send("w2:p1", "held behind a person", notify=True,
                              self_pane_id="w1:p1")
        deadline = time.time() + 5
        while time.time() < deadline and not self.held_events():
            time.sleep(0.05)
        held = self.held_events()
        self.assertTrue(held, "a real draft must still hold the queue")
        self.assertNotEqual("working", held[0].get("reason"),
                            "the reason is the draft, not the status")
        self.assertFalse(self.herdr.sent.is_set(),
                         "nothing may be typed over a person's half-written sentence")


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
        env = {"HERDR_PANE_ID": "wR:p4", "DUNKINGSHEEP_TTY_IDENTITY": "0"}
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch("dunk_client._current_pane_id") as current:
            self.assertEqual("wR:p4", FlockClient().self_pane_id)
            current.assert_not_called()

    def test_a_stale_herdr_id_does_not_beat_the_tmux_pane_we_are_in(self):
        """Measured 2026-10-24: a tmux pane brought up for a herdr lane inherits
        that lane's HERDR_PANE_ID, so every tmux lane believed it was its dead
        herdr twin - this lane wrote and was credited as the vp lane."""
        from dunk_client import FlockClient
        env = {"HERDR_PANE_ID": "w9:pH", "TMUX_PANE": "%4",
               "DUNKINGSHEEP_TTY_IDENTITY": "0"}
        with mock.patch.dict(os.environ, env, clear=True), \
                mock.patch("dunk_client._current_pane_id") as current:
            self.assertEqual("%4", FlockClient().self_pane_id)
            current.assert_not_called()

    def test_an_explicit_self_pane_still_wins_over_the_environment(self):
        from dunk_client import FlockClient
        env = {"HERDR_PANE_ID": "w9:pH", "TMUX_PANE": "%4",
               "DUNKINGSHEEP_TTY_IDENTITY": "0"}
        with mock.patch.dict(os.environ, env, clear=True):
            self.assertEqual("%2", FlockClient(self_pane_id="%2").self_pane_id)

    def test_client_uses_tmux_pane_when_herdr_is_absent(self):
        from dunk_client import FlockClient
        with mock.patch.dict(os.environ, {"TMUX_PANE": "%3",
                                          "DUNKINGSHEEP_TTY_IDENTITY": "0"},
                             clear=True), \
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
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TTY_IDENTITY": "0"},
                             clear=True), \
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


class PaneRegistryRefusalTests(unittest.TestCase):
    """A pane the terminal backend has lost is not automatically a dead lane.

    Measured 2026-10-29 (t699): the role registry listed %108, %109 and %103 while
    `dunkingsheep panes` carried none of them, and a send then said 'dead_lane'
    for panes whose recorded pids were running. The refusal is raised BEFORE the
    message is stored, so a wrong 'dead' both lies and loses mail. The registry
    is the evidence about the PROCESS; the backend list is evidence about the
    TERMINAL.
    """

    def setUp(self):
        import dunk_roles
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.roles = dunk_roles.Roles(path=os.path.join(self.tmp.name, "roles.json"),
                                      default="anonymous")
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox, roles=self.roles)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def _lose_pane(self, pane_id):
        self.herdr.panes = [p for p in self.herdr.panes if p["pane_id"] != pane_id]

    def _register(self, pane_id, pid, start=None):
        self.roles.register(pane_id, "worker", pane_pid=pid, start=start, by="test")

    def test_a_live_registered_pane_the_backend_lost_is_not_called_dead(self):
        from dunk_callers import process_start
        self._lose_pane("w2:p1")
        self._register("w2:p1", os.getpid(), process_start(os.getpid()))
        with self.assertRaises(DunkError) as caught:
            self.flock.resolve_mailbox("w2:p1", strict=True)
        message = str(caught.exception)
        self.assertIn("reason=backend_missing_pane", message)
        self.assertNotIn("reason=dead_lane", message)
        self.assertIn("ALIVE", message)
        self.assertIn(str(os.getpid()), message)

    def test_the_same_refusal_reaches_a_send_and_stores_nothing(self):
        from dunk_callers import process_start
        self._lose_pane("w2:p1")
        self._register("w2:p1", os.getpid(), process_start(os.getpid()))
        with self.assertRaisesRegex(DunkError, "reason=backend_missing_pane"):
            self.flock.inbox_send("w2:p1", "must not vanish", notify=True,
                                  self_pane_id="w1:p1")
        self.assertEqual(0, self.inbox.count("w2:p1")["total"])
        self.assertEqual(0, self.inbox.count("w1:p1")["total"],
                         "nothing is filed under the sender either")

    def test_dead_lane_is_kept_for_a_registration_whose_process_is_gone(self):
        self._lose_pane("w2:p1")
        self._register("w2:p1", 999999, "12345")
        with self.assertRaises(DunkError) as caught:
            self.flock.resolve_mailbox("w2:p1", strict=True)
        message = str(caught.exception)
        self.assertIn("reason=dead_lane", message)
        self.assertNotIn("backend_missing_pane", message)
        self.assertIn("no longer running", message)

    def test_a_pane_in_no_registry_and_no_backend_is_still_dead_lane(self):
        with self.assertRaisesRegex(DunkError, "reason=dead_lane"):
            self.flock.resolve_mailbox("%999", strict=True)

    def test_a_registration_with_a_reused_pid_is_not_called_alive(self):
        # Same pid, different starttime: a reused %N (tmux restart) is a new process.
        self._lose_pane("w2:p1")
        self._register("w2:p1", os.getpid(), "1000")
        with self.assertRaisesRegex(DunkError, "reason=dead_lane"):
            self.flock.resolve_mailbox("w2:p1", strict=True)

    def test_a_decorated_address_gets_the_same_truthful_reason(self):
        from dunk_callers import process_start
        self._lose_pane("w2:p1")
        self._register("w2:p1", os.getpid(), process_start(os.getpid()))
        with self.assertRaises(DunkError) as caught:
            self.flock.resolve_mailbox("codex-lane (w2:p1)", strict=True)
        self.assertIn("reason=backend_missing_pane", str(caught.exception))

    def test_a_lost_pane_costs_one_backend_call_before_refusing(self):
        # The send path must fail fast: the id branch reads the plain pane list
        # and must not pay for tab/workspace labels it will not use (t699: a
        # three-call listing under load was a multi-second wait before a refusal).
        from dunk_callers import process_start
        self._lose_pane("w2:p1")
        self._register("w2:p1", os.getpid(), process_start(os.getpid()))
        calls = {"list": 0, "grouped": 0}
        listed, grouped = self.herdr.list_panes, self.herdr.list_panes_grouped

        def count_list():
            calls["list"] += 1
            return listed()

        def count_grouped():
            calls["grouped"] += 1
            return grouped()

        self.herdr.list_panes = count_list
        self.herdr.list_panes_grouped = count_grouped
        with self.assertRaisesRegex(DunkError, "backend_missing_pane"):
            self.flock.resolve_mailbox("w2:p1", strict=True)
        self.assertEqual(1, calls["list"])
        self.assertEqual(0, calls["grouped"], "labels are not needed to refuse")

    def test_a_live_pane_id_still_resolves_to_the_same_mailbox(self):
        # The fast path changed which listing it reads first; the ANSWER must not
        # change (the tab label is still the mailbox name for an unregistered pane).
        self.assertEqual(self.flock.resolve_mailbox("Codex Site", strict=True)[0],
                         self.flock.resolve_mailbox("w2:p1", strict=True)[0])


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
                              {"to": "Codex Site", "body": "hi",
                               "attach": "/no/such/file", "notify": False},
                              self_pane_id="w1:p2")


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
            self.flock.resolve_mailbox("codex-poolrad2 (wR:p9)", strict=True)
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
            self.flock.resolve_mailbox("wR:p9", strict=True)


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
            "lanes": "lane", "lane_set": "lane", "lane_register": "lane",
            "lane_unregister": "lane", "whoami": "whoami",
            "token_issue": "token", "token_revoke": "token",
            "gh_token": "gh-token", "review_sync": "review", "review_list": "review",
            "poolrad_sync": "poolrad", "poolrad_progress": "poolrad",
            "context_add": "context", "context_promote": "context",
            "context_supersede": "context", "context_retire": "context",
            "delivery_capability": "capability",
            "owner_poll": "poll", "allocation": "allocation",
            "owner_conversation": "conversation", "owner_reply": "reply",
            # t594: the transport half of the owner mailbox.
            "owner_ingest": "ingest", "owner_outbox": "outbox",
            "owner_delivered": "delivered",
            "split_pane": "split", "start_agent": "start-agent", "rename": "rename",
            "focus": "focus", "close": "close", "herdr": "herdr",
            "herdr_help": "herdr-help", "help": "guide", "shutdown": "shutdown",
            "get_message": "messages", "metrics": "metrics",
            "inbox_send": "send", "inbox_count": "count", "inbox_list": "list",
            "inbox_read": "read", "inbox_recover": "recover",
            "inbox_status": "status", "inbox_ack": "ack",
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
            "owner_push": "owner", "owner_take": "owner", "owner_stats": "owner",
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
