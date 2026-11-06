"""Unit tests for the per-command gate of the production socket replay (no tmux, no Rust)."""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401,E402
import production_replay as gate_module  # noqa: E402
from test_diff_harness import FakeDaemon  # noqa: E402

SIX = gate_module.SEND_COMMANDS


def traffic(*commands):
    return [{"cmd": cmd, "args": {}, "response": {}} for cmd in commands]


def replay_result(diffs=(), errors=()):
    return {"sent": 0, "diffs": list(diffs), "ratios": [], "errors": list(errors)}


class ProductionReplayGateTests(unittest.TestCase):
    def test_every_command_matched_and_no_diff_passes(self):
        ok, lines = gate_module.gate(traffic(*SIX), replay_result(), SIX)
        self.assertTrue(ok, lines)
        self.assertEqual(len(lines), len(SIX))
        for line in lines:
            self.assertIn(" ok ", line)
            self.assertIn("matched=1", line)

    def test_a_command_with_no_recorded_traffic_fails(self):
        without = tuple(cmd for cmd in SIX if cmd != "delivery_capability")
        ok, lines = gate_module.gate(traffic(*without), replay_result(), SIX)
        self.assertFalse(ok)
        failing = [line for line in lines if "FAIL" in line]
        self.assertEqual(len(failing), 1, lines)
        self.assertTrue(failing[0].startswith("delivery_capability:"), failing[0])
        self.assertIn("no recorded traffic", failing[0])

    def test_zero_matches_fails_even_though_the_replay_exits_zero(self):
        # Every send_text request got no reply, so nothing matched.
        errors = ["send_text: no reply from /tmp/ds.sock"]
        ok, lines = gate_module.gate(traffic("send_text"), replay_result(errors=errors),
                                     ("send_text",))
        self.assertFalse(ok)
        self.assertIn("zero matched requests", lines[0])
        self.assertIn("matched=0", lines[0])

    def test_a_diff_fails_the_gate_and_is_counted_per_command(self):
        diffs = [("send_keys", "--- expected\n+++ actual\n-x\n+y")]
        ok, lines = gate_module.gate(traffic(*SIX), replay_result(diffs=diffs), SIX)
        self.assertFalse(ok)
        failing = [line for line in lines if "FAIL" in line]
        self.assertEqual(len(failing), 1, lines)
        self.assertTrue(failing[0].startswith("send_keys:"), failing[0])
        self.assertIn("1 diff(s)", failing[0])

    def test_an_unanswered_request_fails_even_when_another_matches(self):
        # Two send_text requests, one unanswered: one matched, but the unanswered one fails the gate.
        errors = ["send_text: no reply from /tmp/ds.sock"]
        ok, lines = gate_module.gate(traffic("send_text", "send_text"),
                                     replay_result(errors=errors), ("send_text",))
        self.assertFalse(ok)
        self.assertIn("requested=2 matched=1", lines[0])
        self.assertIn("1 unanswered request(s)", lines[0])


class CommandLineGateTests(unittest.TestCase):
    """main() replays against a fake daemon, prints the gate, and returns the exit code."""

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def write_traffic(self, entries):
        path = os.path.join(self.dir.name, "traffic.jsonl")
        with open(path, "w", encoding="utf-8") as handle:
            for entry in entries:
                handle.write(json.dumps(entry) + "\n")
        return path

    def daemon(self, replies):
        daemon = FakeDaemon(replies, os.path.join(self.dir.name, "daemon.sock")).start()
        self.addCleanup(daemon.stop)
        return daemon

    def run_main(self, argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = gate_module.main(argv)
        return code, out.getvalue()

    def test_exits_zero_when_every_wanted_command_matches_the_recording(self):
        replies = {"send_text": {"ok": True, "result": {"submitted": True}},
                   "send_keys": {"ok": True, "result": {"sent": 1}}}
        daemon = self.daemon(replies)
        traffic = self.write_traffic([
            {"cmd": "send_text", "args": {"target": "w1:p1", "text": "hi"},
             "response": replies["send_text"]},
            {"cmd": "send_keys", "args": {"target": "w1:p1", "keys": ["Enter"]},
             "response": replies["send_keys"]},
        ])
        code, out = self.run_main(["--traffic", traffic, "--against", daemon.path,
                                   "--commands", "send_text,send_keys"])
        self.assertEqual(0, code, out)
        self.assertIn("send_text: ok requested=1 matched=1 diffs=0", out)
        self.assertIn("send_keys: ok requested=1 matched=1 diffs=0", out)

    def test_exits_one_and_prints_the_diff_when_a_reply_differs(self):
        daemon = self.daemon({"send_text": {"ok": True, "result": {"submitted": False}}})
        traffic = self.write_traffic([
            {"cmd": "send_text", "args": {"target": "w1:p1", "text": "hi"},
             "response": {"ok": True, "result": {"submitted": True}}},
        ])
        code, out = self.run_main(["--traffic", traffic, "--against", daemon.path,
                                   "--commands", "send_text"])
        self.assertEqual(1, code, out)
        self.assertIn("--- DIFF send_text ---", out)
        self.assertIn("send_text: FAIL", out)
        self.assertIn("1 diff(s)", out)

    def test_exits_one_when_a_wanted_command_has_no_recorded_traffic(self):
        daemon = self.daemon({"send_text": {"ok": True}})
        traffic = self.write_traffic([
            {"cmd": "send_text", "args": {}, "response": {"ok": True}},
        ])
        code, out = self.run_main(["--traffic", traffic, "--against", daemon.path,
                                   "--commands", "send_text,delivery"])
        self.assertEqual(1, code, out)
        self.assertIn("delivery: FAIL requested=0", out)

    def test_exits_one_when_the_socket_does_not_answer(self):
        traffic = self.write_traffic([
            {"cmd": "send_text", "args": {}, "response": {"ok": True}},
        ])
        absent = os.path.join(self.dir.name, "absent.sock")
        code, out = self.run_main(["--traffic", traffic, "--against", absent,
                                   "--commands", "send_text"])
        self.assertEqual(1, code, out)
        self.assertIn("unanswered request(s)", out)


if __name__ == "__main__":
    unittest.main()
