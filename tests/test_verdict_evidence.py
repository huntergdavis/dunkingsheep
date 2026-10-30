"""A verdict needs evidence, and the evidence travels with it (t621).

Two needs-fix verdicts were filed on 2026-10-29 with no note anywhere: the
GitHub review said "see the task notes", the follow-up fix tasks said the same,
and there were none — so the work they blocked could not be started. The brief in
docs/LANES.md already said evidence first; this makes the daemon enforce it.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
import dunk_github  # noqa: E402
import dunk_review  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER, REVIEWER, WORKER = "w9:p9", "w2:p1", "w2:p3"
EVIDENCE = "Ran tests/test_dunk_tasks.py (103 tests OK); PR head 4f1c8afe. NOT verified: no browser render."


class FakeTokens:
    def get(self, kind, now=None):
        return {"token": f"tok-{kind}", "kind": kind, "expires_at": 9e9, "cached": False}


class FakeGitHub:
    REVIEW_CONTEXT = dunk_github.REVIEW_CONTEXT
    GitHubError = dunk_github.GitHubError

    def __init__(self):
        self.statuses = []
        self.reviews = []
        self.merged = []

    def post_status(self, repo, sha, state, token, description, target_url=None):
        self.statuses.append((repo, sha, state))

    def review_pull(self, repo, number, event, body, token):
        self.reviews.append((number, event, body))

    def merge_pull(self, repo, number, token, sha=None):
        self.merged.append((repo, number, sha))


class VerdictEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.roles = Roles(path=os.path.join(self.tmp.name, "roles.json"),
                           tokens_path=os.path.join(self.tmp.name, "tokens.json"))
        self.flock = Flock(
            herdr=FakeHerdr(), state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False, restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=self.roles)
        self.roles.register(OWNER, "owner")
        self.roles.register(REVIEWER, "reviewer", provider="claude", label="pi-review-1")
        self.roles.register(WORKER, "worker", provider="pi", label="pi-worker-3")
        self.github = FakeGitHub()
        self.flock._github_token_cache = FakeTokens()
        self.sync = dunk_review.Sync(self.flock, tokens=FakeTokens(), github=self.github)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(
            t, v, by=by)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None):
        self.flock.clear_caller()
        try:
            return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)
        finally:
            self.flock.clear_caller()

    def review(self, author=WORKER, number=119):
        return self.flock.tasks.add(
            "global", f"Review PR #{number}", priority="high", kind="review",
            author=author, repo="huntergdavis/dunkingsheep", pr=number,
            head="a" * 40, created_by="review-sync")

    def claimed_review(self, **kwargs):
        task = self.review(**kwargs)
        self.call("task_next", {}, pane=REVIEWER)
        return task

    def test_a_verdict_without_a_note_is_refused(self):
        task = self.claimed_review()
        with self.assertRaisesRegex(DunkError, "a verdict needs evidence"):
            self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                      pane=REVIEWER)
        self.assertEqual("in_progress", self.flock.tasks.get(task["id"])["status"])
        self.assertIsNone(self.flock.tasks.get(task["id"])["verdict"])

    def test_a_note_by_another_lane_does_not_count(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": "someone else's words"},
                  pane=WORKER)
        with self.assertRaisesRegex(DunkError, "a verdict needs evidence"):
            self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                      pane=REVIEWER)

    def test_a_note_written_before_the_claim_does_not_count(self):
        task = self.review()
        self.call("task_note", {"id": task["id"], "reason": "a plan, not a finding"},
                  pane=REVIEWER)
        self.call("task_next", {}, pane=REVIEWER)
        with self.assertRaisesRegex(DunkError, "a verdict needs evidence"):
            self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                      pane=REVIEWER)

    def test_an_evidence_note_lets_the_verdict_through(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        done = self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                         pane=REVIEWER)
        self.assertEqual("accept", done["verdict"])
        self.assertEqual("done", done["status"])

    def test_the_needs_fix_followup_carries_the_evidence_verbatim(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        done = self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                         pane=REVIEWER)
        followup = self.flock.tasks.get(done["github"]["followup"])
        self.assertIn(EVIDENCE, followup["body"],
                      "the fix task says 'see the notes' again")
        self.assertIn("NOT verified", followup["body"])
        # t723: the fix task is the only record that a lane worked on this head, so it
        # must carry the PR identity; without it the next review of the same PR cannot
        # attribute the head and no-self-review goes unenforced.
        self.assertEqual("huntergdavis/dunkingsheep", followup["repo"])
        self.assertEqual(119, followup["pr"])
        self.assertTrue(followup["head"], "the fix task names the head it fixes")

    def test_the_github_review_body_carries_the_evidence_verbatim(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                  pane=REVIEWER)
        self.assertEqual(1, len(self.github.reviews))
        _number, event, body = self.github.reviews[0]
        self.assertEqual("REQUEST_CHANGES", event)
        self.assertIn(EVIDENCE, body)
        self.assertNotIn("see the task notes", body)

    def test_needs_fix_posts_the_failure_status(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        done = self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                         pane=REVIEWER)
        self.assertEqual("failure", done["github"]["status"])
        self.assertIn(("huntergdavis/dunkingsheep", "a" * 40, "failure"),
                      self.github.statuses,
                      "the failure status stayed pending on the live PRs")

    def test_a_cancelled_review_takes_no_verdict(self):
        """t656: a sweep may cancel a duplicate because the head was already judged."""
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        self.flock.tasks.cancel(task["id"], reason="duplicate of judged review t1")
        with self.assertRaisesRegex(DunkError, "is cancelled"):
            self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                      pane=REVIEWER)
        self.assertEqual("cancelled", self.flock.tasks.get(task["id"])["status"])

    def test_a_done_review_refuses_a_second_verdict(self):
        task = self.claimed_review()
        self.call("task_note", {"id": task["id"], "reason": EVIDENCE}, pane=REVIEWER)
        self.call("task_complete", {"id": task["id"], "verdict": "accept"}, pane=REVIEWER)
        with self.assertRaisesRegex(DunkError, "already done with verdict 'accept'"):
            self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                      pane=REVIEWER)
        self.assertEqual("accept", self.flock.tasks.get(task["id"])["verdict"])

    def test_needs_fix_does_not_overwrite_a_standing_accept_status(self):
        """t656: the failure status must not flip a head another review accepted."""
        first = self.claimed_review(number=120)
        self.call("task_note", {"id": first["id"], "reason": EVIDENCE}, pane=REVIEWER)
        self.call("task_complete", {"id": first["id"], "verdict": "accept"}, pane=REVIEWER)
        self.assertIn(("huntergdavis/dunkingsheep", "a" * 40, "success"), self.github.statuses)
        # A second review for the SAME head that reaches needs-fix.
        duplicate = self.flock.tasks.add(
            "global", "Review PR #120 again", priority="high", kind="review",
            repo="huntergdavis/dunkingsheep", pr=120, head="a" * 40,
            created_by="old-timer")
        self.call("task_next", {}, pane=REVIEWER)
        self.call("task_note", {"id": duplicate["id"], "reason": EVIDENCE}, pane=REVIEWER)
        done = self.call("task_complete", {"id": duplicate["id"], "verdict": "needs-fix"},
                         pane=REVIEWER)
        self.assertEqual("skipped_standing_accept", done["github"]["status"])
        self.assertEqual(first["id"], done["github"]["standing_accept"])
        self.assertNotIn(("huntergdavis/dunkingsheep", "a" * 40, "failure"),
                         self.github.statuses,
                         "a failure status overwrote the standing success")

    def test_the_owner_may_complete_a_review_without_a_note(self):
        """The gate is on the lane that judged the work, not on the owner."""
        task = self.review()
        done = self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                         pane=OWNER)
        self.assertEqual("accept", done["verdict"])


if __name__ == "__main__":
    unittest.main()
