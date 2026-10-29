#!/usr/bin/env python3
"""The review timer: GitHub PRs -> review tasks -> commit status (FLAT_FLEET.md 6.3).

Deterministic, no model. Every two minutes:

1. For every repo in `github-repos.json`, list open non-draft PRs as the
   reviewer App.
2. A PR head with no live `kind=review` task gets one (priority high) whose
   body carries what a real review needs. A PR whose head moved gets its old
   task cancelled and a delta-review task filed; the store's unique index on
   (kind, repo, pr, head) makes a duplicate impossible.
3. The `dunkingsheep/review` commit status follows the task: pending while a
   review is open, success on `accept`, failure on `needs-fix` (plus a
   `kind=work` follow-up for the author lane). On `accept` the reviewer App
   also posts an approving review, which is what branch protection counts.

Authorship: PRs are opened as `dunkingsheep-author[bot]`, so the lane that
wrote them is recovered from the branch name (`<lane label>/...`) against the
role registry, falling back to the lane that minted an author token most
recently before the PR was opened. When neither matches, `author` is null and
the record says so; the reviewer != author rule then has nothing to bind to.
"""

from __future__ import annotations

import logging
import time

import dunk_github

log = logging.getLogger("dunkingsheep.review")


def attribute_author(pr, lanes, mints):
    """Pane id of the lane that opened `pr`, or None."""
    branch = (pr.get("branch") or "").lower()
    for pane_id, entry in (lanes or {}).items():
        label = str(entry.get("label") or "").strip().lower()
        if label and (branch.startswith(label + "/") or branch.startswith(label + "-")):
            return pane_id
    created = _iso(pr.get("created_at"))
    recent = [(pane_id, at) for pane_id, at in (mints or {}).items()
              if created and at <= created and created - at < 2 * 3600]
    # One lane minted in the window: that lane. Several: say unknown rather
    # than guess, because a wrong author lets the real one review its own PR.
    return recent[0][0] if len(recent) == 1 else None


def _iso(value):
    if not value:
        return None
    try:
        import calendar
        return calendar.timegm(time.strptime(value, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return None


def review_body(pr, detail, author):
    lines = [
        f"REVIEW {pr['repo']}#{pr['number']} @ {pr['head']}",
        f"title: {pr['title']}",
        f"url: {pr['url']}",
        f"branch: {pr['branch']} -> {pr['base']}",
        f"author lane: {author or 'unknown (not attributable; say so in the verdict)'}",
        f"opened: {pr.get('created_at')}  updated: {pr.get('updated_at')}",
        f"diff: +{detail.get('additions')} -{detail.get('deletions')} in "
        f"{detail.get('changed_files')} files",
        "",
        "Verdict: `dunkingsheep task complete <id> --verdict accept|needs-fix` from your",
        "reviewer lane, with the evidence in `task note` first: commands run, what was",
        "reproduced from the media, what was NOT verified. A verdict is bound to this",
        "exact head; a new push files a new review.",
    ]
    body = (detail.get("body") or "").strip()
    if body:
        lines += ["", "PR body:", body[:4000]]
    return "\n".join(lines)


class Sync:
    def __init__(self, flock, tokens=None, github=dunk_github):
        self.flock = flock
        self.tokens = tokens or dunk_github.Tokens()
        self.github = github

    def _live_reviews(self):
        tasks = self.flock.tasks.list(scope=None, include_done=False, limit=500,
                                      kind="review")
        by_pr = {}
        for task in tasks:
            if task["status"] in ("open", "in_progress", "blocked"):
                by_pr[(task.get("repo"), task.get("pr"))] = task
        return by_pr

    def run(self, dry_run=False):
        report = {"repos": [], "filed": [], "cancelled": [], "statuses": [],
                  "followups": [], "errors": []}
        try:
            token = self.tokens.get("reviewer")["token"]
        except dunk_github.GitHubError as error:
            report["errors"].append(f"reviewer token: {error}")
            return report
        lanes = self.flock.roles.describe().get("lanes") or {}
        mints = getattr(self.flock, "_gh_token_mints", {}) or {}
        live = self._live_reviews()
        seen = set()
        listed = set()       # repos GitHub answered for; only their closed PRs are cancelled
        for repo in self.github.repos_config():
            try:
                pulls = self.github.open_pulls(repo, token)
            except dunk_github.GitHubError as error:
                report["errors"].append(f"{repo}: {error}")
                continue
            listed.add(repo)
            report["repos"].append({"repo": repo, "open": len(pulls)})
            for pr in pulls:
                if pr["draft"] or not pr["head"]:
                    continue
                key = (repo, pr["number"])
                seen.add(key)
                current = live.get(key)
                if current and current.get("head") == pr["head"]:
                    continue
                if current:
                    if not dry_run:
                        self.flock.tasks.cancel(current["id"],
                                                reason=f"head moved to {pr['head'][:12]}; "
                                                       "delta review filed")
                    report["cancelled"].append(current["id"])
                author = attribute_author(pr, lanes, mints)
                try:
                    detail = self.github.pull_detail(repo, pr["number"], token)
                except dunk_github.GitHubError as error:
                    detail = {"body": "", "error": str(error)}
                title = (f"Review {repo.split('/')[-1]} PR #{pr['number']} "
                         f"{pr['head'][:8]}: {pr['title']}")[:120]
                if dry_run:
                    report["filed"].append({"repo": repo, "pr": pr["number"],
                                            "head": pr["head"], "author": author})
                    continue
                try:
                    task = self.flock.tasks.add(
                        "global", title, body=review_body(pr, detail, author),
                        priority="high", kind="review", author=author, repo=repo,
                        pr=pr["number"], head=pr["head"], created_by="review-sync")
                except Exception as error:  # noqa: BLE001 - one PR must not stop the sweep
                    report["errors"].append(f"{repo}#{pr['number']}: {error}")
                    continue
                report["filed"].append({"task": task["id"], "repo": repo,
                                        "pr": pr["number"], "head": pr["head"],
                                        "author": author})
                self._status(repo, pr["head"], "pending", token,
                             f"review task {task['id']} open", pr["url"], report)
        # PRs that closed or merged: their live review task is moot.
        for key, task in live.items():
            if key not in seen and key[0] in listed and not dry_run:
                self.flock.tasks.cancel(task["id"], reason="PR closed or merged on GitHub")
                report["cancelled"].append(task["id"])
        return report

    def _status(self, repo, sha, state, token, description, url, report):
        try:
            self.github.post_status(repo, sha, state, token, description, url)
            report["statuses"].append({"repo": repo, "head": sha[:12], "state": state})
        except dunk_github.GitHubError as error:
            # The reviewer App without `statuses: write` lands here: say so, keep going.
            report["errors"].append(f"status {repo}@{sha[:12]}: {error}")

    def on_verdict(self, task, verdict, by=None):
        """Called by the daemon when a review completes: status + approval + follow-up."""
        out = {"task": task["id"], "verdict": verdict, "status": None, "review": None,
               "followup": None, "errors": []}
        repo, number, head = task.get("repo"), task.get("pr"), task.get("head")
        if not (repo and number and head):
            return out
        try:
            token = self.tokens.get("reviewer")["token"]
        except dunk_github.GitHubError as error:
            out["errors"].append(str(error))
            return out
        if verdict == "accept":
            try:
                self.github.post_status(repo, head, "success", token,
                                        f"accepted by {by or 'reviewer'} ({task['id']})")
                out["status"] = "success"
            except dunk_github.GitHubError as error:
                out["errors"].append(f"status: {error}")
            try:
                self.github.review_pull(repo, number, "APPROVE",
                                        f"Accepted in {task['id']} by reviewer lane "
                                        f"{by or '?'} at head {head}.", token)
                out["review"] = "approved"
            except dunk_github.GitHubError as error:
                out["errors"].append(f"approve: {error}")
        else:
            try:
                self.github.post_status(repo, head, "failure", token,
                                        f"needs-fix per {task['id']}")
                out["status"] = "failure"
            except dunk_github.GitHubError as error:
                out["errors"].append(f"status: {error}")
            try:
                self.github.review_pull(repo, number, "REQUEST_CHANGES",
                                        f"Needs fix per {task['id']} (reviewer lane "
                                        f"{by or '?'}); see the task notes.", token)
                out["review"] = "changes_requested"
            except dunk_github.GitHubError as error:
                out["errors"].append(f"request-changes: {error}")
            try:
                followup = self.flock.tasks.add(
                    "global", f"Fix {repo.split('/')[-1]} PR #{number} per {task['id']}",
                    body=f"Reviewer verdict needs-fix on {repo}#{number} @ {head}.\n"
                         f"Read the notes on {task['id']}, push a fix to the same branch; "
                         "the review timer files a new review for the new head.",
                    priority="high", kind="work",
                    assignees=[task["author"]] if task.get("author") else None,
                    created_by="review-sync")
                out["followup"] = followup["id"]
            except Exception as error:  # noqa: BLE001
                out["errors"].append(f"followup: {error}")
        return out
