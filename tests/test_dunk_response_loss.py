"""t128: a response lost AFTER the mark must not silently consume the mail.

t93 closed the pre-mark window: a read that fails before the mark leaves the
batch queued. The post-mark window remains: the daemon commits mark_read, then
the socket response is lost (client crash, socket drop) and the batch was
displayed nowhere. The next read returns nothing, so the mail is silently gone
- the reader never saw it and the store says it was read.

The correction is ack-on-display with a bounded recovery buffer. A non-peek
read records a delivery receipt (the latest batch per mailbox, exactly one
row); `inbox_recover` replays that batch for display; `inbox_ack` consumes it
and clears the receipt. Ack stays idempotent, acked mail never resurfaces, and
the buffer holds ONE batch: a newer read overwrites it, which the bound test
below pins as an explicit limit.

The transport double is faithful where it matters: results cross it as
newline-delimited JSON, exactly like dunk_server, and the drop happens AFTER
the daemon committed - the loss the task is about.
"""

import json
import os
import sys
import unittest
import uuid

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import DunkError, Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class SocketDrop(RuntimeError):
    """The daemon answered, but the answer never reached the client."""


def deliver(flock, box, pane=None, limit=10, drop=False):
    """One inbox_read across a JSON socket, optionally lost on the way back."""
    if pane is not None:
        result = flock.inbox_read(box, limit=limit, self_pane_id=pane)
    else:
        result = flock.inbox_read(box, limit=limit)
    # The socket carries newline-delimited JSON: round-trip first so the drop
    # under test is post-serialization, post-commit, like a real socket drop.
    result = json.loads(json.dumps(result))
    if drop:
        raise SocketDrop("response lost after the mark")
    return result


class ResponseLossTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        # One mailbox per test: the store is per-process, so a shared name would
        # accumulate rows across tests and make the counts assert the wrong thing.
        self.box = f"lane-{uuid.uuid4().hex[:8]}"
        self.pane = f"w9:p{abs(hash(self.box)) % 90 + 10}"
        self.flock._remember_lane(self.pane, self.box)

    def tearDown(self):
        self.flock.close()

    def send(self, n, box=None):
        box = box or self.box
        return [self.flock.inbox.send(box, f"message {n}", sender="pm")["message_id"]
                for n in range(n)]

    def test_a_response_lost_after_the_mark_is_recoverable(self):
        sent = self.send(3)
        with self.assertRaises(SocketDrop):
            deliver(self.flock, self.box, self.pane, drop=True)
        # The mark committed: a plain re-read offers nothing.
        retry = deliver(self.flock, self.box, self.pane)
        self.assertEqual([], retry["messages"],
                         "the mark committed, so the batch is not queued")
        # But it is not lost: the receipt replays exactly the dropped batch.
        recovered = self.flock.inbox_recover(self.box, self_pane_id=self.pane)
        self.assertEqual(sent, [m["message_id"] for m in recovered["messages"]])

    def test_recover_then_ack_consumes_exactly_once(self):
        self.send(3)
        with self.assertRaises(SocketDrop):
            deliver(self.flock, self.box, drop=True)
        recovered = self.flock.inbox_recover(self.box)
        ids = [m["message_id"] for m in recovered["messages"]]
        first = self.flock.inbox_ack(ids, mailbox=self.box)
        self.assertEqual(3, len(first["changed"]))
        second = self.flock.inbox_ack(ids, mailbox=self.box)
        self.assertEqual([], second["changed"],
                         "a re-acked batch (lost ack response) must be a no-op")
        self.assertEqual(0, self.flock.inbox_count(self.box)["unread"])
        cleared = self.flock.inbox_recover(self.box)
        self.assertEqual([], cleared["messages"],
                         "a fully acked receipt is cleared, not replayed")

    def test_the_receipt_holds_only_the_latest_batch(self):
        first = self.send(2)
        with self.assertRaises(SocketDrop):
            deliver(self.flock, self.box, drop=True)
        second = self.send(1)
        delivered = deliver(self.flock, self.box)
        self.assertEqual(second,
                         [m["message_id"] for m in delivered["messages"]])
        recovered = self.flock.inbox_recover(self.box)
        self.assertEqual(
            [m["message_id"] for m in delivered["messages"]],
            [m["message_id"] for m in recovered["messages"]])
        # Explicit bound: the first dropped batch is orphaned by the newer
        # read. It stays marked read (today's behaviour, no worse), but the
        # buffer holds one batch, never a growing history.

    def test_a_partial_ack_shrinks_the_receipt(self):
        self.send(3)
        with self.assertRaises(SocketDrop):
            deliver(self.flock, self.box, drop=True)
        recovered = self.flock.inbox_recover(self.box)
        ids = [m["message_id"] for m in recovered["messages"]]
        self.flock.inbox_ack(ids[:1], mailbox=self.box)
        again = self.flock.inbox_recover(self.box)
        self.assertEqual(ids[1:], [m["message_id"] for m in again["messages"]])

    def test_recover_without_a_receipt_answers_empty(self):
        self.send(1)
        # Like an empty read: no receipt is the normal case, not an error.
        recovered = self.flock.inbox_recover(self.box)
        self.assertEqual([], recovered["messages"])

    def test_recover_is_isolated_per_mailbox(self):
        other = f"lane-{uuid.uuid4().hex[:8]}"
        self.send(1)
        with self.assertRaises(SocketDrop):
            deliver(self.flock, self.box, drop=True)
        foreign = self.flock.inbox_recover(other)
        self.assertEqual([], foreign["messages"],
                         "one mailbox's receipt must not replay into another")

    def test_the_happy_path_still_marks(self):
        self.send(3)
        result = deliver(self.flock, self.box, self.pane)
        self.assertEqual(3, len(result["messages"]))
        self.assertEqual(3, result.get("marked_read"))
        self.assertEqual(0, self.flock.inbox_count(self.box)["unread"])


if __name__ == "__main__":
    unittest.main()
