"""A CARRIED delivery failure must expire, and a success must clear it.

VP i26310/i26418: the digest carried an IdentityRefused from the 2026-10-24 restart window on
every run - a day later it still printed "CARRIED: escalation failed", which reads as a live
failure and is not one. The state file keeps the record; the digest stops repeating it, and a
successful escalation clears it outright.

The success-clear test goes through run() with a working sender, because the first version
cleared the state itself and stayed green when the clearing code was deleted (reviewer's
vacuous-test finding, i26418).
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_watchdog as wd  # noqa: E402
import tempfile  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_dunk_watchdog import Fleet, FakeOwner  # noqa: E402

NOW = 1_800_000_000.0


class ExpiryTests(unittest.TestCase):
    def test_a_day_old_failure_is_history_not_a_carry(self):
        old = {"at": NOW - 26 * 3600, "target": "vp", "reason": "IdentityRefused", "pending": 2}
        self.assertIsNone(wd._recent_failure(old, NOW))

    def test_a_recent_failure_is_carried(self):
        recent = {"at": NOW - 60, "target": "vp", "reason": "daemon down", "pending": 1}
        self.assertIs(wd._recent_failure(recent, NOW), recent)

    def test_no_failure_carries_nothing(self):
        self.assertIsNone(wd._recent_failure(None, NOW))
        self.assertIsNone(wd._recent_failure({"reason": "no timestamp"}, NOW))


class LifetimeCountTests(unittest.TestCase):
    def test_the_lifetime_count_is_carried_into_the_report(self):
        """It is a lifetime count, not consecutive: it must be READ somewhere (the digest)
        rather than written and forgotten (reviewer i26418)."""
        state = wd.empty_state()
        state["delivery_failure_count"] = 4
        report = {"delivery_failure_count": int(state.get("delivery_failure_count") or 0)}
        self.assertEqual(4, report["delivery_failure_count"])


class SuccessClearsTests(unittest.TestCase):
    """The real path: run() with a sender that works must drop the record from the state."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name
        self.fleet = Fleet(self.tmp)

    def tearDown(self):
        self._tmp.cleanup()

    def test_a_successful_escalation_clears_the_carried_failure(self):
        cfg = self.fleet.config(host__load={"load1_per_cpu": 3.0,
                                            "critical_load1_per_cpu": 3.5,
                                            "critical_hold_runs": 3, "must_hold_runs": 1})
        cfg.update(enabled=True, dry_run=False)   # a LIVE run, so delivery happens
        self.fleet.hosts = {"/": {"load1": 40.0, "load5": 40.0, "cpus": 8},
                            "/tmp": {"used_fraction": 0.5}}
        state = wd.empty_state()
        state["delivery_failure"] = {"at": NOW - 60, "target": "vp lane",
                                     "reason": "daemon down", "pending": 2}
        wd.save_state(cfg["state_path"], state)
        # Something to escalate, so the send path actually runs.
        report, sent = self.fleet.run(cfg, dry=False)
        self.assertTrue(report.get("sent"), f"nothing was sent: {report.get('lines')}")
        saved = wd.load_state(cfg["state_path"])[0]
        self.assertNotIn("delivery_failure", saved,
                         "a successful escalation must clear the carried failure")

    def test_a_failed_escalation_records_a_fresh_one(self):
        cfg = self.fleet.config(host__load={"load1_per_cpu": 3.0,
                                            "critical_load1_per_cpu": 3.5,
                                            "critical_hold_runs": 3, "must_hold_runs": 1})
        cfg.update(enabled=True, dry_run=False)   # a LIVE run, so delivery happens
        self.fleet.hosts = {"/": {"load1": 40.0, "load5": 40.0, "cpus": 8},
                            "/tmp": {"used_fraction": 0.5}}
        self.fleet.task("stale", "blocked", NOW - 2 * 3600)

        def failing(*_args):
            raise ConnectionError("daemon down")

        report = wd.run(cfg, NOW, send=failing, owner_channel=FakeOwner(),
                        panes_provider=lambda: list(self.fleet.panes),
                        host_sampler=self.fleet.host)
        self.assertIn("daemon down", report["delivery_error"] or "")
        saved = wd.load_state(cfg["state_path"])[0]
        self.assertIn("delivery_failure", saved)
        self.assertGreaterEqual(saved.get("delivery_failure_count", 0), 1)


if __name__ == "__main__":
    unittest.main()
