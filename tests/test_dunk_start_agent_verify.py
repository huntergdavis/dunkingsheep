"""t215: start-agent must FAIL CLOSED - a pane is not a running agent.

Eight opencode panes were reported "started agent ..." and had actually died with
'opencode: not found' (rc=127) because the pane's PATH lacked /snap/bin. The pane existed, so
the old code believed the agent did. Now the pane's own first lines are read before success is
claimed, and a missing command is a refusal with the captured line.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import DunkError, Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class StartAgentVerifyTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.flock.AGENT_VERIFY_S = 0.4

    def tearDown(self):
        self.flock.close()

    def _start(self, screen):
        # The fake creates the pane and sets its screen; start_agent then verifies it.
        self.herdr.screen_after_create = screen
        return self.flock.start_agent("probe", "claude", cwd="/tmp")

    def test_a_missing_command_is_refused_with_the_captured_line(self):
        self.herdr.screen_after_create = "/bin/sh: 1: opencode: not found\n"
        with self.assertRaisesRegex(DunkError, "never ran|not found"):
            self._start(self.herdr.screen_after_create)

    def test_an_immediate_exit_is_refused_with_the_status(self):
        self.herdr.screen_after_create = "\n[agent exited rc=127]\n"
        with self.assertRaisesRegex(DunkError, "exited immediately"):
            self._start(self.herdr.screen_after_create)

    def test_a_running_agent_is_reported_started(self):
        self.herdr.screen_after_create = "✻ Welcome to Claude Code\n❯ \n"
        result = self._start(self.herdr.screen_after_create)
        self.assertEqual("probe", result["name"])
        self.assertTrue(result["pane_id"])

    def test_a_pane_that_vanishes_is_refused(self):
        self.herdr.screen_after_create = "starting...\n"
        self.herdr.vanish_panes = True
        with self.assertRaisesRegex(DunkError, "already gone"):
            self._start(self.herdr.screen_after_create)


if __name__ == "__main__":
    unittest.main()
