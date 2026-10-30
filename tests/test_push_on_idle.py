"""t615: registration is the subscription - the daemon pushes to every registered lane.

Measured 2026-10-29 (owner %95): four registered lanes sat idle for ~40 minutes with
13 open tasks because only one of them had ever called `task follow`. The push loop
fed followers and nobody else, so "work is waiting" and "nobody told me" looked
identical from the queue.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
from dunk_core import Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

IDLE_WORKER = "w2:p1"      # codex, agent_status=idle
BUSY_CLAUDE = "w2:p3"      # claude, agent_status=working
IDLE_SHELL = "w2:p2"       # agent_status=unknown, no agent


class PushOnIdleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.flock = Flock(
            herdr=self.herdr,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False,
            restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "roles.json"),
                        tokens_path=os.path.join(self.tmp.name, "tokens.json")),
        )

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def register(self, pane_id, role, provider="claude"):
        return self.flock.roles.register(pane_id, role=role, provider=provider,
                                         by="%95")

    def claimed_task(self, task_id):
        return self.flock.tasks.get(task_id)

    def file_review(self, title, pr):
        """Review tasks are filed by the timer (system) or the owner; the store is
        the seam the timer itself writes through."""
        return self.flock.tasks.add("global", title, kind="review",
                                    repo="huntergdavis/poolrad-decomp", pr=pr,
                                    head=str(pr) * 40, created_by="review-sync")

    def test_a_registered_lane_is_pushed_without_ever_calling_follow(self):
        """The measured defect: registration must be enough on its own."""
        self.register(IDLE_WORKER, "worker", provider="pi")
        task = self.flock.task_add("waiting work", priority="high",
                                   self_pane_id="%95")
        self.assertEqual([], self.flock.task_followers()["followers"])
        self.flock._push_tasks()
        pushed = self.claimed_task(task["id"])
        self.assertEqual("in_progress", pushed["status"])
        self.assertEqual([IDLE_WORKER], pushed["claimed_by"])
        unread = self.flock.inbox_list(unread_only=True,
                                       self_pane_id=IDLE_WORKER)["messages"]
        self.assertTrue(unread[0]["subject"].startswith(f"TASK {task['id']}"))

    def test_an_unregistered_pane_is_not_pushed(self):
        task = self.flock.task_add("waiting work", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(task["id"])["status"])

    def test_a_busy_registered_lane_is_skipped(self):
        self.register(BUSY_CLAUDE, "worker")
        task = self.flock.task_add("waiting work", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(task["id"])["status"])

    def test_a_lane_holding_a_lease_is_not_given_a_second_task(self):
        self.register(IDLE_WORKER, "worker", provider="pi")
        first = self.flock.task_add("first", priority="high", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("in_progress", self.claimed_task(first["id"])["status"])
        second = self.flock.task_add("second", priority="high", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(second["id"])["status"])
        self.assertEqual(1, len(self.flock.tasks.leases_for_pane(IDLE_WORKER)))

    def test_unread_mail_is_read_before_new_work(self):
        self.register(IDLE_WORKER, "worker", provider="pi")
        self.flock.inbox_send(IDLE_WORKER, "read me first", subject="mail",
                              notify=False, self_pane_id="%95")
        task = self.flock.task_add("waiting work", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(task["id"])["status"],
                         "a lane with unread mail is not handed new work")
        # Once the mail is read, the same sweep does push it.
        self.flock.inbox_read(IDLE_WORKER, limit=10, self_pane_id=IDLE_WORKER)
        self.flock._push_tasks()
        self.assertEqual("in_progress", self.claimed_task(task["id"])["status"])

    def test_a_worker_is_never_pushed_a_review(self):
        """t595: the push path used to claim without the role->kind filter."""
        self.register(IDLE_WORKER, "worker", provider="pi")
        review = self.file_review("review something", 1)
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(review["id"])["status"])
        self.assertEqual([], self.claimed_task(review["id"])["claimed_by"])

    def test_a_reviewer_lane_is_pushed_a_review_and_not_work(self):
        self.register(IDLE_WORKER, "reviewer", provider="claude")
        work = self.flock.task_add("ordinary work", priority="high",
                                   self_pane_id="%95")
        review = self.file_review("review something", 2)
        self.flock._push_tasks()
        self.assertEqual("in_progress", self.claimed_task(review["id"])["status"])
        self.assertEqual("open", self.claimed_task(work["id"])["status"])

    def test_a_reviewer_on_a_provider_that_may_not_review_is_skipped(self):
        """Free models are the worker tier: a pi reviewer is not handed reviews."""
        self.register(IDLE_WORKER, "reviewer", provider="pi")
        review = self.file_review("review something", 3)
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(review["id"])["status"])

    def test_desk_and_planner_lanes_are_never_pushed_work(self):
        self.register(IDLE_SHELL, "desk")
        task = self.flock.task_add("waiting work", self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("open", self.claimed_task(task["id"])["status"])

    def test_an_explicit_follower_keeps_its_scope(self):
        """`task follow mine` still means mine: the sweep must not widen it."""
        self.register(IDLE_WORKER, "worker", provider="pi")
        self.flock.task_follow(scope="mine", self_pane_id=IDLE_WORKER)
        unowned = self.flock.task_add("unowned queue work", self_pane_id="%95")
        mine = self.flock.task_add("my own work", assignees=[IDLE_WORKER],
                                   self_pane_id="%95")
        self.flock._push_tasks()
        self.assertEqual("in_progress", self.claimed_task(mine["id"])["status"])
        self.assertEqual("open", self.claimed_task(unowned["id"])["status"])


if __name__ == "__main__":
    unittest.main()
