"""Unit tests for the per-command gate of the production socket replay (no tmux, no Rust)."""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401,E402
import production_replay as gate_module  # noqa: E402

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


if __name__ == "__main__":
    unittest.main()
