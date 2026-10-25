"""t93: a read that fails must not consume the mail.

VP, live: `inbox read` crashed ('internal error: tuple index out of range') AFTER the
mark-read was committed, so the batch was consumed and gone on the next read.
pi-review-1 issued a verdict on a head whose withdrawal message it never saw.

The order is now: fetch (mark=False) -> build the WHOLE result, including the caller's
task queues (which is where the crash actually happened) -> mark those ids read. A
failure before the mark leaves the batch unread, which is the safe direction: the
reader sees the same mail again instead of losing it.
"""

import os
import sys
import unittest
import uuid
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class ReadOrderTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        # One mailbox per test: the store is per-process, so a shared name would
        # accumulate rows across tests and make the counts assert the wrong thing.
        self.box = f"lane-{uuid.uuid4().hex[:8]}"
        self.pane = f"w9:p{abs(hash(self.box)) % 90 + 10}"
        for n in range(3):
            self.flock.inbox.send(self.box, f"message {n}", sender="pm")
        self.flock._remember_lane(self.pane, self.box)

    def tearDown(self):
        self.flock.close()

    def test_a_render_failure_leaves_the_batch_unread(self):
        with mock.patch.object(type(self.flock.tasks), "for_pane",
                               side_effect=RuntimeError("row factory drift")):
            with self.assertRaises(RuntimeError):
                self.flock.inbox_read(self.box, limit=3, self_pane_id=self.pane)
        self.assertEqual(3, self.flock.inbox_count(self.box)["unread"],
                         "a read that failed consumed the mail")

    def test_a_successful_read_marks_and_says_how_many(self):
        result = self.flock.inbox_read(self.box, limit=3, self_pane_id=self.pane)
        self.assertEqual(3, len(result["messages"]))
        self.assertEqual(3, result.get("marked_read"))
        self.assertEqual(0, self.flock.inbox_count(self.box)["unread"])

    def test_peek_marks_nothing(self):
        result = self.flock.inbox_read(self.box, limit=3, peek=True)
        self.assertEqual(3, len(result["messages"]))
        self.assertNotIn("marked_read", result)
        self.assertEqual(3, self.flock.inbox_count(self.box)["unread"])

    def test_marking_is_idempotent(self):
        result = self.flock.inbox_read(self.box, limit=3, self_pane_id=self.pane)
        ids = [message["message_id"] for message in result["messages"]]
        self.assertEqual(0, self.flock.inbox.mark_read(ids),
                         "a second mark must move nothing")


if __name__ == "__main__":
    unittest.main()
