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

    def test_filed_ids_come_from_live_titles_only(self):
        tasks = [{"title": dunk_poolrad.task_title(row("OVL_0001_0001", 1)), "status": "open"},
                 {"title": dunk_poolrad.task_title(row("OVL_0001_0002", 2)), "status": "done"},
                 {"title": "unrelated", "status": "open"}]
        self.assertEqual({"OVL_0001_0001"}, dunk_poolrad.filed_ids_from_tasks(tasks))

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

    def test_unit_tokens_match_the_file_shapes(self):
        self.assertEqual({"006a_0824", "6a_0824"},
                         dunk_poolrad._unit_tokens("OVL_006A_0824"))
        self.assertEqual({"0038_0b1b", "38_0b1b"},
                         dunk_poolrad._unit_tokens("OVL_0038_0B1B"))
