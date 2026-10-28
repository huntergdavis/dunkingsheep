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

    def test_the_poll_loop_calls_the_floored_helper(self):
        """A spelling pin, kept only as a cheap tripwire next to the behavioural test above."""
        source = open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                   "dunk_core.py")).read()
        self.assertIn("idle_poll_wait(stop_event)", source,
                      "the poll loop must call the floored helper, not wait on IDLE_POLL_S alone")

    def test_a_zero_interval_is_lifted_by_the_code_the_loop_calls(self):
        """BEHAVIOURAL, not a tautology (reviewer, PR #48): the fake event records the argument it was
        given, so this fails if the loop stops applying the floor. Computing max() in the test and
        asserting a property of Python's max() would stay green with the floor removed."""
        class RecordingStop:
            def __init__(self):
                self.arg = None
            def wait(self, seconds):
                self.arg = seconds
                return False
        stop = RecordingStop()
        dunk_core.idle_poll_wait(stop, idle_poll_s=0)
        self.assertGreaterEqual(stop.arg, dunk_core.MIN_IDLE_POLL_S,
                                "a zero interval must be lifted to the floor by the shipped helper")
        stop2 = RecordingStop()
        dunk_core.idle_poll_wait(stop2, idle_poll_s=30)
        self.assertEqual(30, stop2.arg, "a long interval must not be shortened")


if __name__ == "__main__":
    unittest.main()
