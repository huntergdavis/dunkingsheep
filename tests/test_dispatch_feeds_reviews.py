"""t668: the dispatcher must feed the review queue, and say why when it does not.

Measured 2026-10-29 21:19 and 21:24 PDT (steward lane %107): 13 then 8 open review
tasks, six registered reviewer lanes idle at their prompts, and every tick printed
"idle lanes: 0 / dispatched this hour: 0" with "standby queue: 0 pending". The
state file said why: ready 0, working 6, derived_available 17 - the dispatcher saw
plenty of board work and classified all six idle reviewers as WORKING, because a
pi lane reports agent_status=working for its whole session and the tick's fast
path treated that as a hard exclusion. delivery_state's own rule ("working is
INFORMATION now, not a gate") was never consulted.

The rule these tests pin: a lane holding no live lease is classified by
delivery_state; the backend's status is information, not a gate. And a tick that
does not dispatch must record a deterministic reason.
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402
import dunk_dispatch  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

REVIEWER = "w2:p1"
OWNER_PANE = "w1:p1"


class DispatcherFeedsReviewsTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        # The live shape (dunkingsheep panes, 2026-10-29): every reviewer lane reports
        # STATUS working while sitting at its prompt.
        for pane in self.herdr.panes:
            pane["agent_status"] = "working" if pane["pane_id"] == REVIEWER else None
        self.state_dir = tempfile.TemporaryDirectory()
        from dunk_inbox import Inbox
        from dunk_roles import Roles
        from dunk_tasks import Tasks
        self.flock = Flock(
            herdr=self.herdr, persist=False, restore=False,
            state_file=os.path.join(self.state_dir.name, "dunks.json"),
            inbox=Inbox(path=os.path.join(self.state_dir.name, "inbox.db"),
                        blob_dir=os.path.join(self.state_dir.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.state_dir.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.state_dir.name, "roles.json"),
                        tokens_path=os.path.join(self.state_dir.name, "tokens.json")))
        self.tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        self.tmp.close()
        os.unlink(self.tmp.name)
        self._saved = (dunk_api.DISPATCH_LANES_PATH,
                       dunk_api.STATE_PATH if hasattr(dunk_api, "STATE_PATH") else None,
                       os.environ.pop("DUNKINGSHEEP_DISPATCH_LANES", None))
        dunk_api.DISPATCH_LANES_PATH = self.tmp.name
        with open(self.tmp.name, "w") as handle:
            json.dump({"lanes": [REVIEWER]}, handle)
        # The dispatcher's state file carries a 6-hour "recently dispatched" history.
        # Hermetic tests must not inherit it from a previous test (or from a live run).
        self._saved_state_fns = (dunk_dispatch._load_state, dunk_dispatch._write_state)
        dunk_dispatch._load_state = lambda *a, **k: {}
        dunk_dispatch._write_state = lambda *a, **k: None
        # `dispatch tick` is an owner/system verb; this test runs as an owner pane.
        self.flock.roles.register(OWNER_PANE, role="owner", by=OWNER_PANE)
        # The pane is a pi lane at its prompt: the backend says "working" forever.
        self.herdr.statuses[REVIEWER] = "working"
        # A delivery_state that reports this pane READY is what the tick must reach;
        # the screen fake above is what makes the real classifier say so, but the
        # status is the part under test, so it is asserted directly too.
        self.flock.delivery_state = lambda pane_id, status=None, pane=None, screen=None: (
            {"state": "ready", "reason": None})

    def tearDown(self):
        dunk_api.DISPATCH_LANES_PATH, _state, env = self._saved
        if env is not None:
            os.environ["DUNKINGSHEEP_DISPATCH_LANES"] = env
        try:
            os.unlink(self.tmp.name)
        except FileNotFoundError:
            pass
        dunk_dispatch._load_state, dunk_dispatch._write_state = self._saved_state_fns
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.state_dir.cleanup()

    def _tick(self, dry_run=True):
        self.flock.clear_caller()
        try:
            return dunk_api.dispatch(self.flock, "dispatch",
                                     {"action": "tick", "dry_run": dry_run},
                                     self_pane_id=OWNER_PANE)
        finally:
            self.flock.clear_caller()

    def _file_review(self):
        return self.flock.tasks.add("global", "Review o/r PR #7 at abc1234",
                                    kind="review", repo="o/r", pr=7,
                                    head="a" * 40, created_by="review-sync")

    def test_a_working_pi_lane_with_no_lease_is_fed_an_open_review(self):
        review = self._file_review()
        state = self._tick(dry_run=False)
        self.assertEqual(1, state["ready"], "a lane with no lease is not 'working'")
        self.assertEqual(0, state["working"])
        self.assertEqual(1, state["dispatched_count"])
        self.assertTrue(state["dispatched"][0]["sent"])
        self.assertTrue(state["dispatched"][0].get("message_id"),
                        "the send went through the guarded inbox path")
        self.assertEqual(review["id"], state["dispatched"][0]["task_id"])
        self.assertIn(review["id"], state["dispatched"][0]["text"])

    def test_a_lane_holding_a_lease_is_not_fed(self):
        review = self._file_review()
        self.flock.tasks.add("global", "something else")
        self.flock.tasks.claim_next(scope="global", pane_id=REVIEWER)
        state = self._tick()
        self.assertEqual([], state["dispatched"])
        self.assertEqual(0, state["ready"], "a lane holding a lease is working on something")

    def test_an_undispatched_tick_records_a_deterministic_reason(self):
        self._file_review()
        self.flock.delivery_state = lambda pane_id, status=None, pane=None, screen=None: (
            {"state": "blocked", "reason": "draft in the composer"})
        state = self._tick()
        self.assertEqual([], state["dispatched"])
        self.assertEqual(1, len(state["blocked"]))
        self.assertEqual("draft in the composer", state["blocked"][0]["reason"],
                         "a tick that does not dispatch says why, per lane")
        self.assertEqual(1, state["derived_available"])

    def test_the_dispatcher_picks_review_tasks_off_the_board(self):
        """The intended rule, stated as a test: review tasks are BOARD work.

        There is no separate enqueue for them - derive_board_items walks the open
        tasks, and a review task is one of them, so the dispatcher hands it out
        like any other. If that ever stops being true, this fails.
        """
        import dunk_dispatch
        review = self._file_review()
        items, _coarse = dunk_dispatch.derive_board_items(
            self.flock.tasks.list(include_done=False, limit=50))
        self.assertIn(review["id"], [item["task_id"] for item in items])


if __name__ == "__main__":
    unittest.main()
