"""t115: an agent that dies must leave evidence, not an empty space.

VP i24311: `start-agent opencode ...` printed "started agent ... in %24" and the pane
was gone within 3 s, with nothing to read. The tmux backend ran the agent command AS
the window process with remain-on-exit off, so any fast exit destroyed the pane and the
error with it. This test pins the wrapper that keeps the pane, prints the exit status
and drops to a shell.
"""

import os
import shlex
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_terminal import TmuxBackend  # noqa: E402


class AgentLaunchWrapperTests(unittest.TestCase):
    def test_the_command_runs_under_a_shell(self):
        wrapped = TmuxBackend._agent_launch("opencode -m model")
        parts = shlex.split(wrapped)
        self.assertEqual(["sh", "-c"], parts[:2], wrapped)
        self.assertIn("opencode -m model", parts[2])

    def test_the_exit_status_is_printed(self):
        wrapped = TmuxBackend._agent_launch("opencode -m model")
        self.assertIn("agent exited rc=", wrapped)

    def test_a_shell_stays_after_the_agent_exits(self):
        wrapped = TmuxBackend._agent_launch("opencode -m model")
        self.assertIn("exec sh", wrapped,
                      "without a shell the pane closes the moment the agent exits")

    def test_quoting_survives_a_command_with_arguments_containing_spaces(self):
        wrapped = TmuxBackend._agent_launch('claude --append-system-prompt "a b c"')
        parts = shlex.split(wrapped)
        self.assertIn('claude --append-system-prompt "a b c"', parts[2])


if __name__ == "__main__":
    unittest.main()
