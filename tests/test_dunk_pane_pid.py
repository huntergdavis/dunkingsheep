"""t253: the pane-list endpoint must carry pane_pid through.

Measured (audit i28660, re-measured 2026-10-28 on main b17a1d6): core
list_panes() supplies pane_pid (resolve_lanes reads it for process-based
capture - dunk_core "pane_pid lets capture read the session from the PROCESS"),
but dunk_api.h_list_panes() dropped it because its keep tuple did not name it:
`dunkingsheep panes --json` returned 0 of N panes with pane_pid.

Contract, same as t299: the handler is a FILTER - a key is present only when
the core supplies it, and a real value (a PID is an int) must survive.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402


class FakeFlock:
    def __init__(self, panes):
        self._panes = panes

    def list_panes(self, self_pane_id=None):
        return self._panes


def row(**extra):
    base = {"pane_id": "%1", "agent": "pi", "agent_status": "idle", "cwd": "/tmp"}
    base.update(extra)
    return base


class PanePidTests(unittest.TestCase):
    def _one(self, pane):
        result = dunk_api.h_list_panes(FakeFlock([pane]), {}, None)
        return result["panes"][0]

    def test_pane_pid_survives_the_handler(self):
        got = self._one(row(pane_pid=4242))
        self.assertEqual(4242, got["pane_pid"],
                         "a real PID must survive the keep tuple")

    def test_pane_pid_absent_when_core_does_not_supply_it(self):
        got = self._one(row())
        self.assertNotIn("pane_pid", got,
                         "the filter contract: no core key, no null key")

    def test_zero_pid_is_a_value_not_a_missing_key(self):
        got = self._one(row(pane_pid=0))
        self.assertIn("pane_pid", got)
        self.assertEqual(0, got["pane_pid"])


if __name__ == "__main__":
    unittest.main()
