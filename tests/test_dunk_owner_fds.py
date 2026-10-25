"""The owner-push path must not leak file handles (VP i24037).

Measured on the live daemon: 60 open handles on owner-push.db with a soft limit of
1024, and "OSError: [Errno 24] Too many open files" in the journal twice. The cause
was structural - OwnerPush opens a sqlite connection in its constructor and every
handler built a fresh one per call - so the test is structural too: N calls through
the dispatch layer must not change this process's descriptor count.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402
import dunk_owner  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


def open_fds():
    return len(os.listdir("/proc/self/fd"))


class OwnerPushHandleTests(unittest.TestCase):
    def setUp(self):
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_fifty_pushes_and_takes_do_not_grow_the_descriptor_count(self):
        # Warm up first: the first call legitimately creates the shared connection.
        dunk_api.dispatch(self.flock, "owner_push",
                          {"target": "w2:p1", "body": "warm"}, self_pane_id="%4")
        dunk_api.dispatch(self.flock, "owner_take", {"pane_id": "w2:p1"}, self_pane_id="%4")
        before = open_fds()
        for n in range(25):
            dunk_api.dispatch(self.flock, "owner_push",
                              {"target": "w2:p1", "body": f"m{n}"}, self_pane_id="%4")
            dunk_api.dispatch(self.flock, "owner_take",
                              {"pane_id": "w2:p1"}, self_pane_id="%4")
            dunk_api.dispatch(self.flock, "owner_stats", {}, self_pane_id="%4")
        after = open_fds()
        self.assertEqual(before, after,
                         f"descriptor count moved {before} -> {after} over 75 calls")

    def test_the_store_is_a_singleton(self):
        self.assertIs(dunk_owner.shared(), dunk_owner.shared())

    def test_the_store_can_still_be_used_as_a_context_manager(self):
        with dunk_owner.OwnerPush() as store:
            self.assertIsNotNone(store)
        self.assertIs(dunk_owner.shared(), dunk_owner.shared(),
                      "closing a private store must not disturb the shared one")


if __name__ == "__main__":
    unittest.main()
