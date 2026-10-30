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

import bisect
import logging
import re
import time

import dunk_github

log = logging.getLogger("dunkingsheep.review")

# FLAT_FLEET.md 6.4: acceptance is a commit. In poolrad-decomp the commit is the
# ledger row that credits a reviewed body, and it is the only write the reviewer
# App ever makes. It touches one file.
ACCOUNTING_LEDGERS = {
    "huntergdavis/poolrad-decomp": {
        "ledger": "docs/re/strict_full_c_addresses.csv",
        "base": "main",
        "docs_dir": "docs/re/",
    },
}

# The poolrad row-id convention, taken from tools/overlay/check_body_doc_pairing.py
# in that repo: `OVL_<unit>_<offset>` / `RES_<unit>_<offset>`, and a body's evidence
# document carries the offset as an underscore token plus the unit as `u<unit>`.
ROW_ID = re.compile(r"^(?P<kind>OVL|RES)_(?P<unit>[0-9A-F]{4})_(?P<off>[0-9A-F]{4})$")
ROW_TOKEN = re.compile(
    r"(?<![A-Za-z0-9_])(?:OVL|RES)_[0-9A-F]{4}_[0-9A-F]{4}(?![A-Za-z0-9_])")


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


def row_ids(text):
    """Every full row id named in `text`, in order of first appearance, deduped."""
    return list(dict.fromkeys(ROW_TOKEN.findall(text or "")))


def stem_of(path):
    return path.rsplit("/", 1)[-1].rsplit(".", 1)[0]


def own_row(row, stem):
    """True when a row id is this document's by name: the stem carries the offset as an
    underscore token and the unit as `u<unit>` or the four-digit unit (poolrad convention:
    U7F_STATE_FLAGS_0F7E.md <- OVL_007F_0F7E)."""
    match = ROW_ID.match(row or "")
    parts = stem.lower().split("_")
    if not match or match.group("off").lower() not in parts:
        return False
    unit = match.group("unit").lower()
    return "u" + (unit.lstrip("0") or "0") in parts or unit in parts


def ledger_rows(text):
    """(id, evidence_doc) pairs from a `id,evidence_doc` ledger, header skipped."""
    rows = []
    for line in (text or "").splitlines()[1:]:
        if not line.strip() or "," not in line:
            continue
        ident, doc = line.split(",", 1)
        rows.append((ident.strip(), doc.strip()))
    return rows


def with_row(text, ident, doc):
    """The ledger with one row inserted in sorted position.

    Line endings are kept as they are: a CRLF ledger written back as LF would turn a
    one-line credit into a 290-line diff, which is exactly the kind of noise that
    hides a real change in review.
    """
    text = text or ""
    lines = text.splitlines()
    if not lines:
        raise ValueError("the ledger is empty: no header line to keep")
    sep = "\r\n" if "\r\n" in text else "\n"
    header, body = lines[0], lines[1:]
    keys = [line.split(",", 1)[0] for line in body]
    body.insert(bisect.bisect_left(keys, ident), f"{ident},{doc}")
    return sep.join([header] + body) + (sep if text.endswith(("\n", "\r")) else "")


def choose_row(sources, added_docs, ledger_text):
    """(row, reason): the one ledger row an accepted body earns, or why there is none.

    `sources` are the texts the verdict and the accepted PR offer, in the order the
    reviewer is expected to have written them (verdict notes, PR title, PR body). The
    first source that names exactly one row id wins; a source naming several is
    ambiguous and refuses, because a row that guesses is a credit nobody reviewed.
    """
    ident = None
    for label, text in sources:
        ids = row_ids(text)
        if len(ids) == 1:
            ident = ids[0]
            break
        if len(ids) > 1:
            return None, (f"the {label} names {len(ids)} row ids "
                          f"({', '.join(ids)}); one ledger row is one id")
    if ident is None:
        return None, ("no row id (OVL_xxxx_yyyy / RES_xxxx_yyyy) is named in the verdict "
                      "notes, the review task title or the PR body")
    existing = dict(ledger_rows(ledger_text))
    if ident in existing:
        return None, f"{ident} is already credited as {existing[ident]}"
    matched = [doc for doc in added_docs if own_row(ident, stem_of(doc))]
    if not matched:
        return None, (f"{ident}: no evidence document the PR adds carries its unit and "
                      f"offset (added: {', '.join(added_docs) or 'none'})")
    if len(matched) > 1:
        return None, (f"{ident}: {len(matched)} added documents match it "
                      f"({', '.join(sorted(matched))}); refusing to pick one")
    return (ident, matched[0]), ""


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
        tasks = self.flock.tasks.list(scope=None, include_done=True, limit=1000,
                                      kind="review")
        by_pr = {}
        for task in tasks:
            if task["status"] in ("open", "in_progress", "blocked"):
                by_pr[(task.get("repo"), task.get("pr"))] = task
        return by_pr

    def _judged_heads(self):
        """(repo, pr, head) -> verdict for every DONE review: a head that was
        judged is never re-filed (2026-10-29: an accepted PR got a second review
        task the next tick, whose pending status overwrote the success)."""
        tasks = self.flock.tasks.list(scope=None, include_done=True, limit=1000,
                                      kind="review")
        out = {}
        for task in tasks:
            if task["status"] == "done" and task.get("verdict"):
                out[(task.get("repo"), task.get("pr"), task.get("head"))] = task
        return out

    def head_verdicts(self, repo, head, number=None):
        """The verdicts recorded at one head, by DISTINCT lane (t662).

        GitHub cannot tell two lanes apart - every lane approves as the same App - so
        the count that decides whether a head is final has to come from the task store,
        where `completed_by` is the lane. A lane that accepted twice is one lane; a
        needs-fix from any lane stops the head even if others accept it.
        """
        tasks = self.flock.tasks.list(scope=None, include_done=True, limit=1000,
                                      kind="review")
        accepts, needs_fix = set(), set()
        for task in tasks:
            if task.get("repo") != repo or task.get("head") != head:
                continue
            if number is not None and task.get("pr") != number:
                continue
            if task["status"] != "done":
                continue
            lane = task.get("completed_by") or None
            if task.get("verdict") == "accept" and lane:
                accepts.add(str(lane))
            elif task.get("verdict") == "needs-fix" and lane:
                needs_fix.add(str(lane))
        return sorted(accepts), sorted(needs_fix)

    def _is_final(self, repo, head, number=None, lane=None):
        """Is this head final: enough DISTINCT accepting lanes and no needs-fix?"""
        accepts, needs_fix = self.head_verdicts(repo, head, number)
        if lane and str(lane) not in accepts:
            accepts = sorted(set(accepts) | {str(lane)})
        required = dunk_github.required_verdicts(repo)
        return (not needs_fix) and len(accepts) >= required, accepts, needs_fix, required

    def _second_review(self, repo, number, head, first_lane, report=None):
        """File the second review for a head whose first verdict was an accept (t662).

        `author` is the lane that already accepted, and the claim filter excludes an
        author from its own review, so the next reviewer cannot be the same lane. The
        partial unique index allows it because the first review task is done.
        """
        title = (f"Review {repo.split('/')[-1]} PR #{number} {head[:8]} "
                 f"(second reviewer: {first_lane} accepted)")[:120]
        body = (f"SECOND REVIEW {repo}#{number} @ {head}\n"
                f"Lane {first_lane} already accepted this exact head. "
                f"{dunk_github.required_verdicts(repo)} distinct reviewer lanes are "
                "required, so this task exists for a DIFFERENT lane; the claim filter "
                f"keeps {first_lane} off it.\n\n"
                "Verdict: `dunkingsheep task complete <id> --verdict accept|needs-fix` "
                "with the evidence in `task note` first.")
        try:
            task = self.flock.tasks.add("global", title, body=body, priority="high",
                                        kind="review", author=first_lane, repo=repo,
                                        pr=number, head=head, created_by="review-sync")
        except Exception as error:  # noqa: BLE001
            if report is not None:
                report["errors"].append(f"second review {repo}#{number}: {error}")
            return None
        return task["id"]

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
        judged = self._judged_heads()
        report["merged"] = []
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
                done = judged.get((repo, pr["number"], pr["head"]))
                if done is not None:
                    # Already judged at this exact head. Accepted and still open:
                    # protection may now be satisfied, so try the merge again - but
                    # only when the DISTINCT-lane count is met (t662), and never when
                    # any lane said needs-fix.
                    if done.get("verdict") == "accept" and not dry_run:
                        final, accepts, needs_fix, required = self._is_final(
                            repo, pr["head"], pr["number"])
                        if final:
                            self._merge(repo, pr["number"], pr["head"], token, report,
                                        done["id"])
                        elif not live.get(key):
                            # 1 of 2 accepted and nobody is reviewing: keep a second
                            # review in the queue rather than waiting forever.
                            self._second_review(repo, pr["number"], pr["head"],
                                                accepts[0] if accepts else None, report)
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

    def _merge(self, repo, number, head, token, report, task_id=None):
        try:
            self.github.merge_pull(repo, number, token, sha=head)
            report["merged"].append({"repo": repo, "pr": number, "head": head[:12],
                                     "task": task_id})
            return True
        except dunk_github.GitHubError as error:
            # 405 = protection not yet satisfied (approval count, other checks):
            # not an error, the next tick retries. Anything else is reported.
            if "405" not in str(error):
                report["errors"].append(f"merge {repo}#{number}: {error}")
            return False

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
               "followup": None, "accounting": None, "errors": []}
        repo, number, head = task.get("repo"), task.get("pr"), task.get("head")
        if not (repo and number and head):
            return out
        try:
            token = self.tokens.get("reviewer")["token"]
        except dunk_github.GitHubError as error:
            out["errors"].append(str(error))
            return out
        if verdict == "accept":
            # t662: a head is final only when ENOUGH DISTINCT lanes accepted it. The
            # first accept posts pending and files the second review; only the verdict
            # that meets the count approves and merges. A needs-fix anywhere stops it.
            final, accepts, needs_fix, required = self._is_final(
                repo, head, number, lane=by)
            out["accepts"], out["required"] = accepts, required
            if needs_fix and not final:
                try:
                    self.github.post_status(
                        repo, head, "failure", token,
                        f"needs-fix stands from {', '.join(needs_fix)}")
                    out["status"] = "failure"
                except dunk_github.GitHubError as error:
                    out["errors"].append(f"status: {error}")
                return out
            if not final:
                try:
                    self.github.post_status(
                        repo, head, "pending", token,
                        f"accepted by {len(accepts)} of {required} distinct reviewer "
                        f"lanes ({', '.join(accepts)}) - second review filed")
                    out["status"] = "pending"
                except dunk_github.GitHubError as error:
                    out["errors"].append(f"status: {error}")
                out["second_review"] = self._second_review(repo, number, head, by)
                out["merged"] = False
                return out
            try:
                self.github.post_status(repo, head, "success", token,
                                        f"accepted by {len(accepts)} distinct reviewer "
                                        f"lanes ({', '.join(accepts)})")
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
            # Merge now if protection is satisfied (one approval repos); poolrad
            # needs a second verdict first and the sync retries every tick.
            try:
                self.github.merge_pull(repo, number, token, sha=head)
                out["merged"] = True
            except dunk_github.GitHubError as error:
                out["merged"] = False
                if "405" not in str(error):
                    out["errors"].append(f"merge: {error}")
            # FLAT_FLEET.md 6.4: an accepted body earns its ledger row as its own
            # PR, opened by the reviewer App and approved by the author App.
            try:
                out["accounting"] = self.account(task, by=by)
            except Exception as error:  # noqa: BLE001 - never undoes a verdict
                out["accounting"] = {"errors": [str(error)]}
        else:
            evidence = self._evidence(task)
            standing = self._standing_accept(task)
            if standing is not None:
                # A failure status here would overwrite the success that branch
                # protection reads, on a head that another review already
                # accepted (t656). The verdict is recorded on its own task; the
                # head's status is not this verdict's to flip.
                out["status"] = "skipped_standing_accept"
                out["standing_accept"] = standing
                out["errors"].append(
                    f"failure status not posted: {repo}#{number} @ {head[:12]} already "
                    f"carries an accept verdict from {standing}")
            else:
                try:
                    self.github.post_status(repo, head, "failure", token,
                                            f"needs-fix per {task['id']}")
                    out["status"] = "failure"
                except dunk_github.GitHubError as error:
                    out["errors"].append(f"status: {error}")
            try:
                # The evidence goes in the review body, not a pointer to it: the
                # body that said "see the task notes" on 2026-10-29 pointed at
                # nothing, and the fix tasks it created could not be worked (t621).
                self.github.review_pull(
                    repo, number, "REQUEST_CHANGES",
                    self._review_body(task, by, head, evidence), token)
                out["review"] = "changes_requested"
            except dunk_github.GitHubError as error:
                out["errors"].append(f"request-changes: {error}")
            try:
                followup = self.flock.tasks.add(
                    "global", f"Fix {repo.split('/')[-1]} PR #{number} per {task['id']}",
                    body=(f"Reviewer verdict needs-fix on {repo}#{number} @ {head}.\n\n"
                          f"The reviewer's evidence, verbatim:\n\n{evidence}\n\n"
                          "Push a fix to the same branch; the review timer files a new "
                          "review for the new head."),
                    priority="high", kind="work",
                    assignees=[task["author"]] if task.get("author") else None,
                    created_by="review-sync")
                out["followup"] = followup["id"]
            except Exception as error:  # noqa: BLE001
                out["errors"].append(f"followup: {error}")
        return out

    # -- FLAT_FLEET.md 6.4: acceptance is a commit ---------------------------

    def _verdict_text(self, task):
        """The reviewer's own words: every note on the review task, oldest first."""
        try:
            notes = self.flock.tasks.notes(task["id"])
        except Exception:  # noqa: BLE001 - a store without notes is not a failure
            return ""
        return "\n".join(note["text"] for note in notes if note.get("text"))

    def account(self, task, by=None, dry_run=False):
        """Open, approve and queue the accounting PR for an accepted poolrad body.

        Deterministic from two artefacts and nothing else: the verdict (the reviewer's
        notes) and the accepted PR (the evidence documents it adds). The reviewed
        content stays in the body PR; this PR only adds the ledger row that credits it.
        Every refusal is a sentence on the review task, never a guess.
        """
        repo, number = task.get("repo"), task.get("pr")
        out = {"repo": repo, "body_pr": number, "ledger": None, "row": None,
               "accounting_pr": None, "url": None, "review": None,
               "auto_merge": None, "state": None, "reason": None, "errors": []}
        config = ACCOUNTING_LEDGERS.get(repo or "")
        if not config:
            out["reason"] = f"no accounting ledger is configured for {repo or 'this repo'}"
            return out
        ledger, base, docs_dir = config["ledger"], config["base"], config["docs_dir"]
        out["ledger"] = ledger
        try:
            reviewer = self.tokens.get("reviewer")["token"]
            author = self.tokens.get("author")["token"]
        except dunk_github.GitHubError as error:
            out["errors"].append(f"tokens: {error}")
            return out
        try:
            base_sha = self.github.get_ref(repo, base, reviewer)
            ledger_text, _blob = self.github.file_text(repo, ledger, base_sha, reviewer)
        except dunk_github.GitHubError as error:
            out["errors"].append(f"read {ledger}@{base}: {error}")
            return out
        if ledger_text is None:
            out["errors"].append(f"{repo}/{ledger} does not exist on {base}")
            return out
        try:
            files = self.github.pr_files(repo, number, reviewer)
        except dunk_github.GitHubError as error:
            out["errors"].append(f"list PR #{number} files: {error}")
            return out
        added = [item["filename"] for item in files
                 if item.get("status") == "added"
                 and item.get("filename", "").startswith(docs_dir)
                 and item["filename"].endswith(".md")]
        try:
            detail = self.github.pull_detail(repo, number, reviewer)
        except dunk_github.GitHubError:
            detail = {}
        row, reason = choose_row(
            [("verdict notes", self._verdict_text(task)),
             ("review task title", task.get("title") or ""),
             ("PR body", detail.get("body") or "")],
            added, ledger_text)
        if row is None:
            out["reason"] = reason
            self._record_accounting(task, out)
            return out
        ident, doc = row
        out["row"] = f"{ident},{doc}"
        branch = f"accounting/pr{number}-{ident}"
        try:
            if self.github.ref_exists(repo, branch, reviewer):
                open_prs = self.github.pulls_for_head(repo, branch, reviewer)
                if open_prs:
                    out["accounting_pr"] = open_prs[0]["number"]
                    out["url"] = open_prs[0]["url"]
                    out["reason"] = f"an accounting PR for {ident} is already open"
                    self._record_accounting(task, out)
                    return out
                out["reason"] = (f"branch {branch} exists without an open PR; refusing to "
                                 "overwrite a commit nobody reviewed")
                self._record_accounting(task, out)
                return out
        except dunk_github.GitHubError as error:
            out["errors"].append(f"look up {branch}: {error}")
            return out
        new_text = with_row(ledger_text, ident, doc)
        message = (f"Accept {ident}: accounting row for #{number}\n\n"
                   f"Derived from review {task['id']} (verdict accept by {by or 'reviewer'}) "
                   f"and PR #{number} at head {(task.get('head') or '')[:12]}.\n"
                   f"The reviewed content is PR #{number}; this commit only adds "
                   f"{ident},{doc} to {ledger}.\n")
        if dry_run:
            out["reason"] = "dry run: no commit, no PR"
            return out
        try:
            tree = self.github.create_tree(
                repo, self.github.commit_tree(repo, base_sha, reviewer),
                [{"path": ledger, "mode": "100644", "type": "blob",
                  "content": new_text}], reviewer)
            commit = self.github.create_commit(repo, message, tree, [base_sha], reviewer)
            self.github.create_ref(repo, branch, commit, reviewer)
        except dunk_github.GitHubError as error:
            out["errors"].append(f"commit the ledger row: {error}")
            self._record_accounting(task, out)
            return out
        try:
            opened = self.github.open_pull(
                repo, f"Accept {ident}: accounting row for #{number}", branch, base,
                self._accounting_body(task, ident, doc, ledger, number, by), reviewer)
        except dunk_github.GitHubError as error:
            out["errors"].append(f"open the accounting PR: {error}")
            self._record_accounting(task, out)
            return out
        out["accounting_pr"] = opened["number"]
        out["url"] = opened["url"]
        # Two distinct bot identities: the reviewer App wrote it, so only the author
        # App can approve it. GitHub refuses an App approving its own PR.
        try:
            self.github.review_pull(
                repo, opened["number"], "APPROVE",
                f"Accounting row derived from review {task['id']} and the accepted "
                f"content in #{number} at {(task.get('head') or '')[:12]}.", author)
            out["review"] = "approved_by_author_app"
        except dunk_github.GitHubError as error:
            out["errors"].append(f"approve as the author App: {error}")
        try:
            self.github.enable_auto_merge(repo, opened["number"], author)
            out["auto_merge"] = "enabled"
        except dunk_github.GitHubError as error:
            out["errors"].append(f"enable auto-merge: {error}")
        try:
            out["state"] = self.github.pull_state(repo, opened["number"], author)
        except dunk_github.GitHubError as error:
            out["errors"].append(f"read the accounting PR state: {error}")
        self._record_accounting(task, out)
        return out

    def _accounting_body(self, task, ident, doc, ledger, number, by):
        return "\n".join([
            f"Accounting row for the accepted body {ident}.",
            "",
            f"- body PR (the reviewed content): #{number}",
            f"- review task: {task['id']}, verdict `accept` by {by or 'reviewer'}",
            f"- head reviewed: {(task.get('head') or '')}",
            f"- ledger: `{ledger}`",
            f"- row added: `{ident},{doc}`",
            "",
            "Derived deterministically from the verdict notes and the files the body PR",
            "adds: one row id, one evidence document that carries its unit and offset. No",
            "model decided anything here. The reviewed content is the body PR; this PR only",
            "records the credit.",
            "",
            "Opened by the review timer as `dunkingsheep-reviewer[bot]` and approved as",
            "`dunkingsheep-author[bot]` so the two identities GitHub counts are distinct.",
            "Auto-merge is enabled; branch protection decides when it lands.",
        ])

    def _record_accounting(self, task, out):
        """Put both PR numbers on the review task, and the reason when there is no PR."""
        try:
            if out.get("accounting_pr"):
                self.flock.tasks.set_accounting_pr(task["id"], out["accounting_pr"])
            summary = (f"accounting PR #{out['accounting_pr']} for body PR "
                       f"#{out.get('body_pr')}" if out.get("accounting_pr") else
                       f"no accounting PR for body PR #{out.get('body_pr')}: "
                       f"{out.get('reason') or '; '.join(out.get('errors') or [])}")
            self.flock.tasks.note(task["id"], event="accounting", actor="review-sync",
                                  meta={"reason": summary,
                                        "accounting_pr": out.get("accounting_pr"),
                                        "body_pr": out.get("body_pr"),
                                        "row": out.get("row"),
                                        "url": out.get("url"),
                                        "state": out.get("state"),
                                        "errors": out.get("errors") or []})
        except Exception as error:  # noqa: BLE001 - recording never undoes a verdict
            log.warning("could not record accounting for %s: %s", task.get("id"), error)

    def _standing_accept(self, task):
        """The task id of another DONE review that accepted this exact head, or None."""
        repo, number, head = task.get("repo"), task.get("pr"), task.get("head")
        if not (repo and number and head):
            return None
        try:
            tasks = self.flock.tasks.list(scope=None, include_done=True, limit=1000,
                                          kind="review")
        except Exception:  # noqa: BLE001 - a store read must not stop a verdict
            return None
        for other in tasks:
            if (other["id"] != task["id"] and other.get("status") == "done"
                    and other.get("verdict") == "accept"
                    and (other.get("repo"), other.get("pr"), other.get("head"))
                    == (repo, number, head)):
                return other["id"]
        return None

    def _evidence(self, task):
        """The reviewer's own notes, verbatim, or a sentence saying there are none.

        `task complete --verdict` refuses a review with no note (t621), so an empty
        result here means an older verdict or an owner-completed one; it is
        reported as empty rather than silently omitted.
        """
        try:
            notes = self.flock.tasks.notes(task["id"])
        except Exception:  # noqa: BLE001 - a store without notes is not a failure
            return "(no note store)"
        text = "\n\n".join(note["text"].strip() for note in notes
                            if note.get("text", "").strip())
        return text or "(no evidence was recorded on the review task)"

    def _review_body(self, task, by, head, evidence):
        return "\n".join([
            f"Needs fix per {task['id']} (reviewer lane {by or '?'}) at head {head}.",
            "",
            "The reviewer's evidence, verbatim:",
            "",
            evidence,
            "",
            "A verdict is bound to this head: a new push files a new review.",
        ])
