"""Poolrad backlog-sync (FLAT_FLEET.md 6.5) against a fake exporter."""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_poolrad  # noqa: E402
from dunk_core import Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


def row(rid, rank, composable=True, classification="game_evidenced"):
    return {"id": rid, "rank": rank, "score": 50, "bytes": 100, "grade": "A", "unit": 1,
            "offset": 2, "ovr_offset": 3, "instructions": 40, "behaviour": "leaf",
            "classification": classification, "confidence": 0.9, "aliases": ["0x1:0x2"],
            "prior_translation": None,
            "callee_readiness": {"composable": composable, "ready": 2,
                                 "counts": {"strict": 2}}}


class PureTests(unittest.TestCase):
    def test_candidates_skip_credited_filed_uncomposable_and_helpers(self):
        queue = {"rows": [row("OVL_0001_0001", 3), row("OVL_0001_0002", 1),
                          row("OVL_0001_0003", 2, composable=False),
                          row("OVL_0001_0004", 4, classification="helper_candidate"),
                          row("OVL_0001_0005", 5)]}
        picked = dunk_poolrad.candidates(queue, credited={"OVL_0001_0002"},
                                         filed_ids={"OVL_0001_0005"}, limit=5)
        self.assertEqual(["OVL_0001_0001"], [r["id"] for r in picked])

    def test_a_filed_row_counts_in_every_status(self):
        """t857: this test used to assert the opposite - that a DONE filing does not
        count - which IS the defect: three merged-but-uncredited bodies were filed
        four times each, and my own retry refiled ten rows the same way.

        CANCELLED counts too, and that is a decision about the DECISION rather than
        about the body: a cancelled filing was withdrawn by a person, and a timer
        that re-files it undoes that cancel on the next tick - the resurrection shape
        (t854) from the other side. The asymmetry settles it: a person can re-file a
        body in one command, so counting cancelled costs nothing when its reason is
        resolved, while NOT counting it costs a human's decision every thirty
        minutes. (ds-worker-2 argued the other way first and conceded this reading.)
        """
        tasks = [{"title": dunk_poolrad.task_title(row("OVL_0001_0001", 1)), "status": "open"},
                 {"title": dunk_poolrad.task_title(row("OVL_0001_0002", 2)), "status": "done"},
                 {"title": dunk_poolrad.task_title(row("OVL_0001_0003", 3)),
                  "status": "cancelled"},
                 {"title": dunk_poolrad.task_title(row("OVL_0001_0004", 4)), "status": "blocked"},
                 {"title": "unrelated", "status": "open"}]
        self.assertEqual({"OVL_0001_0001", "OVL_0001_0002", "OVL_0001_0003",
                          "OVL_0001_0004"},
                         dunk_poolrad.filed_ids_from_tasks(tasks))

    def test_the_body_carries_a_machine_readable_filing_key(self):
        body = dunk_poolrad.task_body(row("OVL_0001_0001", 1), "abc123", "/repo")
        self.assertIn("filed-key: poolrad-strict:OVL_0001_0001@abc123", body)
        found = dunk_poolrad.filed_keys_from_tasks([{"body": body, "id": "t7",
                                                     "status": "done"}])
        self.assertEqual({"OVL_0001_0001": ("t7", "done")}, found)

    def test_the_key_wins_over_the_title_when_both_are_present(self):
        body = ("POOLRAD STRICT FULL-C BODY OVL_0001_0001\n"
                "filed-key: poolrad-strict:OVL_0001_0009@deadbeef\n")
        found = dunk_poolrad.filed_keys_from_tasks(
            [{"body": body, "id": "t8", "status": "open",
              "title": dunk_poolrad.task_title(row("OVL_0001_0001", 1))}])
        self.assertEqual(["OVL_0001_0009"], list(found))

    def test_body_carries_base_method_and_acceptance(self):
        body = dunk_poolrad.task_body(row("OVL_0001_0001", 1), "abc123", "/repo")
        for needle in ("base: abc123", "STRICT_CANDIDATE_QUEUE.md", "two reviewer-lane verdicts",
                       "NOT verified", "OVL_0001_0001"):
            self.assertIn(needle, body)


class SyncTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock = Flock(
            herdr=FakeHerdr(), state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False, restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "roles.json"),
                        tokens_path=os.path.join(self.tmp.name, "tokens.json")))
        self.queue = {"rows": [row(f"OVL_0001_000{i}", i) for i in range(1, 6)]}

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_sync_files_n_and_never_refiles(self):
        sync = dunk_poolrad.Sync(self.flock, repo=self.tmp.name, exporter=lambda repo: self.queue)
        first = sync.run(limit=2)
        self.assertEqual(2, len(first["filed"]))
        tasks = self.flock.tasks.list(kind="work", include_done=False)
        self.assertEqual(2, len(tasks))
        self.assertTrue(tasks[0]["title"].startswith("Poolrad strict C:"))
        self.assertEqual([], tasks[0]["assignees"], "queue-owned, claimable")
        second = sync.run(limit=2)
        self.assertEqual(["OVL_0001_0003", "OVL_0001_0004"], [f["id"] for f in second["filed"]])
        self.assertEqual(4, len(self.flock.tasks.list(kind="work", include_done=False)))

    def test_an_exporter_failure_files_nothing_and_says_why(self):
        def boom(repo):
            raise RuntimeError("exporter exploded")
        report = dunk_poolrad.Sync(self.flock, repo=self.tmp.name, exporter=boom).run()
        self.assertEqual([], report["filed"])
        self.assertIn("exporter exploded", report["errors"][0])

    def test_a_completed_filing_is_not_refiled(self):
        """t857: the 4x refile. A done task for the row must stop the next pass."""
        sync = dunk_poolrad.Sync(self.flock, repo=self.tmp.name, exporter=lambda repo: self.queue)
        first = sync.run(limit=2)
        self.assertEqual(2, len(first["filed"]))
        for entry in first["filed"]:
            self.flock.tasks.complete(entry["task"])
        again = sync.run(limit=2)
        self.assertEqual(["OVL_0001_0003", "OVL_0001_0004"],
                         [f["id"] for f in again["filed"]],
                         "a completed filing was refiled")
        self.assertEqual(2, len(again["skipped"]["already_filed_tasks"]))

    def test_a_retry_after_a_partial_failure_files_only_the_remainder(self):
        """The EAGAIN incident: the first pass dies mid-way, the retry must not
        refile what the first pass already did."""
        sync = dunk_poolrad.Sync(self.flock, repo=self.tmp.name, exporter=lambda repo: self.queue)
        sync.run(limit=2)                       # a partial pass: two rows filed
        rest = sync.run(limit=10)               # the retry
        self.assertEqual(["OVL_0001_0003", "OVL_0001_0004", "OVL_0001_0005"],
                         [f["id"] for f in rest["filed"]])
        self.assertEqual(5, len(self.flock.tasks.list(kind="work", include_done=True)),
                         "the retry created a duplicate")

    def test_dry_run_files_nothing(self):
        sync = dunk_poolrad.Sync(self.flock, repo=self.tmp.name, exporter=lambda repo: self.queue)
        report = sync.run(limit=3, dry_run=True)
        self.assertEqual(3, len(report["filed"]))
        self.assertEqual([], self.flock.tasks.list(kind="work", include_done=False))


if __name__ == "__main__":
    unittest.main()


class ClosedCandidateTests(unittest.TestCase):
    """t686: a row a reviewed disposition closed, or whose body is already on main,
    is not an authoring task. Both were filed twice today (t575/t576) and handed to a
    lane as "write the strict full-C body" - the exact claim this fleet records most.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = os.path.join(self.tmp.name, "repo")
        for path in ("docs/re", "tools/overlay", "src/recovered"):
            os.makedirs(os.path.join(self.repo, path), exist_ok=True)
        self.state = tempfile.TemporaryDirectory()
        self.flock = Flock(
            herdr=FakeHerdr(), persist=False, restore=False,
            state_file=os.path.join(self.state.name, "d.json"),
            inbox=Inbox(path=os.path.join(self.state.name, "i.db"),
                        blob_dir=os.path.join(self.state.name, "b")),
            tasks=Tasks(path=os.path.join(self.state.name, "t.db")),
            roles=Roles(path=os.path.join(self.state.name, "r.json"),
                        tokens_path=os.path.join(self.state.name, "k.json")))

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()
        self.state.cleanup()

    def write(self, relpath, text):
        path = os.path.join(self.repo, relpath)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)

    def test_a_disposition_document_closes_its_row(self):
        self.write("docs/re/OVL_0068_0294_DISPOSITION.md",
                   "Verdict: bounded model, no identity, no strict credit.\n")
        closed = dunk_poolrad.disposition_closures(self.repo)
        self.assertEqual({"OVL_0068_0294": "docs/re/OVL_0068_0294_DISPOSITION.md"}, closed)

    def test_a_body_and_checker_on_main_close_its_row(self):
        self.write("tools/overlay/ovl_006a_0824_cpu_audit.py",
                   "CHECKER_ROWS = ('OVL_006A_0824',)\n")
        self.write("src/recovered/overlay_u6a_0824_raw_cpu.c", "/* body */\n")
        checkers = dunk_poolrad.registered_checker_ids(self.repo)
        self.assertEqual("tools/overlay/ovl_006a_0824_cpu_audit.py",
                         checkers["OVL_006A_0824"])
        reason = dunk_poolrad.already_on_main(self.repo, "OVL_006A_0824",
                                              checkers=checkers)
        self.assertIn("already on main", reason)
        self.assertIn("overlay_u6a_0824_raw_cpu.c", reason)

    def test_a_checker_without_a_model_on_main_does_not_close_its_row(self):
        self.write("tools/overlay/ovl_006a_0824_cpu_audit.py",
                   "CHECKER_ROWS = ('OVL_006A_0824',)\n")
        self.assertIsNone(dunk_poolrad.already_on_main(self.repo, "OVL_006A_0824"))

    def test_candidates_never_return_a_closed_row(self):
        queue = {"rows": [row("OVL_0068_0294", 1), row("OVL_0001_0001", 2)]}
        picked = dunk_poolrad.candidates(
            queue, credited=set(), filed_ids=set(), limit=5,
            closed={"OVL_0068_0294": "closed by docs/re/OVL_0068_0294_DISPOSITION.md"})
        self.assertEqual(["OVL_0001_0001"], [r["id"] for r in picked])

    def test_a_dry_run_files_neither_row_and_names_both_reasons(self):
        """The acceptance line: over a queue holding both rows, nothing is filed for
        either, and the report says which rows it skipped and why."""
        self.write("docs/re/OVL_0068_0294_DISPOSITION.md", "no strict credit\n")
        self.write("tools/overlay/ovl_006a_0824_cpu_audit.py",
                   "CHECKER_ROWS = ('OVL_006A_0824',)\n")
        self.write("src/recovered/overlay_u6a_0824_raw_cpu.c", "/* body */\n")
        queue = {"rows": [row("OVL_0068_0294", 1), row("OVL_006A_0824", 2),
                          row("OVL_0001_0001", 3)]}
        sync = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                 exporter=lambda _repo: queue)
        report = sync.run(limit=6, dry_run=True)
        filed = [entry["id"] for entry in report["filed"]]
        self.assertNotIn("OVL_0068_0294", filed)
        self.assertNotIn("OVL_006A_0824", filed)
        self.assertEqual(["OVL_0001_0001"], filed, "the control row is still filed")
        closed = report["skipped"]["closed"]
        self.assertIn("DISPOSITION", closed["OVL_0068_0294"])
        self.assertIn("already on main", closed["OVL_006A_0824"])

    def test_the_model_is_found_whatever_the_filename_says_in_the_middle(self):
        """t862 (%100's measurement): the skip rule missed two of three bodies because
        it looked for an ADJACENT 'unit_offset' token, and these filenames carry a
        descriptive middle."""
        for rid, model in (("OVL_0038_0CC3", "overlay_u38_0cc3_cpu.c"),
                           ("OVL_0020_11AC", "overlay_u20_list_walk_11ac_cpu.c"),
                           ("OVL_03B0_06FB", "overlay_u3b0_row_clear_06fb_cpu.c")):
            self.write(f"tools/overlay/{rid.lower()}_audit.py",
                       f"CHECKER_ROWS = ('{rid}',)\n")
            self.write(f"src/recovered/{model}", "/* body */\n")
            reason = dunk_poolrad.already_on_main(self.repo, rid)
            self.assertIn("already on main", reason or "", f"{rid} ({model}) was missed")
            self.assertIn(model, reason or "")

    def test_a_same_unit_different_offset_is_not_a_match(self):
        """Segment equality, not substring: the unit alone must not close a row."""
        self.write("tools/overlay/ovl_0020_0001_audit.py",
                   "CHECKER_ROWS = ('OVL_0020_0001',)\n")
        self.write("src/recovered/overlay_u20_list_walk_11ac_cpu.c", "/* body */\n")
        self.assertIsNone(dunk_poolrad.already_on_main(self.repo, "OVL_0020_0001"))

class CreditPendingTests(unittest.TestCase):
    """t733: a body whose model is already on main is a missing LEDGER ROW.

    Measured 2026-10-29 (ds-worker-4): OVL_0038_0CC3 (merged in PR #1513),
    OVL_0020_11AC and OVL_03B0_06FB were filed as strict-C AUTHORING tasks while
    their model, header, checker and doc were already on main and only the credit
    was missing. The authoring task cannot be done; the owner needs a credit review
    against the PR head that added the model instead.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = os.path.join(self.tmp.name, "repo")
        for path in ("docs/re", "tools/overlay", "src/recovered"):
            os.makedirs(os.path.join(self.repo, path), exist_ok=True)
        self.git("-c", "init.defaultBranch=main", "init", cwd=self.repo)
        self.git("config", "user.email", "t@example.com", cwd=self.repo)
        self.git("config", "user.name", "t", cwd=self.repo)
        self.state = tempfile.TemporaryDirectory()
        self.flock = Flock(
            herdr=FakeHerdr(), persist=False, restore=False,
            state_file=os.path.join(self.state.name, "d.json"),
            inbox=Inbox(path=os.path.join(self.state.name, "i.db"),
                        blob_dir=os.path.join(self.state.name, "b")),
            tasks=Tasks(path=os.path.join(self.state.name, "t.db")),
            roles=Roles(path=os.path.join(self.state.name, "r.json"),
                        tokens_path=os.path.join(self.state.name, "k.json")))
        self.pending_path = os.path.join(self.state.name, "credit-pending.json")

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()
        self.state.cleanup()

    def git(self, *args, cwd=None):
        import subprocess
        out = subprocess.run(["git", "-C", cwd or self.repo, *args],
                             capture_output=True, text=True, timeout=30)
        self.assertEqual(0, out.returncode, out.stderr)
        return out.stdout.strip()

    def _initial_commit(self):
        try:
            self.git("rev-parse", "--verify", "HEAD", cwd=self.repo)
        except AssertionError:
            with open(os.path.join(self.repo, "README.md"), "w", encoding="utf-8") as h:
                h.write("repo\n")
            self.git("add", "-A", cwd=self.repo)
            self.git("commit", "-q", "-m", "init", cwd=self.repo)

    def commit_model(self, name, pr=None):
        """Add a model file on a side branch and merge it as PR `pr` if given."""
        self.git("checkout", "-q", "-B", "main", cwd=self.repo)
        self._initial_commit()
        relative = os.path.join("src", "recovered", name)
        branch = f"side-{pr}" if pr is not None else "main"
        if pr is not None:
            self.git("checkout", "-q", "-b", branch, "main", cwd=self.repo)
        with open(os.path.join(self.repo, relative), "w", encoding="utf-8") as handle:
            handle.write("/* model */\n")
        self.git("add", "-A", cwd=self.repo)
        self.git("commit", "-q", "-m", "add model", cwd=self.repo)
        if pr is None:
            return self.git("rev-parse", "HEAD", cwd=self.repo)
        self.git("checkout", "-q", "main", cwd=self.repo)
        self.git("merge", "-q", "--no-ff", "-m",
                 f"Merge pull request #{pr} from x/y", branch, cwd=self.repo)
        return self.git("rev-parse", "HEAD^2", cwd=self.repo)

    # -- filename matching ---------------------------------------------------

    def test_the_label_between_unit_and_offset_still_matches(self):
        self.assertTrue(dunk_poolrad._model_name_matches(
            "overlay_u20_list_walk_11ac_cpu.c", "OVL_0020_11AC"))
        self.assertTrue(dunk_poolrad._model_name_matches(
            "overlay_u3b0_row_clear_06fb_cpu.c", "OVL_03B0_06FB"))

    def test_each_half_must_land_on_a_token_boundary(self):
        self.assertFalse(dunk_poolrad._model_name_matches(
            "overlay_u200_11ac.c", "OVL_0020_11AC"), "u20 must not match u200")
        self.assertFalse(dunk_poolrad._model_name_matches(
            "overlay_u20_11ac0.c", "OVL_0020_11AC"), "11ac must not match 11ac0")
        self.assertFalse(dunk_poolrad._model_name_matches(
            "overlay_u20_other.c", "OVL_0020_11AC"))

    # -- evidence from the repo ---------------------------------------------

    def test_the_evidence_names_the_file_the_commit_and_the_pr(self):
        commit = self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        evidence = dunk_poolrad.model_evidence(self.repo, "OVL_0020_11AC")
        self.assertEqual("src/recovered/overlay_u20_list_walk_11ac_cpu.c",
                         evidence["model"])
        self.assertEqual(commit, evidence["commit"])
        self.assertEqual(1433, evidence["pr"])

    def test_a_row_with_no_model_on_main_has_no_evidence(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.assertIsNone(dunk_poolrad.model_evidence(self.repo, "OVL_0001_0002"))

    def test_a_commit_that_reached_main_without_a_pr_says_so(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c")
        evidence = dunk_poolrad.model_evidence(self.repo, "OVL_0020_11AC")
        self.assertIsNone(evidence["pr"], "no merge commit, no PR number invented")

    # -- the sync ------------------------------------------------------------

    def test_the_sync_files_a_credit_review_candidate_instead_of_an_authoring_task(self):
        commit = self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        queue = {"rows": [row("OVL_0020_11AC", 1), row("OVL_0001_0002", 2)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda repo: queue).run(limit=5)
        self.assertEqual(["OVL_0001_0002"], [f["id"] for f in report["filed"]],
                         "a body already on main must not be filed as authoring work")
        self.assertEqual(commit, report["credit_pending"]["OVL_0020_11AC"]["commit"])
        self.assertEqual(1433, report["credit_pending"]["OVL_0020_11AC"]["pr"])
        self.assertIn("ledger credit pending", report["skipped"]["closed"]["OVL_0020_11AC"])

    def test_the_credit_pending_file_is_written_once_and_keeps_the_first_evidence(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        sync = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                 exporter=lambda repo: queue,
                                 credit_path=self.pending_path)
        report = sync.run(limit=5)
        self.assertEqual(str(self.pending_path), report["credit_pending_file"])
        bodies = dunk_poolrad.credit_pending(self.pending_path)
        self.assertEqual(1433, bodies["OVL_0020_11AC"]["pr"])
        self.assertEqual(0o600, os.stat(self.pending_path).st_mode & 0o777)
        # A later run re-derives the same row; the first-seen evidence must stand.
        osmtime = os.stat(self.pending_path).st_mtime
        second = dunk_poolrad.record_credit_pending(
            {"OVL_0020_11AC": {"model": "other.c", "commit": "def", "pr": 8},
             "OVL_0001_0002": {"model": "y.c", "commit": "ghi", "pr": 9}},
            path=self.pending_path)
        self.assertEqual(2, second["count"])
        bodies = dunk_poolrad.credit_pending(self.pending_path)
        self.assertEqual(1433, bodies["OVL_0020_11AC"]["pr"],
                         "the first evidence is kept, not overwritten")
        self.assertGreaterEqual(os.stat(self.pending_path).st_mtime, osmtime)

    def test_progress_reports_the_recorded_count(self):
        dunk_poolrad.record_credit_pending(
            {"OVL_0001_0001": {"model": "m.c", "commit": "c", "pr": 1}},
            path=self.pending_path)
        summary = dunk_poolrad.progress(self.repo, credit_path=self.pending_path)
        self.assertEqual(1, summary["credit_pending"])

    def test_a_dry_run_records_nothing(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda repo: queue,
                                   credit_path=self.pending_path).run(limit=5,
                                                                      dry_run=True)
        self.assertEqual([], report["filed"])
        self.assertIn("OVL_0020_11AC", report["credit_pending"])
        self.assertFalse(os.path.exists(self.pending_path),
                         "a dry run writes no owner-facing file")

    def test_a_missing_file_reads_as_empty(self):
        self.assertEqual({}, dunk_poolrad.credit_pending(self.pending_path))
        with open(self.pending_path, "w", encoding="utf-8") as handle:
            handle.write("{broken")
        self.assertEqual({}, dunk_poolrad.credit_pending(self.pending_path))

    # -- credit readiness (review of #149, t738) ------------------------------

    def write_partial(self, rows):
        """docs/re/partial_callee_models.csv rows: (id, status)."""
        body = "id,status,evidence_doc\n" + "".join(
            f"{rid},{status},{rid}.md\n" for rid, status in rows)
        with open(os.path.join(self.repo, "docs", "re",
                               "partial_callee_models.csv"), "w",
                  encoding="utf-8") as handle:
            handle.write(body)

    def write_checker(self, rid):
        """A tools/overlay checker declaring CHECKER_ROWS for `rid` (t686)."""
        with open(os.path.join(self.repo, "tools", "overlay", "check_x.py"), "w",
                  encoding="utf-8") as handle:
            handle.write(f'CHECKER_ROWS = ["{rid}"]\n')

    def test_a_filename_alone_does_not_make_a_body_credit_ready(self):
        # The reviewer's finding: 49 of 74 name-matches on a live checkout had no
        # registered checker, so "model on main" was read as "credit-ready".
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        ready, why_not = dunk_poolrad.credit_ready(self.repo, "OVL_0020_11AC")
        self.assertFalse(ready)
        self.assertIn("no registered checker", why_not)

    def test_a_model_with_a_checker_is_credit_ready(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.assertEqual((True, None),
                         dunk_poolrad.credit_ready(self.repo, "OVL_0020_11AC"))

    def test_a_partial_model_is_never_credited(self):
        # 4 rows are partial on main and all four model files exist; crediting one
        # would put a partial model in the strict ledger.
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.write_partial([("OVL_0020_11AC", "partial")])
        ready, why_not = dunk_poolrad.credit_ready(self.repo, "OVL_0020_11AC")
        self.assertFalse(ready)
        self.assertIn("partial", why_not)

    def test_a_callback_model_is_never_credited(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.write_partial([("OVL_0020_11AC", "callback")])
        self.assertFalse(dunk_poolrad.credit_ready(self.repo, "OVL_0020_11AC")[0])

    def test_a_full_status_with_a_checker_is_credit_ready(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.write_partial([("OVL_0020_11AC", "full")])
        self.assertEqual((True, None),
                         dunk_poolrad.credit_ready(self.repo, "OVL_0020_11AC"))

    def test_a_partial_model_is_not_closed_by_its_checker(self):
        """%100's composition warning: a partial body keeps its authoring work even
        when a registered checker audits a file on main. Before the guard, the partial
        case survived only because the token bug made the skip return None."""
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.write_partial([("OVL_0020_11AC", "partial")])
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda _repo: queue,
                                   credit_path=self.pending_path).run(limit=5)
        self.assertEqual(["OVL_0020_11AC"], [f["id"] for f in report["filed"]],
                         "a partial model is still an authoring candidate")
        self.assertNotIn("OVL_0020_11AC", report["skipped"]["closed"],
                         "the checker closed a body whose model is only partial")
        self.assertEqual({}, report["credit_pending"])

    def test_a_partial_body_stays_an_authoring_task(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        self.write_partial([("OVL_0020_11AC", "partial")])
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda repo: queue,
                                   credit_path=self.pending_path).run(limit=5)
        self.assertEqual(["OVL_0020_11AC"], [f["id"] for f in report["filed"]],
                         "a partial model is still an authoring candidate")
        self.assertEqual({}, report["credit_pending"])
        self.assertIn("partial",
                      report["models_not_credit_ready"]["OVL_0020_11AC"]["why_not"])
        self.assertFalse(os.path.exists(self.pending_path))

    def test_a_checkerless_body_stays_an_authoring_task(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda repo: queue,
                                   credit_path=self.pending_path).run(limit=5)
        self.assertEqual(["OVL_0020_11AC"], [f["id"] for f in report["filed"]])
        self.assertEqual({}, report["credit_pending"])
        self.assertIn("no registered checker",
                      report["models_not_credit_ready"]["OVL_0020_11AC"]["why_not"])

    def test_a_credit_ready_body_is_not_filed_and_is_recorded(self):
        self.commit_model("overlay_u20_list_walk_11ac_cpu.c", pr=1433)
        self.write_checker("OVL_0020_11AC")
        queue = {"rows": [row("OVL_0020_11AC", 1)]}
        report = dunk_poolrad.Sync(self.flock, repo=self.repo,
                                   exporter=lambda repo: queue,
                                   credit_path=self.pending_path).run(limit=5)
        self.assertEqual([], report["filed"])
        self.assertIn("credit-ready", report["skipped"]["closed"]["OVL_0020_11AC"])
        self.assertEqual(1, len(dunk_poolrad.credit_pending(self.pending_path)))
