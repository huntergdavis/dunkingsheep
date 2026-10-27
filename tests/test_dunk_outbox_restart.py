"""t249: held mail survives a restart, and a DELIVERED message does not come back.

Measured 2026-10-26 on 2.70.46: _enqueue() saved the outbox when a message became held, but
_deliver() never saved when it stopped being held. So dunks.json kept the last snapshot, and a
restart re-queued a message that had already been sent - typed a second time under its old id,
which is exactly what _restore_outbox's docstring forbids.

The first version of this file pinned the wrong things (reviewer, PR #43): it simulated delivery and
called _save() explicitly, so deleting the production _save() left it green; and it built the
"already decided" set in the SAME process, where message_history is populated, instead of across a
restart, where it is empty. These tests write the state file the way a previous process would have
left it and construct a SECOND Flock from it - the real restart shape.
"""

import json
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class OutboxRestartTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="outbox-")
        self.state = os.path.join(self.tmp, "dunks.json")

    def _flock(self):
        """A daemon on this state file. A SECOND one is a restart."""
        return Flock(herdr=FakeHerdr(), state_file=self.state, restore=True)

    def _write_state(self, outbox, decided=None):
        payload = {"version": 1, "dunkers": [], "next_id": 1, "next_message_id": 99,
                   "outbox": outbox, "decided": decided or {}}
        with open(self.state, "w") as handle:
            json.dump(payload, handle)

    @staticmethod
    def _row(message_id, text="held notice", pane="w1:p2", notify=(), created_at=None):
        return {"id": message_id, "pane_id": pane, "target_label": "Claude Proj", "text": text,
                "source": "test", "kind": "text", "hold_while_typing": True,
                "is_notice": False, "notify_for": list(notify),
                "created_at": time.time() if created_at is None else created_at}

    def _held_ids(self, flock):
        return [m.id for queue in flock.outbox.values() for m in queue
                if m.status == "queued"]

    # -- the restart shape: a file left by a previous process -----------------------------

    def test_a_restart_keeps_a_message_that_never_finished(self):
        self._write_state([self._row("m1", "still waiting")])
        flock = self._flock()
        try:
            self.assertEqual(["m1"], self._held_ids(flock))
            queued = [m for q in flock.outbox.values() for m in q if m.id == "m1"][0]
            self.assertEqual("still waiting", queued.text, "the TEXT is what is persisted")
            self.assertTrue(queued.hold_while_typing)
        finally:
            flock.close()

    def test_a_restart_does_not_resurrect_a_message_the_file_says_was_sent(self):
        """The persisted verdict is the only thing a fresh process has; it must be used."""
        stamp = 1793000000.0
        self._write_state([self._row("m1", created_at=stamp)],
                          decided={"m1": {"status": "sent", "created_at": stamp}})
        flock = self._flock()
        try:
            self.assertEqual([], self._held_ids(flock),
                             "a delivered message must not be queued again by a restart")
        finally:
            flock.close()

    def test_a_reused_id_does_not_refuse_a_new_message(self):
        """Reviewer, PR #43: ids are reused across daemons (m6379 went to wN:p1 on 2026-10-15 and to
        %4 on 2026-10-26). A verdict is a claim about a MESSAGE, so it needs the message's identity;
        refusing on the id alone would drop a brand-new message while logging a clean restore."""
        old_stamp, new_stamp = 1792000000.0, 1793000000.0
        self._write_state([self._row("m1", "a brand new message", created_at=new_stamp)],
                          decided={"m1": {"status": "sent", "created_at": old_stamp}})
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            flock = self._flock()
            try:
                self.assertEqual(["m1"], self._held_ids(flock),
                                 "a new message that happens to reuse an id must be restored")
            finally:
                flock.close()
        self.assertIn("was reused", "\n".join(captured.output))

    def test_a_verdict_without_a_timestamp_does_not_drop_the_message(self):
        self._write_state([self._row("m1")], decided={"m1": "sent"})
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            flock = self._flock()
            try:
                self.assertEqual(["m1"], self._held_ids(flock))
            finally:
                flock.close()
        self.assertIn("no timestamp", "\n".join(captured.output))

    def test_the_skip_is_logged_as_a_verdict_not_as_a_failure(self):
        """Reviewer, PR #43: the skip used to raise StopIteration into the per-row handler, so the
        log said "could not restore a held message:" with an empty reason - a correct refusal
        reported as a failure."""
        stamp = 1793000000.0
        self._write_state([self._row("m1", created_at=stamp)],
                          decided={"m1": {"status": "sent", "created_at": stamp}})
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            flock = self._flock()
            try:
                self.assertEqual([], self._held_ids(flock))
            finally:
                flock.close()
        text = "\n".join(captured.output)
        self.assertIn("not restoring message m1: it already sent", text)
        self.assertNotIn("could not restore", text)

    def test_a_failed_notice_whose_mail_is_still_unread_does_not_drop_the_row(self):
        """Reviewer, PR #43: "failed" in the delivery loop means "this attempt failed, retrying", not
        "delivered". Dropping a failed notice whose mail nobody has read removes the only nudge."""
        flock = self._flock()
        try:
            sent = flock.inbox.send("Claude Proj", "nobody has read this", sender="%4")
            inbox_id = sent["message_id"]
            flock.inbox.record_notify(inbox_id, "failed")
        finally:
            flock.close()
        self._write_state([self._row("m1", notify=[inbox_id])])
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            second = self._flock()
            try:
                self.assertEqual(["m1"], self._held_ids(second),
                                 "an unread failed notice must be restored, not dropped")
            finally:
                second.close()
        self.assertIn("STILL UNREAD", "\n".join(captured.output))

    def test_a_failed_notice_whose_mail_is_read_decides_the_row(self):
        """The other half: the retry loop is obsolete once the mail it announced is no longer
        waiting, so the row is dropped WITH the reason logged."""
        flock = self._flock()
        try:
            sent = flock.inbox.send("Claude Proj", "already read", sender="%4")
            inbox_id = sent["message_id"]
            flock.inbox.record_notify(inbox_id, "failed")
            flock.inbox.mark_read([inbox_id])
        finally:
            flock.close()
        self._write_state([self._row("m1", notify=[inbox_id])])
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            second = self._flock()
            try:
                self.assertEqual([], self._held_ids(second))
            finally:
                second.close()
        text = "\n".join(captured.output)
        self.assertIn("is decided", text)
        self.assertIn("no longer waiting", text)

    def test_a_cancelled_inbox_verdict_also_decides_the_row(self):
        """Reviewer, PR #43: cancelled is a real status; a cancelled notice must not be typed."""
        flock = self._flock()
        try:
            sent = flock.inbox.send("Claude Proj", "cancelled in the inbox", sender="%4")
            inbox_id = sent["message_id"]
            flock.inbox.record_notify(inbox_id, "cancelled")
        finally:
            flock.close()
        self._write_state([self._row("m1", notify=[inbox_id])])
        second = self._flock()
        try:
            self.assertEqual([], self._held_ids(second))
        finally:
            second.close()

    def test_a_restart_does_not_resurrect_a_message_the_inbox_says_was_sent(self):
        """Second source of the verdict: the inbox record, which the notice path writes."""
        flock = self._flock()
        try:
            sent = flock.inbox.send("Claude Proj", "already delivered", sender="%4")
            inbox_id = sent["message_id"]
            flock.inbox.record_notify(inbox_id, "sent")
        finally:
            flock.close()
        self._write_state([self._row("m1", notify=[inbox_id])])
        second = self._flock()
        try:
            self.assertEqual([], self._held_ids(second))
        finally:
            second.close()

    # -- the fix itself: the production delivery path persists the departure ---------------

    def test_a_cancellation_leaves_the_file(self):
        """t255, third instance: cancel_message() mutated state and never saved, so a cancelled row
        stayed in dunks.json and a restart restored it. Measured 2026-10-27 on the live host:
        `cancel-message m862` reported "cancelled" while the file still held m862."""
        flock = self._flock()
        try:
            message = flock._new_message("w1:p2", "Claude Proj", "cancel me",
                                         source="test", deliver=lambda: None)
            flock._enqueue(message)
            with open(self.state) as handle:
                self.assertIn(message.id, [row["id"] for row in json.load(handle)["outbox"]])
            flock.cancel_message(message.id)
            with open(self.state) as handle:
                self.assertNotIn(message.id, [row["id"] for row in json.load(handle)["outbox"]],
                                 "a cancelled message must leave the file a restart reads")
        finally:
            flock.close()

    def test_delivery_through_the_production_path_removes_it_from_the_file(self):
        flock = self._flock()
        try:
            message = flock._new_message("w1:p2", "Claude Proj", "sent for real",
                                         source="test", deliver=lambda: None)
            flock._enqueue(message)
            with open(self.state) as handle:
                self.assertIn(message.id, [row["id"] for row in json.load(handle)["outbox"]])
            flock.send_guarded = lambda pane_id, deliver, *a, **k: (True, True, None)
            flock._deliver(message)                     # production path, no explicit _save()
            with open(self.state) as handle:
                payload = json.load(handle)
            self.assertNotIn(message.id, [row["id"] for row in payload["outbox"]],
                             "_deliver() must persist the message LEAVING the queue")
            self.assertEqual("sent", payload["decided"][message.id]["status"],
                             "the verdict has to travel, or the next process cannot use it")
            self.assertIsNotNone(payload["decided"][message.id]["created_at"],
                                 "and it must carry the message's identity, not just its id")
        finally:
            flock.close()


if __name__ == "__main__":
    unittest.main()
