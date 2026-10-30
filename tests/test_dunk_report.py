"""The deterministic fleet report (FLAT_FLEET.md 7.3): no model, no invention."""

import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
import dunk_report  # noqa: E402
from dunk_core import Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER, DESK, WORKER = "%95", "%96", "%99"


class ReportTests(unittest.TestCase):
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
        self.roles.register(DESK, "desk", provider="pi", label="desk")
        self.roles.register(WORKER, "worker", provider="pi", label="pi-worker-3")
        self.out = os.path.join(self.tmp.name, "reports")

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

    def seed(self):
        task = self.flock.tasks.add("global", "rebase PR 1625", priority="high")
        self.flock.tasks.assign(task["id"], [WORKER])
        self.flock.tasks.claim(task["id"], WORKER) if hasattr(self.flock.tasks, "claim") else None
        blocked = self.flock.tasks.add("global", "needs a decision", priority="urgent")
        self.flock.tasks.update(blocked["id"], status="blocked",
                                block_reason="needs-owner: switch the desk over?")
        return task["id"], blocked["id"]

    def test_every_section_is_present_and_measured_or_says_it_is_not(self):
        self.seed()
        report = dunk_report.collect(self.flock, now=time.time())
        for name in ("tasks", "lanes", "needs_owner", "reviews", "poolrad",
                     "budget", "watchdog"):
            self.assertIn(name, report["sections"])
        text = dunk_report.render(report)
        for heading in ("## Task metrics", "## Lanes", "## Needs the owner",
                        "## Review queue", "## Poolrad", "## Budget", "## Watchdog"):
            self.assertIn(heading, text)
        # An unmeasured section says so; it never prints a bare zero.
        self.assertIn("not measured here", text)

    def test_the_needs_owner_question_is_quoted_not_summarised(self):
        _working, blocked = self.seed()
        report = dunk_report.collect(self.flock, now=time.time())
        questions = report["sections"]["needs_owner"]["questions"]
        self.assertEqual([blocked], [row["id"] for row in questions])
        self.assertEqual("switch the desk over?", questions[0]["question"])
        self.assertIn(f"{blocked} [urgent]", dunk_report.render(report))

    def test_a_run_writes_one_file_and_posts_to_the_owner_mailbox(self):
        self.seed()
        out = self.call("report_run", {"out_dir": self.out}, pane=DESK)
        self.assertTrue(os.path.exists(out["path"]))
        text = open(out["path"], encoding="utf-8").read()
        self.assertIn("Deterministic block", text)
        posted = out["posted"] or {}
        self.assertTrue(posted.get("message_id"), posted)
        self.assertEqual("report", posted.get("transport"))
        mailbox = self.flock.inbox.conversation("owner")
        self.assertEqual("report", mailbox[-1]["transport"])
        self.assertIn("Fleet report", mailbox[-1]["subject"])

    def test_no_post_writes_the_file_only(self):
        self.seed()
        out = self.call("report_run", {"out_dir": self.out, "post": False}, pane=DESK)
        self.assertTrue(os.path.exists(out["path"]))
        self.assertIsNone(out["posted"])
        self.assertEqual([], self.flock.inbox.conversation("owner"))

    def test_report_list_shows_the_files_newest_first(self):
        self.seed()
        first = self.call("report_run", {"out_dir": self.out, "post": False}, pane=DESK)
        time.sleep(0.01)
        second = self.call("report_run", {"out_dir": self.out, "post": False}, pane=DESK)
        listed = self.call("report_list", {"out_dir": self.out}, pane=WORKER)
        self.assertEqual(2, listed["count"])
        self.assertEqual(os.path.basename(second["path"]),
                         os.path.basename(listed["reports"][0]["path"]))

    def test_prose_naming_a_task_the_report_does_not_contain_is_rejected(self):
        self.seed()
        out = self.call("report_run",
                        {"out_dir": self.out, "post": False,
                         "prose": "t999 was merged and everything is done."}, pane=DESK)
        self.assertTrue(out["prose_rejected"])
        self.assertTrue(any("t999" in problem for problem in out["prose_rejected"]))
        self.assertNotIn("In plain language", out["text"],
                         "rejected prose is not posted above the numbers")

    def test_prose_claiming_a_status_the_report_does_not_report_is_rejected(self):
        self.seed()
        out = self.call("report_run",
                        {"out_dir": self.out, "post": False,
                         "prose": "Every task is done now."}, pane=DESK)
        self.assertTrue(any("done" in problem for problem in out["prose_rejected"]))

    def test_prose_that_stays_inside_the_numbers_is_kept_below_them(self):
        working, blocked = self.seed()
        out = self.call("report_run",
                        {"out_dir": self.out, "post": False,
                         "prose": f"{blocked} is waiting on the owner."}, pane=DESK)
        self.assertEqual([], out["prose_rejected"])
        self.assertIn("In plain language", out["text"])
        self.assertLess(out["text"].index("## Task metrics"),
                        out["text"].index("In plain language"),
                        "the deterministic block is posted above the prose")

    def test_a_worker_may_read_reports_but_not_post_them(self):
        self.seed()
        with self.assertRaises(Exception):
            self.call("report_run", {"out_dir": self.out}, pane=WORKER)
        self.assertEqual(0, self.call("report_list", {"out_dir": self.out},
                                      pane=WORKER)["count"])

    def test_the_units_exist_and_use_the_system_token(self):
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        service = os.path.join(root, "packaging/systemd/dunkingsheep-report.service")
        timer = os.path.join(root, "packaging/systemd/dunkingsheep-report.timer")
        self.assertTrue(os.path.exists(service))
        self.assertTrue(os.path.exists(timer))
        text = open(service, encoding="utf-8").read()
        self.assertIn("DUNKINGSHEEP_SERVICE_TOKEN_FILE", text)
        self.assertIn("report run", text)
        self.assertIn("OnUnitActiveSec=4h", open(timer, encoding="utf-8").read())


if __name__ == "__main__":
    unittest.main()


class UnmeasuredSectionTests(unittest.TestCase):
    """t637: the reviewer of #124 proved three ways the report lied about a failure.

    The rule the report exists to enforce is "a zero is a measurement; 'we did not
    look' is not one". These three bugs broke it in the section the owner reads first
    and in the lane list, and each one is reproduced here as it was demonstrated.
    """

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
        self.now = time.time()

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_a_busy_daemon_does_not_kill_the_whole_report(self):
        """Finding A: needs_owner_section was the one builder with no guard, so a
        raising task_list made collect() raise and NO report was written."""
        def boom(*args, **kwargs):
            raise RuntimeError("daemon busy")

        self.flock.task_list = boom
        report = dunk_report.collect(self.flock, now=self.now)  # must not raise
        section = report["sections"]["needs_owner"]
        self.assertIn("error", section)
        self.assertIn("daemon busy", section["error"])
        text = dunk_report.render(report)
        self.assertIn("not measured here", text)
        self.assertIn("daemon busy", text)

    def test_an_unreadable_needs_owner_section_never_prints_none(self):
        """Finding B: the renderer ignored the error key and printed '- none', telling
        the owner nothing is waiting when the truth was that we could not look."""
        report = dunk_report.collect(self.flock, now=self.now)
        report["sections"]["needs_owner"] = {
            "error": "needs-owner query unavailable: daemon busy"}
        text = dunk_report.render(report)
        head, _, tail = text.partition("## Needs the owner")
        self.assertTrue(tail, "the section must be present")
        self.assertNotIn("- none", tail.split("##")[0],
                         "an unread section must not read as an empty one")
        self.assertIn("not measured here", tail)

    def test_a_failed_task_read_does_not_report_live_panes_as_idle(self):
        """Finding C: lanes_section swallowed the failure into tasks=[], so a pane that
        might be working was printed as idle."""
        class BrokenTasks:
            def __getattr__(self, name):
                def boom(*args, **kwargs):
                    raise RuntimeError("tasks.db is locked")
                return boom

        original = self.flock.tasks
        self.flock.tasks = BrokenTasks()
        try:
            section = dunk_report.lanes_section(self.flock, now=self.now)
        finally:
            self.flock.tasks = original
        self.assertIn("error", section, "the failure must be carried, not swallowed")
        self.assertIn("tasks.db is locked", section["error"])
        self.assertEqual([], section.get("idle") or [],
                         "an idle list computed without the lease table is a guess")
        report = dunk_report.collect(self.flock, now=self.now)
        report["sections"]["lanes"] = section
        text = dunk_report.render(report)
        self.assertIn("tasks.db is locked", text)
