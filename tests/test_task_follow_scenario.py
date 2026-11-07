"""The task-follow scenario (t1605 slice 4): five requests, four commands, all made as one pane,
with task_add first so the follow has a queue to follow. It runs against a fake socket here, never
the sandbox or the live daemon."""

import contextlib
import io
import os
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))
sys.path.insert(0, ROOT)

import _bootstrap  # noqa: F401,E402
import diff_harness as harness  # noqa: E402
import dunk_roles  # noqa: E402
import production_replay as gate  # noqa: E402
import task_follow_scenario as scenario_module  # noqa: E402
from test_diff_harness import FakeDaemon  # noqa: E402

ORDER = ("task_add", "task_follow", "task_followers", "task_unfollow", "task_followers")
FOUR = ("task_add", "task_follow", "task_followers", "task_unfollow")


class ScenarioShapeTests(unittest.TestCase):
    def test_task_add_seeds_first_then_follow_list_unfollow_list(self):
        requests = scenario_module.scenario("%2")
        self.assertEqual(ORDER, tuple(cmd for cmd, _args in requests))

    def test_the_scenario_covers_exactly_the_commands_the_gate_wants(self):
        requests = scenario_module.scenario("%2")
        self.assertEqual(set(gate.TASK_FOLLOW_COMMANDS), {cmd for cmd, _args in requests})
        self.assertEqual(FOUR, gate.TASK_FOLLOW_COMMANDS)

    def test_the_seed_is_a_low_priority_global_task_with_no_assignee(self):
        args = dict(scenario_module.scenario("%2"))["task_add"]
        self.assertEqual(scenario_module.SEED_TITLE, args["title"])
        self.assertEqual(scenario_module.SEED_BODY, args["body"])
        self.assertEqual("low", args["priority"])
        # No assignee and no scope: the daemon files it with an empty assignee list and
        # created_by set to the pane (the recorded Python behaviour), and global is the default,
        # so the task is visible to the whole sandbox.
        self.assertNotIn("assignees", args)
        self.assertNotIn("scope", args)

    def test_the_follow_is_global_and_no_request_names_a_target(self):
        requests = scenario_module.scenario("%2")
        self.assertEqual({"scope": "global"}, dict(requests)["task_follow"])
        for cmd, args in requests:
            self.assertNotIn("target", args, cmd)
            if cmd in ("task_followers", "task_unfollow"):
                self.assertEqual({}, args, cmd)


class IdentityTests(unittest.TestCase):
    """The identity each command needs, as dunk_roles answers it: the owner pane the sandbox
    registers passes every gate; a worker lane would too; an anonymous caller only reads."""

    def test_the_owner_and_a_worker_may_call_all_four(self):
        for role in ("owner", "worker"):
            for cmd in FOUR:
                self.assertTrue(dunk_roles.allowed(role, cmd), f"{role} {cmd}")

    def test_an_anonymous_caller_may_only_list_the_followers(self):
        self.assertTrue(dunk_roles.allowed("anonymous", "task_followers"))
        for cmd in ("task_add", "task_follow", "task_unfollow"):
            self.assertFalse(dunk_roles.allowed("anonymous", cmd), cmd)


class ScenarioRunTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.path = os.path.join(self.dir.name, "proxy.sock")

    def serve(self, replies):
        daemon = FakeDaemon(replies, self.path).start()
        self.addCleanup(daemon.stop)
        return daemon

    def run_main(self, argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = scenario_module.main(argv)
        return code, out.getvalue()

    def test_every_request_claims_the_pane_and_all_five_are_answered_in_order(self):
        daemon = self.serve({cmd: {"ok": True, "result": {}} for cmd in FOUR})
        code, out = self.run_main(["--socket", self.path, "--pane", "%2"])
        self.assertEqual(0, code, out)
        self.assertEqual(list(ORDER), [request["cmd"] for request in daemon.seen])
        for request in daemon.seen:
            self.assertEqual("%2", request["self_pane_id"], request["cmd"])
        self.assertEqual(scenario_module.SEED_TITLE, daemon.seen[0]["args"]["title"])
        self.assertIn("scenario: 5/5 requests answered", out)

    def test_a_refused_request_is_printed_with_its_reason_and_still_counts_as_answered(self):
        replies = {cmd: {"ok": True, "result": {}} for cmd in FOUR}
        replies["task_follow"] = {"ok": False,
                                  "error": "task follow requires a known caller pane"}
        self.serve(replies)
        code, out = self.run_main(["--socket", self.path])
        self.assertEqual(0, code, out)
        self.assertIn("task_follow: refused: task follow requires a known caller pane", out)
        self.assertIn("scenario: 5/5 requests answered", out)

    def test_no_reply_at_all_is_exit_one(self):
        code, out = self.run_main(["--socket", self.path])
        self.assertEqual(1, code, out)
        self.assertIn("no reply", out)
        self.assertIn("scenario: 0/5 requests answered", out)

    def test_the_live_socket_is_refused_before_any_request(self):
        with self.assertRaises(SystemExit):
            scenario_module.main(["--socket", harness.LIVE_SOCKET])


if __name__ == "__main__":
    unittest.main()
