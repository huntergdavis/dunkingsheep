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
