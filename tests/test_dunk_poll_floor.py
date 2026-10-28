"""t212: the idle poll has a floor, so a leaked daemon cannot spin.

Every pane poll forks tmux, and through the guard shim on PATH that is two more processes - 218 guards
were measured at load 78 on 2026-10-27. A test that sets the poll interval to 0 (the suite does, so CLI
calls stay responsive) must not turn a leaked daemon into a fork storm.

Pinned as an INVARIANT rather than a fork count: counting forks over a window is timing-dependent and
would pass or fail with the host's scheduler, and a test that cannot fail reliably is worse than none.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402


class PollFloorTests(unittest.TestCase):
    def test_the_floor_is_at_least_a_quarter_second(self):
        self.assertGreaterEqual(dunk_core.MIN_IDLE_POLL_S, 0.25,
                                "a zero floor is a fork storm when a daemon is leaked")

    def test_the_poll_loop_uses_the_floor(self):
        """The constant is worthless if the loop waits on IDLE_POLL_S alone."""
        source = open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                   "dunk_core.py")).read()
        self.assertIn("max(IDLE_POLL_S, MIN_IDLE_POLL_S)", source,
                      "the poll loop must apply the floor, not just define it")

    def test_a_zero_interval_is_lifted(self):
        saved = dunk_core.IDLE_POLL_S
        try:
            dunk_core.IDLE_POLL_S = 0
            self.assertGreaterEqual(max(dunk_core.IDLE_POLL_S, dunk_core.MIN_IDLE_POLL_S), 0.5,
                                    "a zero interval must be lifted to the floor")
        finally:
            dunk_core.IDLE_POLL_S = saved


if __name__ == "__main__":
    unittest.main()
