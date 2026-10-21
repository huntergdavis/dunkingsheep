import os
import unittest

from dunk_callers import (
    agent_name,
    caller_alive,
    caller_from_peer,
    process_alive,
    process_start,
)


class ProcessIdentityTests(unittest.TestCase):
    def test_self_is_alive_with_its_own_start_time(self):
        start = process_start(os.getpid())
        self.assertIsNotNone(start)
        self.assertTrue(process_alive(os.getpid(), start))

    def test_start_time_mismatch_is_dead(self):
        start = process_start(os.getpid())
        self.assertFalse(process_alive(os.getpid(), str(start) + "0"))

    def test_missing_pid_is_dead(self):
        self.assertFalse(process_alive(99_999_999))
        self.assertFalse(caller_alive({"pid": None, "start": None}))
        self.assertFalse(caller_alive(None))

    def test_agent_name_of_this_test_process_is_none(self):
        self.assertIsNone(agent_name(os.getpid()))

    def test_caller_walks_to_a_live_ancestor_and_keeps_the_pane(self):
        caller = caller_from_peer(os.getpid(), "w8:p8")
        self.assertEqual("w8:p8", caller["pane_id"])
        self.assertIsNotNone(caller["pid"])
        self.assertIsNotNone(caller["start"])
        self.assertIn(caller["source"], ("agent", "session", "peer"))
        self.assertTrue(caller_alive(caller))

    def test_the_agent_ancestor_is_preferred_over_the_short_lived_peer(self):
        caller = caller_from_peer(os.getpid(), "w8:p8")
        if caller["source"] != "agent":
            self.skipTest("this test process is not running under an agent")
        self.assertNotEqual(os.getpid(), caller["pid"])
        self.assertIn(caller["name"], {"pi", "claude", "codex", "opencode"})

    def test_invalid_peer_is_a_named_none_caller(self):
        caller = caller_from_peer(None, "w8:p8")
        self.assertEqual("none", caller["source"])
        self.assertIsNone(caller["pid"])
        self.assertFalse(caller_alive(caller))


if __name__ == "__main__":
    unittest.main()
