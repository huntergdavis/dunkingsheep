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
# poolrad needs two distinct reviewer lanes (t662): the second one is not the author.
SECOND_REVIEWER = "w2:p2"


class FakeTokens:
    def __init__(self):
        self.minted = []

    def get(self, kind, now=None):
        self.minted.append(kind)
        return {"token": f"tok-{kind}", "kind": kind, "slug": f"dunkingsheep-{kind}",
                "expires_at": 9e9, "cached": False}


class FakeGitHub:
    """Enough of dunk_github for the timer: open PRs, details, statuses, reviews.

    The accounting flow also needs a small git: refs, trees, commits, file text.
    It is modelled honestly rather than stubbed, because the point of the feature
    is that the ledger row lands as a commit on top of the base branch.
    """
    REVIEW_CONTEXT = dunk_github.REVIEW_CONTEXT
    GitHubError = dunk_github.GitHubError

    def __init__(self, pulls, repo="o/r", ledger=None, files=None, pr_body="pr body"):
        self.pulls = pulls
        self.repo = repo
        self.statuses = []
        self.reviews = []
        self.fail_status = False
        self.fail_branch = False
        self.pr_body = pr_body
        self.ledger = ("id,evidence_doc\n"
                       "OVL_0001_0001,docs/re/X_0001.md\n"
                       "OVL_00FF_0000,docs/re/X_00FF.md\n"
                       if ledger is None else ledger)
        self.files = ([{"filename": "docs/re/U7F_STATE_FLAGS_0F7E.md", "status": "added"}]
                      if files is None else files)
        self.base_sha = "c" * 40
        self.refs = {"main": self.base_sha}
        self.texts = {self.base_sha: self.ledger}      # commit sha -> ledger text
        self.trees = {}
        # The base commit exists before anything is committed: the accounting commit
        # must hang off it, not off nothing.
        self.commits = {self.base_sha: {"tree": "t0000", "parents": [], "message": "base",
                                        "token": None}}
        self.prs_opened = []
        self.auto_merge = []                           # (number, token)
        self.state = {"state": "open", "merged": False, "merged_at": None,
                      "mergeable_state": "blocked", "auto_merge": True}
        self.next_number = 900

    # -- what the review timer already used ---------------------------------

    def repos_config(self):
        return [self.repo]

    def open_pulls(self, repo, token):
        return [dict(p, repo=repo) for p in self.pulls]

    def pull_detail(self, repo, number, token):
        # GET /repos/{r}/pulls/{n}: mergeable is a BOOLEAN and mergeable_state is
        # free text ("dirty" = conflicts, "blocked" = a pending check, "clean").
        detail = {"additions": 10, "deletions": 2, "changed_files": 1,
                  "body": self.pr_body}
        detail.update(getattr(self, "details", {}).get(number, {}))
        return detail

    def post_status(self, repo, sha, state, token, description, target_url=None):
        if self.fail_status:
            raise dunk_github.GitHubError("GitHub POST statuses -> 403: Resource not accessible")
        self.statuses.append((repo, sha, state, token))

    def review_pull(self, repo, number, event, body, token):
        self.reviews.append((repo, number, event, token))

    # -- the accounting flow (FLAT_FLEET.md 6.4, revised) -------------------

    def get_ref(self, repo, ref, token):
        if ref not in self.refs:
            raise dunk_github.GitHubError(f"GitHub GET git/ref/heads/{ref} -> 404: Not Found")
        return self.refs[ref]

    def ref_exists(self, repo, branch, token):
        return branch in self.refs

    def commit_tree(self, repo, sha, token):
        return self.commits[sha]["tree"]

    def file_text(self, repo, path, ref, token):
        if ref not in self.texts:
            return None, None
        return self.texts[ref], "b" * 40

    def pr_files(self, repo, number, token):
        return list(self.files)

    def create_tree(self, repo, base_tree, entries, token):
        sha = f"t{len(self.trees) + 1:04d}"
        self.trees[sha] = (base_tree, entries)
        return sha

    def create_commit(self, repo, message, tree, parents, token):
        sha = f"k{len(self.commits) + 1:04d}"
        self.commits[sha] = {"tree": tree, "parents": list(parents),
                             "message": message, "token": token}
        _base, entries = self.trees[tree]
        for entry in entries:
            self.texts[sha] = entry["content"]
        return sha

    def create_ref(self, repo, branch, sha, token):
        if branch in self.refs or self.fail_branch:
            raise dunk_github.GitHubError("GitHub POST git/refs -> 422: Reference already exists")
        self.refs[branch] = sha
        return sha

    def open_pull(self, repo, title, head, base, body, token):
        number = self.next_number
        self.next_number += 1
        pr = {"number": number, "url": f"https://x/{number}", "node_id": f"n{number}",
              "head": head, "base": base, "title": title, "body": body,
              "token": token}
        self.prs_opened.append(pr)
        return {"number": number, "url": pr["url"], "node_id": pr["node_id"],
                "head": self.refs[head]}

    def pulls_for_head(self, repo, branch, token):
        return [{"number": pr["number"], "url": pr["url"], "node_id": pr["node_id"]}
                for pr in self.prs_opened if pr["head"] == branch]

    def enable_auto_merge(self, repo, number, token, method="MERGE"):
        self.auto_merge.append((number, token, method))
        return {"pullRequest": {"number": number}}

    def pull_state(self, repo, number, token):
        return dict(self.state)

    def merge_pull(self, repo, number, token, sha=None):
        if getattr(self, "merge_blocked", False):
            raise dunk_github.GitHubError("GitHub PUT merge -> 405: Required status check is expected")
        self.merged = getattr(self, "merged", []) + [(repo, number, sha)]


def pull(number=7, head="a" * 40, branch="pi-worker-3/t124", login="dunkingsheep-author[bot]",
         mergeable=None):
    """One row of GET /repos/{r}/pulls, as GitHub actually answers it.

    The LIST endpoint carries no `mergeable` and no `mergeable_state` on
    github.com (measured 2026-10-30: the keys are absent), which is why t704's
    skip cannot read them from here. `mergeable` is accepted only so a test can
    put a stale value in the list payload and prove it is ignored; the real
    signal comes from pull_detail() below.
    """
    row = {"number": number, "title": f"PR {number}", "url": f"https://x/{number}",
           "draft": False, "head": head, "branch": branch, "base": "main",
           "login": login, "created_at": "2026-10-29T20:00:00Z",
           "updated_at": "2026-10-29T20:00:00Z"}
    if mergeable is not None:
        row["mergeable"] = mergeable
    return row


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
        # A verdict needs evidence (t621): the reviewer notes what it ran first.
        self.call("task_note", {"id": task["id"], "reason": "reproduced 4 tests, head aaaa"},
                  pane=REVIEWER)
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
        # A verdict needs evidence, recorded after the claim (t621, #131). This
        # test landed after that gate merged, so it was red on main until here.
        self.call("task_note", {"id": task["id"], "reason": "reproduced; head aaaa"},
                  pane=REVIEWER)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)
        self.github.merge_blocked = True       # poolrad-style: a second approval still missing
        done = self.call("task_complete", {"id": task["id"], "verdict": "accept"}, pane=REVIEWER)
        self.assertFalse(done["github"]["merged"])
        again = self.sync.run()
        self.assertEqual([], again["filed"], "a judged head gets no second review")
        self.assertEqual(2, len(self.github.statuses))  # pending then success only
        self.assertEqual("success", self.github.statuses[-1][2])
        self.github.merge_blocked = False
        third = self.sync.run()
        self.assertEqual([("o/r", 7, "a" * 40)], self.github.merged)
        self.assertEqual(1, len(third["merged"]))

    def test_needs_fix_requests_changes_and_files_work_for_the_author(self):
        self.sync.run()
        task = self.flock.tasks.list(kind="review", include_done=False)[0]
        self.call("task_next", {}, pane=REVIEWER)
        self.call("task_note", {"id": task["id"], "reason": "the emitter does not reproduce"},
                  pane=REVIEWER)
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


POOLRAD = "huntergdavis/poolrad-decomp"


def poolrad_pull(number=1631, head="d" * 40, title="Add raw-CPU model of OVL_007F_0F7E"):
    return {"number": number, "title": title, "url": f"https://x/{number}",
            "draft": False, "head": head, "branch": "pi-worker-3/t124", "base": "main",
            "login": "dunkingsheep-author[bot]",
            "created_at": "2026-10-29T20:00:00Z",
            "updated_at": "2026-10-29T20:00:00Z"}


class AccountingPureTests(unittest.TestCase):
    def test_row_id_pairing_uses_the_poolrad_convention(self):
        self.assertTrue(dunk_review.own_row("OVL_007F_0F7E", "U7F_STATE_FLAGS_0F7E"))
        self.assertTrue(dunk_review.own_row("OVL_0038_0400", "overlay_u38_slot11_0400_cpu"))
        self.assertTrue(dunk_review.own_row("RES_00BA_2199", "resident_00ba_2199_lookup"))
        # A document that names neither the unit nor the offset pairs with nothing: the
        # refusal is the point (poolrad's own RES rows mostly point at a shared note).
        self.assertFalse(dunk_review.own_row("RES_00BA_2199", "RESIDENT_SIDECAR_LOOKUP"))
        self.assertFalse(dunk_review.own_row("OVL_007F_0F7E", "U7F_STATE_FLAGS_0A01"))
        self.assertFalse(dunk_review.own_row("OVL_007F_0F7E", "U7F_STATE_FLAGS"))
        self.assertFalse(dunk_review.own_row("garbage", "U7F_STATE_FLAGS_0F7E"))

    def test_row_ids_are_whole_tokens_only(self):
        text = "Accept OVL_007F_0F7E (84 B): 289 strict. Not OVL_007F_0F7E_EXTRA or XOVL_007F_0F7E."
        self.assertEqual(["OVL_007F_0F7E"], dunk_review.row_ids(text))

    def test_the_row_is_inserted_in_sorted_position(self):
        ledger = "id,evidence_doc\nOVL_0001_0001,docs/re/A.md\nOVL_00FF_0000,docs/re/B.md\n"
        out = dunk_review.with_row(ledger, "OVL_007F_0F7E", "docs/re/C.md")
        self.assertEqual(["OVL_0001_0001", "OVL_007F_0F7E", "OVL_00FF_0000"],
                         [line.split(",")[0] for line in out.splitlines()[1:]])
        self.assertTrue(out.endswith("\n"))
        self.assertEqual("id,evidence_doc", out.splitlines()[0])

    def test_a_crlf_ledger_keeps_its_line_endings(self):
        ledger = "id,evidence_doc\r\nOVL_0001_0001,docs/re/A.md\r\n"
        out = dunk_review.with_row(ledger, "OVL_007F_0F7E", "docs/re/C.md")
        self.assertNotIn("\n", out.replace("\r\n", ""))
        self.assertEqual(3, out.count("\r\n"))

    def test_choose_row_refuses_a_verdict_naming_two_ids(self):
        row, reason = dunk_review.choose_row(
            [("verdict notes", "Accept OVL_007F_0F7E and OVL_007F_0A01")],
            ["docs/re/U7F_STATE_FLAGS_0F7E.md"], "id,evidence_doc\n")
        self.assertIsNone(row)
        self.assertIn("names 2 row ids", reason)

    def test_choose_row_refuses_an_already_credited_id(self):
        ledger = "id,evidence_doc\nOVL_007F_0F7E,docs/re/OLD.md\n"
        row, reason = dunk_review.choose_row(
            [("verdict notes", "Accept OVL_007F_0F7E")],
            ["docs/re/U7F_STATE_FLAGS_0F7E.md"], ledger)
        self.assertIsNone(row)
        self.assertIn("already credited as docs/re/OLD.md", reason)

    def test_choose_row_prefers_the_verdict_then_the_title(self):
        row, reason = dunk_review.choose_row(
            [("verdict notes", "looks good"), ("PR title", "Add OVL_007F_0F7E model")],
            ["docs/re/U7F_STATE_FLAGS_0F7E.md"], "id,evidence_doc\n")
        self.assertEqual(("OVL_007F_0F7E", "docs/re/U7F_STATE_FLAGS_0F7E.md"), row)
        self.assertEqual("", reason)


class AccountingTests(unittest.TestCase):
    """FLAT_FLEET.md 6.4 revised: acceptance is a bot-reviewed accounting PR."""

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
        # poolrad needs two distinct reviewer lanes (t662), so a final accept exists.
        self.roles.register(SECOND_REVIEWER, "reviewer", provider="codex", label="rev-2")
        self.github = FakeGitHub([poolrad_pull()], repo=POOLRAD)
        self.tokens = FakeTokens()
        self.flock._github_token_cache = self.tokens
        self.sync = dunk_review.Sync(self.flock, tokens=self.tokens, github=self.github)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)

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

    def review_task(self, verdict="accept", note="Accept OVL_007F_0F7E (84 B): 289 strict / 334 identities"):
        self.sync.run()
        task = self.flock.tasks.list(kind="review", include_done=False)[0]
        self.call("task_next", {}, pane=REVIEWER)
        if note:
            self.call("task_note", {"id": task["id"], "reason": note}, pane=REVIEWER)
        done = self.call("task_complete", {"id": task["id"], "verdict": verdict},
                         pane=REVIEWER)
        if verdict != "accept" or (done.get("github") or {}).get("status") != "pending":
            return done
        # A poolrad head is final only when two DISTINCT lanes accept it (t662), and
        # the ledger row travels with the verdict that makes it final (6.4): the
        # first accept files the second review, the second one accounts.
        second = [t for t in self.flock.tasks.list(kind="review", include_done=False)
                  if t["id"] != task["id"] and t["status"] != "done"][0]
        self.call("task_next", {}, pane=SECOND_REVIEWER)
        self.call("task_note", {"id": second["id"], "reason": note}, pane=SECOND_REVIEWER)
        return self.call("task_complete", {"id": second["id"], "verdict": verdict},
                         pane=SECOND_REVIEWER)

    def test_accept_opens_an_accounting_pr_approved_by_the_author_app(self):
        done = self.review_task()
        accounting = done["github"]["accounting"]
        self.assertEqual("OVL_007F_0F7E,docs/re/U7F_STATE_FLAGS_0F7E.md", accounting["row"])
        self.assertEqual(900, accounting["accounting_pr"])
        self.assertEqual("approved_by_author_app", accounting["review"])
        self.assertEqual("enabled", accounting["auto_merge"])
        self.assertEqual([], accounting["errors"])
        # The commit is the reviewer App's; the approval and the auto-merge are the
        # author App's, which is what makes the two identities distinct.
        commit = self.github.commits[self.github.refs["accounting/pr1631-OVL_007F_0F7E"]]
        self.assertEqual("tok-reviewer", commit["token"])
        self.assertIn("Accept OVL_007F_0F7E: accounting row for #1631", commit["message"])
        self.assertEqual(1, len(self.github.prs_opened))
        self.assertEqual("tok-reviewer", self.github.prs_opened[0]["token"])
        self.assertEqual([(900, "APPROVE", "tok-author")],
                         [(n, e, t) for _r, n, e, t in self.github.reviews if n == 900])
        self.assertEqual([(900, "tok-author", "MERGE")], self.github.auto_merge)

    def test_the_ledger_row_lands_on_top_of_the_base_commit(self):
        self.review_task()
        branch = "accounting/pr1631-OVL_007F_0F7E"
        sha = self.github.refs[branch]
        self.assertEqual([self.github.base_sha], self.github.commits[sha]["parents"])
        text = self.github.texts[sha]
        self.assertEqual(["id,evidence_doc", "OVL_0001_0001,docs/re/X_0001.md",
                          "OVL_007F_0F7E,docs/re/U7F_STATE_FLAGS_0F7E.md",
                          "OVL_00FF_0000,docs/re/X_00FF.md"], text.splitlines())

    def test_both_pr_numbers_are_recorded_on_the_review_task(self):
        done = self.review_task()
        task = self.flock.tasks.get(done["id"])
        self.assertEqual(1631, task["pr"])
        self.assertEqual(900, task["accounting_pr"])
        notes = [n["text"] for n in self.flock.tasks.notes(task["id"], event="accounting")]
        self.assertTrue(notes)
        self.assertIn("accounting PR #900 for body PR #1631", notes[-1])

    def test_a_verdict_without_a_row_id_opens_nothing_and_says_why(self):
        self.github.pulls = [poolrad_pull(title="Add a raw-CPU model of a slot-43 handler")]
        done = self.review_task(note="Reviewed; the media reproduce.")
        accounting = done["github"]["accounting"]
        self.assertIsNone(accounting["accounting_pr"])
        self.assertIn("no row id", accounting["reason"])
        self.assertEqual([], self.github.prs_opened)
        self.assertIsNone(self.flock.tasks.get(done["id"])["accounting_pr"])

    def test_an_already_credited_row_opens_no_pr(self):
        self.github.texts[self.github.base_sha] = ("id,evidence_doc\n"
                                                   "OVL_007F_0F7E,docs/re/OLD.md\n")
        done = self.review_task()
        accounting = done["github"]["accounting"]
        self.assertIn("already credited as docs/re/OLD.md", accounting["reason"])
        self.assertEqual([], self.github.prs_opened)

    def test_needs_fix_opens_no_accounting_pr(self):
        done = self.review_task(verdict="needs-fix")
        self.assertIsNone(done["github"]["accounting"])
        self.assertEqual([], self.github.prs_opened)

    def test_a_repo_without_a_ledger_gets_no_accounting_pr(self):
        self.github.repo = "o/r"
        self.github.pulls = [pull()]
        done = self.review_task()
        accounting = done["github"]["accounting"]
        self.assertIn("no accounting ledger is configured", accounting["reason"])
        self.assertEqual([], self.github.prs_opened)

    def test_an_existing_accounting_branch_is_reused_not_overwritten(self):
        branch = "accounting/pr1631-OVL_007F_0F7E"
        self.github.refs[branch] = "e" * 40
        self.github.prs_opened.append({"number": 811, "url": "https://x/811",
                                       "node_id": "n811", "head": branch})
        done = self.review_task()
        accounting = done["github"]["accounting"]
        self.assertEqual(811, accounting["accounting_pr"])
        self.assertIn("already open", accounting["reason"])
        self.assertEqual({self.github.base_sha}, set(self.github.commits),
                         "no new commit for an open accounting PR")
        self.assertEqual(811, self.flock.tasks.get(done["id"])["accounting_pr"])

    def test_an_accept_before_the_merge_files_no_accounting_pr(self):
        """t835: #1637 was filed for body #1630 while #1630 was still open, so the
        evidence doc it credited was not on main and check_evidence_docs failed."""
        self.github.merge_blocked = True          # second verdict / protection pending
        done = self.review_task()
        self.assertFalse(done["github"]["merged"])
        self.assertIn("not merged", done["github"]["accounting"]["skipped"])
        self.assertEqual([], self.github.prs_opened, "no credit before the body lands")

    def test_the_merge_files_exactly_one_accounting_pr(self):
        """The row is written by the sync that OBSERVES the merge, once."""
        self.github.merge_blocked = True
        self.review_task()
        self.assertEqual([], self.github.prs_opened)
        self.github.merge_blocked = False
        report = self.sync.run()
        self.assertEqual([(POOLRAD, 1631)],
                         [(repo, number) for repo, number, _sha in self.github.merged])
        self.assertEqual(1, len(report["accounting"]), "the merge writes the row")
        self.assertEqual(900, report["accounting"][0]["accounting_pr"])
        self.assertEqual(1, len(self.github.prs_opened))
        # Idempotent: a second sweep may retry the merge (the fake has no MERGED
        # state), but it must never open a second accounting PR for the same body.
        again = self.sync.run()
        self.assertEqual(1, len(again["accounting"]))
        self.assertIn("already open", again["accounting"][0]["reason"] or "")
        self.assertEqual(1, len(self.github.prs_opened))

    def test_a_github_failure_on_the_commit_keeps_the_verdict(self):
        self.github.fail_branch = True
        done = self.review_task()
        accounting = done["github"]["accounting"]
        self.assertIsNone(accounting["accounting_pr"])
        self.assertTrue(any("422" in error for error in accounting["errors"]))
        self.assertEqual("accept", done["verdict"], "a GitHub failure never undoes a verdict")


if __name__ == "__main__":
    unittest.main()


class DistinctReviewerTests(unittest.TestCase):
    """t662: GitHub sees ONE App identity, so the daemon counts distinct lanes.

    Measured on poolrad #1630: two App approvals came from the same lane (%101), so
    "two reviewers" was one reviewer and GitHub could never tell. The count lives in
    the task store, where completed_by is the lane.
    """

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
        # Two reviewer panes, both on a provider that may review.
        self.roles.register(REVIEWER, "reviewer", provider="claude", label="rev-1")
        self.roles.register("w2:p2", "reviewer", provider="codex", label="rev-2")
        self.github = FakeGitHub([pull()])
        self.github.merged = []
        self.tokens = FakeTokens()
        self.flock._github_token_cache = self.tokens
        self.sync = dunk_review.Sync(self.flock, tokens=self.tokens, github=self.github)
        self.flock._after_review_verdict = lambda t, v, by=None: self.sync.on_verdict(t, v, by=by)
        self._real_required = dunk_github.REPO_REQUIRED_VERDICTS
        dunk_github.REPO_REQUIRED_VERDICTS = {"o/r": 2}

    def tearDown(self):
        dunk_github.REPO_REQUIRED_VERDICTS = self._real_required
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

    def review_tasks(self):
        return [t for t in self.flock.tasks.list(kind="review", include_done=False)]

    def accept(self, pane):
        self.call("task_next", {}, pane=pane)
        task = [t for t in self.review_tasks() if t["status"] == "in_progress"][0]
        # A verdict needs evidence recorded after the claim (t621).
        self.call("task_note", {"id": task["id"],
                                "reason": f"reproduced at head aaaa from {pane}"},
                  pane=pane)
        return self.call("task_complete", {"id": task["id"], "verdict": "accept"},
                         pane=pane)

    def test_one_of_two_does_not_approve_or_merge_and_files_the_second_review(self):
        self.sync.run()
        done = self.accept(REVIEWER)
        self.assertFalse(done["github"]["merged"])
        self.assertEqual("pending", done["github"]["status"],
                         "1 of 2 accepted is not a final head")
        self.assertIsNone(done["github"]["review"],
                          "no App approval until the lane count is met")
        self.assertEqual([], self.github.reviews)
        self.assertEqual([], self.github.merged)
        second = self.flock.tasks.get(done["github"]["second_review"])
        self.assertEqual("review", second["kind"])
        self.assertEqual(REVIEWER, second["author"],
                         "the lane that accepted is excluded from the second review")
        self.assertEqual("a" * 40, second["head"])

    def test_the_same_lane_accepting_twice_is_one_lane(self):
        self.sync.run()
        self.accept(REVIEWER)
        accepts, needs_fix = self.sync.head_verdicts("o/r", "a" * 40, 7)
        self.assertEqual([REVIEWER], accepts)
        self.assertEqual([], needs_fix)
        final, accepts, _needs_fix, required = self.sync._is_final("o/r", "a" * 40, 7)
        self.assertFalse(final)
        self.assertEqual(2, required)

    def test_two_distinct_lanes_approve_and_merge(self):
        self.sync.run()
        self.accept(REVIEWER)
        done = self.accept("w2:p2")
        self.assertEqual("success", done["github"]["status"])
        self.assertEqual("approved", done["github"]["review"])
        self.assertTrue(done["github"]["merged"])
        self.assertEqual([("o/r", 7, "APPROVE", "tok-reviewer")], self.github.reviews)
        self.assertEqual([("o/r", 7, "a" * 40)], self.github.merged)
        accepts, _ = self.sync.head_verdicts("o/r", "a" * 40, 7)
        self.assertEqual(sorted([REVIEWER, "w2:p2"]), accepts)

    def test_a_needs_fix_from_any_lane_stops_the_head(self):
        self.sync.run()
        self.call("task_next", {}, pane=REVIEWER)
        task = [t for t in self.review_tasks() if t["status"] == "in_progress"][0]
        # A verdict needs evidence recorded after the claim (t621).
        self.call("task_note", {"id": task["id"], "reason": "the emitter does not reproduce"},
                  pane=REVIEWER)
        self.call("task_complete", {"id": task["id"], "verdict": "needs-fix"},
                  pane=REVIEWER)
        # A second lane accepting the SAME head must not turn it green.
        self.flock.tasks.add("global", "second opinion on the same head", kind="review",
                             repo="o/r", pr=7, head="a" * 40, author=REVIEWER,
                             created_by="review-sync")
        done = self.accept("w2:p2")
        self.assertFalse(done["github"].get("merged"))
        self.assertEqual("failure", done["github"]["status"])
        self.assertEqual([], self.github.merged)


class ConflictedHeadTests(unittest.TestCase):
    """t704: a CONFLICTING head cannot merge, so no reviewer turn is spent on it.

    Measured 2026-10-29: #120 was reviewed at 9781629 while conflicted, and #124's
    APPROVED head 5e2e8b3 was already conflicted - each round cost a reviewer turn
    and a worker turn, and the cutover gate never fired.
    """

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
        self.roles.register(REVIEWER, "reviewer", provider="claude", label="rev-1")
        self.tokens = FakeTokens()
        self.flock._github_token_cache = self.tokens

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def sync(self, pulls, detail=None):
        github = FakeGitHub(pulls)
        github.merged = []
        github.details = {7: dict(detail or {})}
        return dunk_review.Sync(self.flock, tokens=self.tokens, github=github)

    def review_tasks(self):
        return self.flock.tasks.list(kind="review", include_done=False)

    def test_a_conflicted_pr_gets_no_review_task(self):
        """Measured shape: GET /pulls/7 answers mergeable=false, state=dirty."""
        report = self.sync([pull()], {"mergeable": False, "mergeable_state": "dirty"}).run()
        self.assertEqual([], report["filed"], "no reviewer turn on an unmergeable head")
        self.assertEqual([], self.review_tasks())
        self.assertEqual([{"repo": "o/r", "pr": 7, "head": "a" * 12}],
                         report["conflicted"], "the report names it, so it is not silent")

    def test_the_list_payload_is_not_trusted_for_mergeable(self):
        """The list endpoint carries no mergeable at all, and a stale string in it
        must not decide anything: the word "conflicting" is GraphQL-only."""
        report = self.sync([pull(mergeable="conflicting")],
                           {"mergeable": True, "mergeable_state": "clean"}).run()
        self.assertEqual(1, len(report["filed"]))
        self.assertEqual([], report["conflicted"])

    def test_an_unknown_mergeable_still_files(self):
        """mergeable=None means GitHub has not computed it yet - `unknown` or
        `dirty`-less - and UNKNOWN must never stall the review queue."""
        for detail in ({"mergeable": None, "mergeable_state": "unknown"}, {}):
            with self.subTest(detail=detail):
                self.flock.tasks.close()
                self.flock.tasks = Tasks(path=os.path.join(self.tmp.name,
                                                           f"tasks-{len(detail)}.db"))
                report = self.sync([pull()], detail).run()
                self.assertEqual(1, len(report["filed"]))
                self.assertEqual([], report["conflicted"])

    def test_a_mergeable_pr_is_filed_as_before(self):
        report = self.sync([pull()], {"mergeable": True,
                                      "mergeable_state": "blocked"}).run()
        self.assertEqual(1, len(report["filed"]), "a blocked-by-checks PR is still reviewed")
        self.assertEqual([], report["conflicted"])

    def test_a_failed_lookup_files_the_review_and_reports_the_error(self):
        github = FakeGitHub([pull()])
        github.pull_detail = lambda *a, **k: (_ for _ in ()).throw(
            dunk_github.GitHubError("GitHub GET pulls/7 -> 500"))
        report = dunk_review.Sync(self.flock, tokens=self.tokens, github=github).run()
        self.assertEqual(1, len(report["filed"]), "an API error is not evidence of a conflict")
        self.assertTrue(any("mergeable o/r#7" in e for e in report["errors"]))
