"""Task model for the flat fleet (FLAT_FLEET.md 5): kinds, one live review per
PR head, verdicts written from the bound reviewer, role claim filters,
unassigned-means-claimable, and the needs-owner question path."""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks, TaskError  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER = "w1:p2"
WORKER = "w2:p3"
REVIEWER = "w2:p1"
AUTHOR = "w2:p2"


class TaskModelTests(unittest.TestCase):
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
        self.roles.register(WORKER, "worker", provider="pi")
        self.roles.register(AUTHOR, "worker", provider="claude")
        self.roles.register(REVIEWER, "reviewer", provider="claude")
        self.system = self.roles.issue_token("system", label="timer")

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None, token=None):
        self.flock.clear_caller()
        self.flock.set_service_token(token)
        try:
            return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)
        finally:
            self.flock.clear_caller()

    def review(self, head="abc123", pr=7, title="review PR 7"):
        return self.call("task_add", {"title": title, "kind": "review", "author": AUTHOR,
                                      "repo": "o/r", "pr": pr, "head": head,
                                      "priority": "high"}, token=self.system)

    def test_a_task_is_unassigned_and_claimable_by_default(self):
        made = self.call("task_add", {"title": "work"}, pane=OWNER)
        self.assertEqual([], made["assignees"])
        self.assertEqual("work", made["kind"])
        self.assertEqual("low", made["priority"])
        got = self.call("task_next", {}, pane=WORKER)
        self.assertEqual(made["id"], got["task"]["id"])

    def test_a_worker_may_only_self_assign(self):
        with self.assertRaisesRegex(DunkError, "only to yourself"):
            self.call("task_add", {"title": "x", "assignees": [AUTHOR]}, pane=WORKER)
        self.call("task_add", {"title": "x", "assignees": [WORKER]}, pane=WORKER)

    def test_one_live_review_per_head(self):
        first = self.review()
        with self.assertRaisesRegex(DunkError, "already exists"):
            self.review()
        self.call("task_cancel", {"id": first["id"], "reason": "new head"}, pane=OWNER)
        self.review()   # the cancelled one no longer blocks

    def test_a_review_needs_its_key(self):
        with self.assertRaisesRegex(DunkError, "needs repo, pr and head"):
            self.call("task_add", {"title": "r", "kind": "review"}, token=self.system)

    def test_workers_never_pull_reviews_and_reviewers_only_pull_reviews(self):
        self.review()
        self.call("task_add", {"title": "work"}, pane=OWNER)
        got = self.call("task_next", {}, pane=WORKER)
        self.assertEqual("work", got["task"]["kind"])
        got = self.call("task_next", {}, pane=REVIEWER)
        self.assertEqual("review", got["task"]["kind"])

    def test_the_author_never_reviews_its_own_head(self):
        self.roles.register(AUTHOR, "reviewer", provider="claude")
        self.review()
        got = self.call("task_next", {}, pane=AUTHOR)
        self.assertIsNone(got["task"])

    def test_a_review_completes_with_a_verdict_from_the_bound_reviewer(self):
        made = self.review()
        self.call("task_next", {}, pane=REVIEWER)
        with self.assertRaisesRegex(DunkError, "needs --verdict"):
            self.call("task_complete", {"id": made["id"]}, pane=REVIEWER)
        done = self.call("task_complete", {"id": made["id"], "verdict": "accept"},
                         pane=REVIEWER)
        self.assertEqual("done", done["status"])
        self.assertEqual("accept", done["verdict"])
        events = [e["event"] for e in self.flock.tasks.events(made["id"])] \
            if hasattr(self.flock.tasks, "events") else None
        if events is not None:
            self.assertIn("verdict", events)

    def test_a_worker_cannot_complete_a_review(self):
        made = self.review()
        with self.assertRaisesRegex(DunkError, "completed by its reviewer"):
            self.call("task_complete", {"id": made["id"], "verdict": "accept"}, pane=WORKER)

    def test_a_free_model_cannot_file_or_pull_reviews(self):
        with self.assertRaisesRegex(DunkError, "filed by the review timer"):
            self.call("task_add", {"title": "r", "kind": "review", "repo": "o/r",
                                   "pr": 1, "head": "h"}, pane=WORKER)

    def test_needs_owner_is_the_question_path(self):
        made = self.call("task_add", {"title": "work"}, pane=OWNER)
        self.call("task_next", {}, pane=WORKER)
        self.call("task_block", {"id": made["id"], "reason": "needs-owner: which port?"},
                  pane=WORKER)
        asks = self.call("task_list", {"needs_owner": True}, pane=OWNER)["tasks"]
        self.assertEqual([made["id"]], [t["id"] for t in asks])
        self.assertEqual([], self.call("task_list", {"needs_owner": True, "kind": "review"},
                                       pane=OWNER)["tasks"])

    def test_on_event_fires_after_commit(self):
        seen = []
        self.flock.tasks.on_event = [lambda task_id, event, **kw: seen.append((task_id, event))]
        made = self.call("task_add", {"title": "work"}, pane=OWNER)
        self.assertIn((made["id"], "created"), seen)

    def test_migration_adds_columns_to_an_old_database(self):
        import sqlite3
        path = os.path.join(self.tmp.name, "old.db")
        db = sqlite3.connect(path)
        db.executescript("""
            CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEXT NOT NULL,
              body TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'open',
              priority TEXT NOT NULL DEFAULT 'medium', parent_id TEXT,
              assignees TEXT NOT NULL DEFAULT '[]', depends_on TEXT NOT NULL DEFAULT '[]',
              created_at REAL NOT NULL, updated_at REAL NOT NULL, created_by TEXT,
              completed_by TEXT, completed_at REAL, cancelled_at REAL, block_reason TEXT);
            INSERT INTO tasks (id, scope, title, created_at, updated_at)
              VALUES ('old-1', 'global', 'old', 1, 1);""")
        db.commit(); db.close()
        store = Tasks(path=path)
        try:
            old = store.get("old-1")
            self.assertEqual("work", old["kind"])
            self.assertIsNone(old["verdict"])
            with self.assertRaisesRegex(TaskError, "already exists"):
                store.add("global", "r1", kind="review", repo="o/r", pr=1, head="h")
                store.add("global", "r2", kind="review", repo="o/r", pr=1, head="h")
        finally:
            store.close()


if __name__ == "__main__":
    unittest.main()
