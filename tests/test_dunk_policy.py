"""Resource policy: thresholds, the deny evaluation, dry-run, and one real kill.

Acceptance: each threshold trips against a synthetic sample; a dry run logs and
kills nothing; a real kill of a process we started records the signal; a denied
pid is refused by policy, not by luck.
"""

import subprocess
import unittest

from tests import _bootstrap  # noqa: F401

import dunk_load
import dunk_policy


def sample(**overrides):
    base = {"name": "lane", "pid": 4242, "starttime": 99, "cpu_pct": 340.0,
            "rss_mb": 2100.0, "wall_minutes": 95, "cwd": "/home/hunter/workspace/x"}
    base.update(overrides)
    return base


class EvaluateTests(unittest.TestCase):
    def test_cpu_threshold_trips_with_its_window(self):
        findings = dunk_policy.evaluate(sample(), {"cpu_pct_over": 300, "cpu_window_s": 600})
        self.assertEqual("cpu", findings[0]["rule"])
        self.assertEqual(340.0, findings[0]["measured"])
        self.assertEqual(600, findings[0]["window"])

    def test_rss_threshold_trips(self):
        findings = dunk_policy.evaluate(sample(), {"rss_mb_over": 1500, "rss_window_s": 300})
        self.assertEqual("rss", findings[0]["rule"])

    def test_wall_threshold_trips(self):
        findings = dunk_policy.evaluate(sample(), {"wall_minutes_over": 90})
        self.assertEqual("wall", findings[0]["rule"])

    def test_below_threshold_does_not_trip(self):
        self.assertEqual([], dunk_policy.evaluate(sample(cpu_pct=10), {"cpu_pct_over": 300, "cpu_window_s": 60}))

    def test_a_threshold_without_a_window_is_a_policy_error(self):
        with self.assertRaises(dunk_policy.PolicyError):
            dunk_policy.evaluate(sample(), {"cpu_pct_over": 300})


class KillTests(unittest.TestCase):
    def test_dry_run_logs_and_signals_nothing(self):
        calls = []
        record = dunk_policy.kill_decision(
            sample(), {"kill": True, "cpu_pct_over": 300, "cpu_window_s": 600},
            registered={(4242, 99)}, dry_run=True, signal_fn=lambda *a: calls.append(a))
        self.assertEqual("would-kill", record["decision"])
        self.assertEqual([], calls)

    def test_default_is_off(self):
        record = dunk_policy.kill_decision(
            sample(), {"cpu_pct_over": 300, "cpu_window_s": 60}, registered={(4242, 99)})
        self.assertEqual("alert-only", record["decision"])

    def test_an_unregistered_pid_is_refused(self):
        record = dunk_policy.kill_decision(
            sample(), {"kill": True, "cpu_pct_over": 1, "cpu_window_s": 1},
            registered=set(), dry_run=False)
        self.assertEqual("refused", record["decision"])
        self.assertIn("unattributed", record["reason"])

    def test_a_protected_pane_is_refused_by_policy_not_by_luck(self):
        calls = []
        record = dunk_policy.kill_decision(
            sample(), {"kill": True, "cpu_pct_over": 1, "cpu_window_s": 1},
            registered={(4242, 99)}, protected_panes=(4242,), dry_run=False,
            signal_fn=lambda *a: calls.append(a))
        self.assertEqual("refused", record["decision"])
        self.assertIn("protected", record["reason"])
        self.assertEqual([], calls)

    def test_a_cwd_outside_a_workspace_is_refused(self):
        record = dunk_policy.kill_decision(
            sample(cwd="/tmp/elsewhere"), {"kill": True, "cpu_pct_over": 1, "cpu_window_s": 1},
            registered={(4242, 99)}, workspace_roots=("/home/hunter/workspace",), dry_run=False)
        self.assertEqual("refused", record["decision"])

    def test_a_real_kill_of_a_process_we_started(self):
        process = subprocess.Popen(["sleep", "30"])
        self.addCleanup(lambda: process.poll() is None and process.kill())
        stat = dunk_load._stat(process.pid)
        record = dunk_policy.kill_decision(
            {"name": "child", "pid": process.pid, "starttime": stat["starttime"],
             "cpu_pct": 999.0, "rss_mb": 1.0, "wall_minutes": 1},
            {"kill": True, "cpu_pct_over": 1, "cpu_window_s": 1},
            registered={(process.pid, stat["starttime"])},
            dry_run=False, grace_s=0.2)
        self.assertEqual("kill", record["decision"])
        self.assertIn("TERM", record["signals"])
        process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
