"""Activity per harness: the tool knows, the screen guesses, absent is UNKNOWN.

The pi lane that read 'ready' while working is the case that made this module: pi
reports no status on its pane record, so our status field was not the thing it
claimed to be. Each harness now answers for itself, and a missing answer is
'unknown with a reason' - never 'idle'.
"""
import os
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_activity  # noqa: E402


class ClaudeActivityTests(unittest.TestCase):
    def client(self, session=None, error=None):
        class Status:
            def session_for_pane(self, pane, fresh=False):
                if error:
                    raise error
                return session
        return Status()

    def test_each_claude_status_maps_to_a_state(self):
        pane = {"pane_id": "w1:p1", "agent": "claude", "cwd": "/x"}
        for reported, expected in (("busy", "working"), ("waiting", "waiting"), ("idle", "idle")):
            record = dunk_activity.activity(pane, claude_status=self.client(
                {"status": reported, "waitingFor": "user"}))
            self.assertEqual(expected, record["state"], reported)
            self.assertIsNone(record["reason"])

    def test_no_matching_session_is_unknown_not_idle(self):
        pane = {"pane_id": "w1:p1", "agent": "claude", "cwd": "/x"}
        record = dunk_activity.activity(pane, claude_status=self.client(None))
        self.assertEqual("unknown", record["state"])
        self.assertIn("no claude session", record["reason"])

    def test_a_failing_probe_is_unknown_with_the_error(self):
        pane = {"pane_id": "w1:p1", "agent": "claude", "cwd": "/x"}
        record = dunk_activity.activity(pane, claude_status=self.client(error=OSError("nope")))
        self.assertEqual("unknown", record["state"])
        self.assertIn("nope", record["reason"])

    def test_an_unrecognised_status_is_unknown(self):
        pane = {"pane_id": "w1:p1", "agent": "claude", "cwd": "/x"}
        record = dunk_activity.activity(pane, claude_status=self.client({"status": "compacting"}))
        self.assertEqual("unknown", record["state"])
        self.assertIn("compacting", record["reason"])


class PiActivityTests(unittest.TestCase):
    def test_an_active_turn_is_working_with_its_start(self):
        now = time.time()
        turns = {"w1:p1": {"started_at": now - 60, "expires_at": now + 60}}
        record = dunk_activity.activity({"pane_id": "w1:p1", "agent": "pi"}, turns=turns, now=now)
        self.assertEqual("working", record["state"])
        self.assertEqual(now - 60, record["since"])

    def test_an_ended_turn_is_idle_because_the_end_time_is_evidence(self):
        now = time.time()
        turns = {"w1:p1": {"started_at": now - 600, "ended_at": now - 60, "expires_at": 0.0}}
        record = dunk_activity.activity({"pane_id": "w1:p1", "agent": "pi"}, turns=turns, now=now)
        self.assertEqual("idle", record["state"])
        self.assertEqual(now - 60, record["since"])

    def test_no_turn_ever_recorded_is_unknown(self):
        record = dunk_activity.activity({"pane_id": "w1:p9", "agent": "pi"}, turns={}, now=time.time())
        self.assertEqual("unknown", record["state"])
        self.assertIn("no pi turn", record["reason"])


class UnknownIsNeverIdleTests(unittest.TestCase):
    def test_codex_and_opencode_use_declared_turns_with_explicit_provenance(self):
        now = time.time()
        for harness in ("codex", "opencode"):
            pane = {"pane_id": "%1", "agent": harness}
            for turn, expected in (
                ({"started_at": now - 1, "expires_at": now + 60}, "working"),
                ({"ended_at": now - 1, "expires_at": 0}, "idle"),
                ({"expires_at": now - 1}, "unknown"),
                ({"ended_at": now - dunk_activity.IDLE_CLAIM_S - 1}, "unknown"),
                ({}, "unknown"),
            ):
                with self.subTest(harness=harness, turn=turn):
                    record = dunk_activity.activity(pane, turns={"%1": turn}, now=now)
                    self.assertEqual(expected, record["state"])
                    self.assertEqual(f"{harness} declared turns", record["source"])
                    self.assertTrue(record["inferred"])
                    if expected == "unknown":
                        self.assertTrue(record["reason"])

    def test_every_missing_signal_reads_unknown_with_a_reason(self):
        cases = [
            ({"pane_id": "w1:p1", "agent": "codex"}, {}),
            ({"pane_id": "w1:p1", "agent": "mystery"}, {}),
            ({"pane_id": "w1:p1", "agent": "claude"}, {}),
            ({"pane_id": "w1:p1", "agent": "pi"}, {}),
            ({"pane_id": "w1:p1"}, {}),
        ]
        for pane, kwargs in cases:
            record = dunk_activity.activity(pane, now=time.time(), **kwargs)
            self.assertEqual("unknown", record["state"], pane)
            self.assertTrue(record["reason"], "an unknown must say why")

    def test_the_states_are_a_closed_set(self):
        self.assertEqual(("working", "waiting", "idle", "unknown"), dunk_activity.STATES)


if __name__ == "__main__":
    unittest.main()
