"""t1053: the sweep's unread gate (dunk_api._gate_unread)."""

import os
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402


class Flock:
    def __init__(self, turns=None):
        self.turns = turns or {}


class GateUnreadTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def gate(self, flock=None, expired=None):
        return dunk_api._gate_unread(flock or Flock(), self.inbox, "%1",
                                     [] if expired is None else expired)

    def test_a_lane_with_only_daemon_announcements_is_fed(self):
        for n in range(5):
            self.inbox.send("%1", "x", subject=f"TASK t{n}", sender=None)
        self.assertEqual(0, self.gate())

    def test_fresh_lane_mail_skips_once_then_expires_and_feeds(self):
        self.inbox.send("%1", "hi", sender="%5")
        self.assertEqual(1, self.gate())
        expired = []
        with mock.patch("dunk_inbox._now", return_value=time.time() + 601):
            self.assertEqual(0, self.gate(expired=expired))
        self.assertEqual(["%1"], expired)

    def test_mail_older_than_the_last_turn_end_does_not_gate(self):
        self.inbox.send("%1", "hi", sender="%5")
        flock = Flock({"%1": {"ended_at": time.time() + 5}})
        self.assertEqual(0, self.gate(flock))


if __name__ == "__main__":
    unittest.main()
