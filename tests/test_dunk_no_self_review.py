"""t723: no-self-review must not depend on a field the timer cannot fill.

Measured 2026-10-30 by ds-reviewer-1: review task t720 was filed for dunkingsheep#124
@ ec21292 - the head that lane had pushed as the author App minutes earlier - with
`author` NULL, because the commit carried the App identity and no lane label. The
guard was `role == 'reviewer' and task['author'] and task['author'] == self_pane_id`,
so a NULL author disabled it: a lane could have accepted its own rebase, and the only
thing that stopped it was the lane refusing on its own initiative.

The work tasks for the same repo+pr DO name the lane (assignee, completer, author), so
authorship is read from them - for the guard, and for the review timer's attribution.
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
import dunk_core  # noqa: E402
import dunk_github  # noqa: E402
import dunk_review  # noqa: E402
import dunk_roles  # noqa: E402
import dunk_tasks  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class AuthorshipFromWorkTasksTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tasks = dunk_tasks.Tasks(
            path=os.path.join(self.tmp.name, "tasks.db"))
        self.roles = dunk_roles.Roles(path=os.path.join(self.tmp.name, "roles.json"),
                                      default="anonymous")
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = dunk_core.Flock(herdr=FakeHerdr(), persist=False, restore=False,
                                     tasks=self.tasks, roles=self.roles,
                                     inbox=self.inbox)
        self.roles.register("%101", "reviewer", provider="claude", by="test")
        self.roles.register("%102", "reviewer", provider="claude", by="test")

    def tearDown(self):
        self.flock.close()
        self.tasks.close()
        self.inbox.close()
        self.tmp.cleanup()

    # -- helpers --------------------------------------------------------------

    def _fix_task(self, assignees=("%101",), completed_by=None, status="in_progress",
                  repo="huntergdavis/dunkingsheep", pr=124, author=None):
        task = self.tasks.add("global", "fix the thing", priority="high",
                              assignees=list(assignees), repo=repo, pr=pr,
                              author=author, created_by="%95")
        if status != "open":
            self.tasks.update(task["id"], status=status)
        if completed_by:
            self.tasks.complete(task["id"], by=completed_by)
        return task

    def _review_task(self, author=None, repo="huntergdavis/dunkingsheep", pr=124,
                     head="ec2129247a291773abe705311f396debedf8a5d5", assignee="%102"):
        return self.tasks.add("global", "review it", kind="review", priority="high",
                              assignees=[assignee], author=author, repo=repo, pr=pr,
                              head=head, created_by="review-sync")

    def _note(self, task_id, pane):
        # A verdict is only accepted after the reviewer recorded a note (t605).
        self.flock.task_note(task_id, reason="ran the tests; nothing else", self_pane_id=pane)

    def _lease(self, task_id, pane):
        claimed = self.tasks.claim_next(scope=dunk_tasks.GLOBAL, pane_id=pane,
                                        kinds=["review"])
        self.assertEqual(task_id, (claimed.get("task") or {}).get("id"),
                         f"{pane} did not get {task_id}: {claimed}")

    # -- the guard ------------------------------------------------------------

    def test_a_null_author_review_refuses_the_lane_that_worked_on_the_pr(self):
        self._fix_task(assignees=("%101",))
        review = self._review_task(author=None, assignee="%101")
        self._lease(review["id"], "%101")
        self._note(review["id"], "%101")
        with self.assertRaisesRegex(dunk_core.DunkError, "never accepts its own work"):
            self.flock.task_complete(review["id"], self_pane_id="%101",
                                     verdict="accept")

    def test_a_different_lane_may_file_the_verdict(self):
        self._fix_task(assignees=("%101",))
        review = self._review_task(author=None, assignee="%102")
        self._lease(review["id"], "%102")
        self._note(review["id"], "%102")
        done = self.flock.task_complete(review["id"], self_pane_id="%102",
                                        verdict="accept")
        self.assertEqual("accept", done.get("verdict") or
                         self.tasks.get(review["id"])["verdict"])

    def test_the_completer_of_the_fix_task_is_an_author_too(self):
        # t689's shape: assigned to two lanes, completed by one, head pushed by it.
        self._fix_task(assignees=("%101",), completed_by="%101", status="done")
        review = self._review_task(author=None, assignee="%101")
        self._lease(review["id"], "%101")
        self._note(review["id"], "%101")
        with self.assertRaisesRegex(dunk_core.DunkError, "never accepts its own work"):
            self.flock.task_complete(review["id"], self_pane_id="%101",
                                     verdict="needs-fix")

    def test_another_pr_is_not_this_pr(self):
        self._fix_task(assignees=("%101",), pr=77)
        review = self._review_task(author=None, pr=124, assignee="%101")
        self._lease(review["id"], "%101")
        self._note(review["id"], "%101")
        self.flock.task_complete(review["id"], self_pane_id="%101", verdict="accept")

    def test_the_review_tasks_own_assignee_does_not_make_the_reviewer_an_author(self):
        # The review task itself names the reviewer; counting it would refuse
        # every second review of the same PR (t662) - the exact false positive
        # that would break the two-reviewer rule.
        self.tasks.add("global", "earlier review", kind="review", priority="high",
                       assignees=["%102"], repo="huntergdavis/dunkingsheep", pr=124,
                       head="aaaa", created_by="review-sync", status="done")
        self.assertEqual([], self.tasks.authors_of("huntergdavis/dunkingsheep", 124))

    def test_authors_of_names_assignees_completers_and_authors(self):
        self._fix_task(assignees=("%101", "%99"), author="%102")
        authors = self.tasks.authors_of("huntergdavis/dunkingsheep", 124)
        self.assertEqual(["%101", "%102", "%99"], authors)

    # -- the review timer's attribution --------------------------------------

    def test_the_timer_attributes_a_rebase_to_the_one_lane_that_pushed_it(self):
        self._fix_task(assignees=("%101",), completed_by="%101", status="done")
        self.assertEqual("%101",
                         dunk_review.author_from_tasks("huntergdavis/dunkingsheep",
                                                       124, self.flock))

    def test_two_lanes_on_the_pr_are_not_guessed_into_one_author(self):
        self._fix_task(assignees=("%101", "%99"))
        self.assertIsNone(dunk_review.author_from_tasks("huntergdavis/dunkingsheep",
                                                        124, self.flock))

    def test_a_store_that_raises_attributes_nobody(self):
        class Broken:
            def authors_of(self, repo, pr):
                raise RuntimeError("db locked")
        flock = type("F", (), {"tasks": Broken()})()
        self.assertIsNone(dunk_review.author_from_tasks("huntergdavis/dunkingsheep",
                                                        124, flock))

    def test_an_unanswerable_store_refuses_the_verdict_rather_than_allowing_it(self):
        # Fail CLOSED: an unverified reviewer is precisely what this guard stops.
        self._fix_task(assignees=("%101",))
        review = self._review_task(author=None, assignee="%102")
        self._lease(review["id"], "%102")
        self._note(review["id"], "%102")

        def boom(repo, pr):
            raise RuntimeError("db locked")
        self.tasks.authors_of = boom
        with self.assertRaisesRegex(dunk_core.DunkError, "cannot verify"):
            self.flock.task_complete(review["id"], self_pane_id="%102",
                                     verdict="accept")


class AuthorMintRecordTests(unittest.TestCase):
    """The author App is shared, so the MINT is the only lane-level evidence (t723)."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "github-author-mints.json")

    def tearDown(self):
        self.tmp.cleanup()

    def test_the_only_recent_mint_is_the_author(self):
        now = time.time()
        dunk_github.record_author_mint("%101", at=now - 60, path=self.path)
        self.assertEqual("%101", dunk_github.author_from_mints(now - 1800, path=self.path,
                                                               now=now))

    def test_two_lanes_in_the_window_attribute_nobody(self):
        now = time.time()
        dunk_github.record_author_mint("%101", at=now - 60, path=self.path)
        dunk_github.record_author_mint("%108", at=now - 30, path=self.path)
        self.assertIsNone(dunk_github.author_from_mints(now - 1800, path=self.path,
                                                        now=now))

    def test_a_mint_before_the_window_is_not_the_author_of_this_head(self):
        now = time.time()
        dunk_github.record_author_mint("%101", at=now - 7200, path=self.path)
        self.assertIsNone(dunk_github.author_from_mints(now - 1800, path=self.path,
                                                        now=now))

    def test_the_same_lane_minting_twice_is_still_one_author(self):
        now = time.time()
        dunk_github.record_author_mint("%101", at=now - 600, path=self.path)
        dunk_github.record_author_mint("%101", at=now - 60, path=self.path)
        self.assertEqual("%101", dunk_github.author_from_mints(now - 1800, path=self.path,
                                                               now=now))

    def test_a_missing_or_unreadable_record_attributes_nobody(self):
        self.assertIsNone(dunk_github.author_from_mints(0, path=self.path))
        with open(self.path, "w", encoding="utf-8") as handle:
            handle.write("{not json")
        self.assertIsNone(dunk_github.author_from_mints(0, path=self.path))

    def test_the_record_is_private_and_prunes_old_mints(self):
        now = time.time()
        dunk_github.record_author_mint("%101", at=now - 90000, path=self.path)
        dunk_github.record_author_mint("%101", at=now, path=self.path)
        self.assertEqual(0o600, os.stat(self.path).st_mode & 0o777,
                         "a mint record names lanes; keep it to the owner")
        data = json.load(open(self.path, encoding="utf-8"))
        self.assertEqual(1, len(data["panes"]["%101"]),
                         "a mint a day old is pruned on write")

    def test_gh_token_persists_the_mint(self):
        # The daemon must record it at mint time, or the review timer has nothing.
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        old = os.environ.get("DUNKINGSHEEP_DIR")
        os.environ["DUNKINGSHEEP_DIR"] = tmp.name
        try:
            import importlib
            importlib.reload(dunk_github)
            flock = dunk_core.Flock(herdr=FakeHerdr(), persist=False, restore=False)
            flock.roles.register("%101", "worker", provider="pi", by="test")
            try:
                flock.gh_token(kind="author", self_pane_id="%101")
            except Exception as error:
                # No App keys in a test environment: the METER must not need them.
                self.assertIsNone(dunk_github.author_from_mints(0), str(error))
            else:
                path = os.path.join(tmp.name, dunk_github.AUTHOR_MINT_FILE)
                self.assertTrue(os.path.exists(path), "the mint was not recorded")
            flock.close()
        finally:
            if old is None:
                os.environ.pop("DUNKINGSHEEP_DIR", None)
            else:
                os.environ["DUNKINGSHEEP_DIR"] = old
            import importlib
            importlib.reload(dunk_github)


if __name__ == "__main__":
    unittest.main()
