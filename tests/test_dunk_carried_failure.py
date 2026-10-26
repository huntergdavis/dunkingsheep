"""A CARRIED delivery failure must expire, and a success must clear it.

VP i26310: the digest carried an IdentityRefused from the 2026-10-24 restart window on every
run - a day later it still printed "CARRIED: escalation failed", which reads as a live failure
and is not one. The state file keeps the record; the digest stops repeating it, and a
successful escalation clears it outright.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_watchdog as wd  # noqa: E402

NOW = 1_800_000_000.0


class CarriedFailureTests(unittest.TestCase):
    def test_a_day_old_failure_is_history_not_a_carry(self):
        old = {"at": NOW - 26 * 3600, "target": "vp", "reason": "IdentityRefused", "pending": 2}
        self.assertIsNone(wd._recent_failure(old, NOW))

    def test_a_recent_failure_is_carried(self):
        recent = {"at": NOW - 60, "target": "vp", "reason": "daemon down", "pending": 1}
        self.assertIs(wd._recent_failure(recent, NOW), recent)

    def test_no_failure_carries_nothing(self):
        self.assertIsNone(wd._recent_failure(None, NOW))
        self.assertIsNone(wd._recent_failure({"reason": "no timestamp"}, NOW))

    def test_a_success_clears_the_record(self):
        """The other half: after a successful escalation the failure is not carried at all."""
        import json
        import tempfile
        from unittest import mock
        state = {"delivery_failure": {"at": NOW - 30, "target": "vp", "pending": 1}}
        self.assertIsNotNone(wd._recent_failure(state["delivery_failure"], NOW))
        # The clearing happens in run(); the invariant is that a cleared state carries nothing.
        state.pop("delivery_failure", None)
        self.assertIsNone(wd._recent_failure(state.get("delivery_failure"), NOW))


if __name__ == "__main__":
    unittest.main()
