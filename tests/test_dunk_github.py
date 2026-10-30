"""GitHub Apps and the review timer (FLAT_FLEET.md 6), against a fake GitHub."""

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

OWNER, WORKER, REVIEWER = "w1:p2", "w2:p3", "w2:p1"


class FakeTokens:
    def __init__(self):
        self.minted = []

    def get(self, kind, now=None):
        self.minted.append(kind)
        return {"token": f"tok-{kind}", "kind": kind, "slug": f"dunkingsheep-{kind}",
                "expires_at": 9e9, "cached": False}


class FakeGitHub:
    """Enough of dunk_github for the timer: open PRs, details, statuses, reviews."""
    REVIEW_CONTEXT = dunk_github.REVIEW_CONTEXT
    GitHubError = dunk_github.GitHubError

    def __init__(self, pulls):
        self.pulls = pulls
        self.statuses = []
        self.reviews = []
        self.fail_status = False

    def repos_config(self):
        return ["o/r"]

    def open_pulls(self, repo, token):
        return [dict(p, repo=repo) for p in self.pulls]

    def pull_detail(self, repo, number, token):
        return {"additions": 10, "deletions": 2, "changed_files": 1, "body": "pr body"}

    def post_status(self, repo, sha, state, token, description, target_url=None):
        if self.fail_status:
            raise dunk_github.GitHubError("GitHub POST statuses -> 403: Resource not accessible")
        self.statuses.append((repo, sha, state, token))

    def review_pull(self, repo, number, event, body, token):
        self.reviews.append((repo, number, event, token))

    def merge_pull(self, repo, number, token, sha=None):
        if getattr(self, "merge_blocked", False):
            raise dunk_github.GitHubError("GitHub PUT merge -> 405: Required status check is expected")
        self.merged = getattr(self, "merged", []) + [(repo, number, sha)]


def pull(number=7, head="a" * 40, branch="pi-worker-3/t124", login="dunkingsheep-author[bot]"):
    return {"number": number, "title": f"PR {number}", "url": f"https://x/{number}",
            "draft": False, "head": head, "branch": branch, "base": "main",
            "login": login, "created_at": "2026-10-29T20:00:00Z",
            "updated_at": "2026-10-29T20:00:00Z"}


class PureTests(unittest.TestCase):
    def test_kind_for_role(self):
        self.assertEqual("author", dunk_github.kind_for_role("worker"))
        self.assertEqual("reviewer", dunk_github.kind_for_role("reviewer"))
        self.assertIsNone(dunk_github.kind_for_role("owner"))
        self.assertEqual("reviewer", dunk_github.kind_for_role("owner", "reviewer"))
        self.assertIsNone(dunk_github.kind_for_role("desk"))
        with self.assertRaises(dunk_github.GitHubError):
            dunk_github.kind_for_role("worker", "reviewer")

    def test_protection_payload_pins_the_check_to_the_reviewer_app(self):
        payload = dunk_github.protection_payload(2, 5531872, bypass_app_slug="dunkingsheep-reviewer")
        self.assertEqual(2, payload["required_pull_request_reviews"]["required_approving_review_count"])
        self.assertTrue(payload["required_pull_request_reviews"]["dismiss_stale_reviews"])
        self.assertEqual([{"context": "dunkingsheep/review", "app_id": 5531872}],
                         payload["required_status_checks"]["checks"])
        self.assertTrue(payload["enforce_admins"])
        self.assertEqual(["dunkingsheep-reviewer"],
                         payload["required_pull_request_reviews"]["bypass_pull_request_allowances"]["apps"])

    def test_author_attribution_by_branch_then_by_recent_mint(self):
        lanes = {"w2:p3": {"role": "worker", "label": "pi-worker-3"}}
        self.assertEqual("w2:p3", dunk_review.attribute_author(pull(), lanes, {}))
        created = dunk_review._iso("2026-10-29T20:00:00Z")
        mints = {"w9:p9": created - 600, "w9:p8": created - 7200 * 2}
        self.assertEqual("w9:p9", dunk_review.attribute_author(pull(branch="x/y"), {}, mints))
        self.assertIsNone(dunk_review.attribute_author(pull(branch="x/y"), {}, {}))


class TimerTests(unittest.TestCase):
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
        self.roles.register(WORKER, "worker", provider="pi", label="pi-worker-3")
        self.roles.register(REVIEWER, "reviewer", provider="claude", label="pi-review-1")
        self.github = FakeGitHub([pull()])
        self.tokens = FakeTokens()
        self.flock._github_token_cache = self.tokens
        self.sync = dunk_review.Sync(self.flock, tokens=self.tokens, github=self.github)

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

    def test_a_pr_becomes_one_review_task_with_a_pending_status(self):
        first = self.sync.run()
        self.assertEqual(1, len(first["filed"]))
        self.assertEqual(WORKER, first["filed"][0]["author"])
        self.assertEqual([("o/r", "a" * 40, "pending", "tok-reviewer")], self.github.statuses)
        second = self.sync.run()
        self.assertEqual([], second["filed"], "same head: nothing new")
        tasks = self.flock.tasks.list(kind="review", include_done=False)
        self.assertEqual(1, len(tasks))
        self.assertIn("author lane: w2:p3", tasks[0]["body"])

    def test_a_moved_head_cancels_the_old_review_and_files_a_delta(self):
        self.sync.run()
        self.github.pulls = [pull(head="b" * 40)]
        report = self.sync.run()
        self.assertEqual(1, len(report["cancelled"]))
        self.assertEqual(1, len(report["filed"]))
        live = [t for t in self.flock.tasks.list(kind="review", include_done=True)
                if t["status"] != "cancelled"]
        self.assertEqual(["b" * 40], [t["head"] for t in live])

    def test_a_closed_pr_cancels_its_review(self):
        self.sync.run()
        self.github.pulls = []
        report = self.sync.run()
        self.assertEqual(1, len(report["cancelled"]))

    def test_a_missing_statuses_permission_is_reported_not_fatal(self):
        self.github.fail_status = True
        report = self.sync.run()
        self.assertEqual(1, len(report["filed"]))
        self.assertTrue(any("403" in e for e in report["errors"]))

    def test_accept_posts_success_and_approves_as_the_reviewer_app(self):
        self.sync.run()
        task = self.flock.tasks.list(kind="review", include_done=False)[0]
        self.call("task_next", {}, pane=REVIEWER)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)
        done = self.call("task_complete", {"id": task["id"], "verdict": "accept"}, pane=REVIEWER)
        self.assertEqual("accept", done["verdict"])
        self.assertEqual("approved", done["github"]["review"])
        self.assertIn(("o/r", "a" * 40, "success", "tok-reviewer"), self.github.statuses)
        self.assertEqual([("o/r", 7, "APPROVE", "tok-reviewer")], self.github.reviews)

    def test_an_accepted_head_is_never_refiled_and_is_merged_when_protection_allows(self):
        self.sync.run()
        task = self.flock.tasks.list(kind="review", include_done=False)[0]
        self.call("task_next", {}, pane=REVIEWER)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)
        self.github.merge_blocked = True       # poolrad-style: a second approval still missing
        done = self.call("task_complete", {"id": task["id"], "verdict": "accept"}, pane=REVIEWER)
        self.assertFalse(done["github"]["merged"])
        again = self.sync.run()
        self.assertEqual([], again["filed"], "a judged head gets no second review")
        # Pending, success, then success re-asserted by the retry: never pending again.
        self.assertEqual(["pending", "success"], [s[2] for s in self.github.statuses][:2])
        self.assertNotIn("pending", [s[2] for s in self.github.statuses][1:])
        self.github.merge_blocked = False
        third = self.sync.run()
        self.assertEqual([("o/r", 7, "a" * 40)], self.github.merged)
        self.assertEqual(1, len(third["merged"]))
        self.assertEqual("success", self.github.statuses[-1][2], "retry re-asserts success")

    def test_needs_fix_requests_changes_and_files_work_for_the_author(self):
        self.sync.run()
        task = self.flock.tasks.list(kind="review", include_done=False)[0]
        self.call("task_next", {}, pane=REVIEWER)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)
        done = self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"}, pane=REVIEWER)
        self.assertEqual("changes_requested", done["github"]["review"])
        followup = self.flock.tasks.get(done["github"]["followup"])
        self.assertEqual("work", followup["kind"])
        self.assertEqual([WORKER], followup["assignees"])

    def test_gh_token_follows_the_role(self):
        self.assertEqual("author", self.call("gh_token", {}, pane=WORKER)["kind"])
        self.assertEqual("reviewer", self.call("gh_token", {}, pane=REVIEWER)["kind"])
        self.assertIsNone(self.call("gh_token", {}, pane=OWNER)["token"])
        self.assertEqual("reviewer", self.call("gh_token", {"kind": "reviewer"}, pane=OWNER)["kind"])
        with self.assertRaisesRegex(DunkError, "may only act as the author App"):
            self.call("gh_token", {"kind": "reviewer"}, pane=WORKER)
        self.assertIn(WORKER, self.flock._gh_token_mints)

    def test_review_list_reports_age_and_reviewer(self):
        self.sync.run()
        self.call("task_next", {}, pane=REVIEWER)
        rows = self.call("review_list", {}, pane=OWNER)["reviews"]
        self.assertEqual(1, len(rows))
        self.assertEqual([REVIEWER], rows[0]["reviewer"])
        self.assertEqual(7, rows[0]["pr"])

    def test_review_sync_is_not_for_lanes(self):
        with self.assertRaisesRegex(DunkError, "not permitted for role worker|requires a superuser"):
            self.call("review_sync", {}, pane=WORKER)


if __name__ == "__main__":
    unittest.main()
