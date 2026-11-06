"""The send scenario (t1605 slice 3): six commands at one pane, each claimed by that pane, with only
no-op typing. It runs against a fake socket here, never the sandbox or the live daemon."""

import contextlib
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401,E402
import diff_harness as harness  # noqa: E402
import send_scenario  # noqa: E402
from test_diff_harness import FakeDaemon  # noqa: E402

SIX = ("delivery", "delivery_capability", "send_text", "send_keys", "flush_box", "clear_box")


class ScenarioShapeTests(unittest.TestCase):
    def test_the_six_commands_run_in_order_and_all_target_the_pane(self):
        requests = send_scenario.scenario("%2")
        self.assertEqual(SIX, tuple(cmd for cmd, _args in requests))
        for cmd, args in requests:
            self.assertEqual("%2", args["target"], cmd)

    def test_the_only_typing_is_a_shell_no_op(self):
        requests = send_scenario.scenario("%2")
        self.assertEqual(["true"], [args["text"] for _cmd, args in requests if "text" in args])
        self.assertEqual([["Enter"]], [args["keys"] for _cmd, args in requests if "keys" in args])


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
            code = send_scenario.main(argv)
        return code, out.getvalue()

    def test_every_request_claims_the_pane_and_all_six_are_answered(self):
        daemon = self.serve({cmd: {"ok": True, "result": {}} for cmd in SIX})
        code, out = self.run_main(["--socket", self.path, "--pane", "%2"])
        self.assertEqual(0, code, out)
        self.assertEqual(list(SIX), [request["cmd"] for request in daemon.seen])
        for request in daemon.seen:
            self.assertEqual("%2", request["self_pane_id"], request["cmd"])
        self.assertIn("scenario: 6/6 requests answered", out)

    def test_a_refused_request_is_printed_with_its_reason_and_still_counts_as_answered(self):
        replies = {cmd: {"ok": True, "result": {}} for cmd in SIX}
        replies["clear_box"] = {"ok": False,
                                "error": "clear_box requires a superuser (the owner role)"}
        self.serve(replies)
        code, out = self.run_main(["--socket", self.path])
        self.assertEqual(0, code, out)
        self.assertIn("clear_box: refused: clear_box requires a superuser", out)
        self.assertIn("scenario: 6/6 requests answered", out)

    def test_no_reply_at_all_is_exit_one(self):
        code, out = self.run_main(["--socket", self.path])
        self.assertEqual(1, code, out)
        self.assertIn("no reply", out)
        self.assertIn("scenario: 0/6 requests answered", out)

    def test_the_live_socket_is_refused_before_any_request(self):
        with self.assertRaises(SystemExit):
            send_scenario.main(["--socket", harness.LIVE_SOCKET])


if __name__ == "__main__":
    unittest.main()
