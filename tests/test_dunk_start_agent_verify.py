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

    def _start(self, screen, proc=None):
        # The fake creates the pane and sets its screen; start_agent then verifies it.
        # proc=None means 'no process table on this platform' (screen-only, as before);
        # otherwise child dicts {argv, comm, state, start}, or per-call frames of them.
        self.herdr.screen_after_create = screen
        self.herdr.proc_after_create = proc
        return self.flock.start_agent("probe", "claude", cwd="/tmp")

    @staticmethod
    def _child(argv, comm=None, state="S", start=2000):
        entry = {"argv": list(argv), "state": state, "start": start}
        entry["comm"] = comm if comm is not None else os.path.basename(argv[0])
        return entry

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

    def test_a_benign_startup_line_containing_not_found_still_starts(self):
        """The control the reviewer asked for: an agent's own 'not found' is not a refusal."""
        self.herdr.screen_after_create = ("\u273b theme cache not found, rebuilding\n"
                                          "\u273b ready\n\u276f \n")
        result = self._start(self.herdr.screen_after_create)
        self.assertEqual("probe", result["name"])

    def test_shell_shaped_failures_are_still_refused(self):
        for line in ("/bin/sh: 1: opencode: not found\n",
                     "bash: opencode: command not found\n",
                     "sh: cannot execute opencode\n"):
            self.herdr.screen_after_create = line
            with self.assertRaisesRegex(DunkError, "never ran|not found|cannot execute"):
                self._start(line)

    def test_the_refusal_names_the_pane_it_left_behind(self):
        self.herdr.screen_after_create = "/bin/sh: 1: opencode: not found\n"
        with self.assertRaises(DunkError) as caught:
            self._start(self.herdr.screen_after_create)
        self.assertRegex(str(caught.exception), r"w\d+:p\d+|%\d+")

    def test_a_pane_that_vanishes_is_refused(self):
        self.herdr.screen_after_create = "starting...\n"
        self.herdr.vanish_panes = True
        with self.assertRaisesRegex(DunkError, "already gone"):
            self._start(self.herdr.screen_after_create)

    def test_a_clean_screen_with_no_process_is_refused(self):
        """Criterion (a): a bare idle shell is not a running agent where pids exist."""
        with self.assertRaisesRegex(DunkError, "never became a process"):
            self._start("$ \n", proc=[])

    def test_a_matching_child_is_confirmed_with_its_pid(self):
        """Kernel comm match, clean screen across the window: started with the pid."""
        result = self._start("$ \n", proc=[self._child(
            ["/usr/bin/claude", "--model", "opus"], comm="claude", state="R")])
        self.assertEqual("probe", result["name"])
        self.assertIsNotNone(result["child_pid"])
        self.assertEqual(self.herdr.panes[-1]["pane_pid"] + 1, result["child_pid"])

    def test_early_match_then_exit_line_is_refused(self):
        """P1 counterexample: blank first frame with a live pid, then rc=127.
        The pid sighting is recorded, never returned early."""
        with self.assertRaisesRegex(DunkError, "exited immediately"):
            self._start(["$ \n", "\n[agent exited rc=127]\n"],
                        proc=[self._child(["/usr/bin/claude"], comm="claude")])

    def test_transient_pid_then_gone_is_refused(self):
        """P1 revalidation: seen on the first frame, gone at the deadline."""
        frames = [[self._child(["/usr/bin/claude"], comm="claude")], []]
        with self.assertRaisesRegex(DunkError, "gone at the end"):
            self._start("$ \n", proc=frames)

    def test_argv_spoof_is_not_identity(self):
        """P2 counterexample: /bin/sleep with argv0=claude must not match."""
        entries = [{"pid": 101, "comm": "sleep", "state": "S", "start": 2000,
                    "argv": ["claude", "--model", "opus"]}]
        self.assertEqual([], Flock._match_agent_process(entries, "claude"))

    def test_stopped_and_zombie_are_not_evidence(self):
        """P2: a stopped process cannot provide the claimed working agent."""
        for state in ("T", "t", "Z", "X", "x"):
            entries = [{"pid": 101, "comm": "claude", "state": state,
                        "start": 2000, "argv": ["/usr/bin/claude"]}]
            self.assertEqual([], Flock._match_agent_process(entries, "claude"),
                              f"state {state} must not match")

    def test_stale_generation_is_not_evidence(self):
        """P2: a leftover pid older than the pane is not this launch."""
        entries = [{"pid": 101, "comm": "claude", "state": "R", "start": 500,
                    "argv": ["/usr/bin/claude"]}]
        self.assertEqual([], Flock._match_agent_process(entries, "claude",
                                                        since_start=1000))
        self.assertEqual([101], Flock._match_agent_process(entries, "claude",
                                                           since_start=500))
        # Unknown generation skips the clause rather than refusing blind.
        self.assertEqual([101], Flock._match_agent_process(entries, "claude"))

    def test_kernel_comm_match_holds(self):
        entries = [{"pid": 11, "comm": "bash", "state": "S", "start": 1000,
                    "argv": ["bash"]},
                   {"pid": 12, "comm": "claude", "state": "R", "start": 2000,
                    "argv": ["/usr/bin/claude"]}]
        self.assertEqual([12], Flock._match_agent_process(entries, "claude"))
        self.assertEqual([12], Flock._match_agent_process(entries, "Claude"))
        self.assertEqual([], Flock._match_agent_process(entries, "sh"))
        self.assertEqual([], Flock._match_agent_process([], "claude"))

    def test_no_process_table_stays_screen_only(self):
        """Without a table the old contract holds: banner starts, child_pid is None."""
        result = self._start("✻ Welcome to Claude Code\n❯ \n", proc=None)
        self.assertEqual("probe", result["name"])
        self.assertIsNone(result["child_pid"])


if __name__ == "__main__":
    unittest.main()
