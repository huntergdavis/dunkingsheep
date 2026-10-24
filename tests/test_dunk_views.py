"""The Command Center views: what each store knows, and a reason when it does not.

Two lessons are encoded here, both measured today:
  replied is answer_id IS NOT NULL, not answered_at (all 456 owner rows carry
  answered_at; 70 have no answer_id), so answered_at is 'picked up';
  the application's delivery column does double duty (transport for owner rows, reply
  lifecycle for agent rows) and must be split, never passed through raw.
"""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_views  # noqa: E402


class AppMessageStateTests(unittest.TestCase):
    def state(self, **row):
        base = {"id": 1, "role": "owner", "channel": "vp", "created_at": "2026-10-24T00:00:00Z",
                "delivery": "acked", "delivery_detail": "Handled by agent",
                "answer_id": None, "answered_at": None, "answer": None}
        base.update(row)
        return dunk_views._app_state(base, 0)

    def test_replied_requires_answer_id_not_answered_at(self):
        # Picked up but not answered: this is the case that made every message look
        # replied when answered_at was used as the signal.
        state = self.state(answered_at="2026-10-24T00:10:00Z", answer_id=None)
        self.assertEqual("awaiting reply (no answer_id yet)", state["replied"]["reason"])
        self.assertEqual("2026-10-24T00:10:00Z", state["seen"]["at"],
                         "answered_at is 'picked up' and belongs under seen")

    def test_replied_carries_the_reply_row_and_its_time(self):
        state = self.state(answer_id=99, answered_at="2026-10-24T00:10:00Z",
                           answer={"at": "2026-10-24T00:20:00Z", "excerpt": "done"})
        self.assertEqual("2026-10-24T00:20:00Z", state["replied"]["at"])
        self.assertEqual(99, state["replied"]["reply_id"])
        self.assertEqual("done", state["replied"]["reply_excerpt"])

    def test_an_answer_id_with_a_missing_reply_row_says_so(self):
        state = self.state(answer_id=99, answer=None)
        self.assertIsNone(state["replied"]["at"])
        self.assertIn("reply row is missing", state["replied"]["reason"])

    def test_the_delivery_column_is_split_by_role(self):
        owner = self.state(role="owner", delivery="acked")
        self.assertEqual("acked", owner["delivered"]["transport"])
        self.assertIsNone(owner["delivered"]["reply_lifecycle"])
        agent = self.state(role="agent", delivery="posted")
        self.assertEqual("posted", agent["delivered"]["reply_lifecycle"])
        self.assertIsNone(agent["delivered"]["transport"])

    def test_an_unexpected_state_is_null_with_a_reason(self):
        state = self.state(role="owner", delivery="posted")
        self.assertIsNone(state["delivered"]["transport"])
        self.assertIn("unexpected transport state", state["delivered"]["reason"])


class GitMergesTests(unittest.TestCase):
    def test_a_non_repo_is_null_with_a_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = dunk_views.git_merges(tmp)
        self.assertEqual([], result["merges"])
        self.assertIsNone(result["as_of"])
        self.assertIn("not a git repository", result["reason"])


if __name__ == "__main__":
    unittest.main()
