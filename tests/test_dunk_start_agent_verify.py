"""t215: start-agent must FAIL CLOSED - a pane is not a running agent.

Eight opencode panes were reported "started agent ..." and had actually died with
'opencode: not found' (rc=127) because the pane's PATH lacked /snap/bin. The pane existed, so
the old code believed the agent did. Now the pane's own first lines are read before success is
claimed, and a missing command is a refusal with the captured line.
"""

import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_terminal import TmuxBackend  # noqa: E402
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

    def test_a_launcher_noise_line_does_not_refuse_a_live_pi_lane(self):
        """t599, measured live: a pi lane's first screen carries its launcher's own
        ENOENT/sed noise. The old order raised on that text BEFORE looking at the
        process table, so the start was refused, lane_register never ran, and the
        owner registered %99/%101 by hand. Process evidence wins over the proxy."""
        self.herdr.screen_after_create = ("sed: can't read /home/hunter/.pi/settings.json: "
                                          "No such file or directory\n\n\u276f ready\n")
        self.herdr.proc_after_create = [self._child(
            ["/home/hunter/.local/bin/pi", "--offline"], comm="pi", state="S")]
        result = self.flock.start_agent("probe", "pi", cwd="/tmp")
        self.assertEqual("probe", result["name"])
        self.assertIsNotNone(result["child_pid"])

    def test_the_same_line_without_a_live_process_is_still_refused(self):
        """The guard t599 must not weaken: text plus no process is a refusal."""
        screen = ("sed: can't read /home/hunter/.pi/settings.json: "
                  "No such file or directory\n")
        with self.assertRaisesRegex(DunkError, "never ran|No such file"):
            self._start(screen, proc=[])

    def test_an_exit_line_still_beats_a_pid_sighting(self):
        """t215 REVISE P1 must survive t599: an explicit rc line is a refusal."""
        with self.assertRaisesRegex(DunkError, "exited immediately"):
            self._start(["$ \n", "\n[agent exited rc=127]\n"],
                        proc=[self._child(["/usr/bin/claude"], comm="claude")])

    def test_a_first_run_trust_dialog_is_refused_and_named(self):
        """t957: a lane that reported 'started' and was inert behind a dialog.

        Measured by the owner-role lane: start-agent said 'started agent reviewer-7 in
        %110', the role registered, then `read %110` was blank, a dunk sat at 'Held
        (typing)' and the stuck machine logged 'the composer text is not provably
        ours'. The pane was up; the CLI was waiting for a dialog nobody answered.
        """
        screen = ("Do you trust the files in this folder?\n"
                  "❯ 1. Yes, proceed\n  2. No, exit\n")
        with self.assertRaisesRegex(DunkError, "NOT interactive.*trust the files"):
            self._start(screen)

    def test_a_first_run_login_dialog_is_refused_too(self):
        screen = "Select login method:\n❯ 1. Claude account\n  2. API key\n"
        with self.assertRaisesRegex(DunkError, "NOT interactive.*login method"):
            self._start(screen)

    def test_the_dialog_refusal_wins_over_a_live_process(self):
        """A live claude sitting in a trust prompt is exactly the inert lane: the
        process evidence must not excuse it (unlike launcher noise, t599)."""
        with self.assertRaisesRegex(DunkError, "NOT interactive"):
            self._start("Do you trust the files in this folder?\n",
                        proc=[self._child(["claude"])])

    def test_a_welcome_screen_with_no_dialog_still_starts(self):
        result = self._start("✻ Welcome to Claude Code\n❯ \n")
        self.assertEqual("probe", result["name"])

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


@unittest.skipUnless(os.path.isdir("/proc"), "needs a process table")
class TmuxBackendContractTests(unittest.TestCase):
    """P1 follow-up (t418): the DEFAULT backend must check processes.

    The actual TmuxBackend class with only terminal IO mocked - no server, no
    pane creation, real /proc reads. Before the wiring, hasattr failed and a
    clean screen silently passed screen-only on the backend production runs.
    """

    DEAD_PID = 4194304  # above any real pid: dead by construction

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.backend = TmuxBackend(
            tmux_bin="/bin/false",
            registry_path=os.path.join(self.tmp.name, "tmux-panes.json"))
        self.flock = Flock(herdr=self.backend, persist=False, restore=False)
        self.flock.AGENT_VERIFY_S = 0.3
        self.screen = "$ \n"
        self.pane_pid = self.DEAD_PID
        patcher = mock.patch.object(self.backend, "_tmux",
                                    side_effect=self._fake_tmux)
        patcher.start()
        self.addCleanup(patcher.stop)

    def tearDown(self):
        self.flock.close()

    def _pane_line(self):
        fields = ["%9", "$1", "@1", "/tmp", "bash", "shell", "1",
                  "s", "w", "0", "0", str(self.pane_pid), "80", "24"]
        return TmuxBackend.PANE_SEP.join(fields)

    def _fake_tmux(self, *args, timeout=None):
        if not args:
            return 1, "", ""
        if args[0] == "list-panes":
            return 0, self._pane_line() + "\n", ""
        if args[0] == "capture-pane":
            return 0, self.screen, ""
        return 1, "", ""

    def test_default_backend_refuses_clean_screen_with_no_process(self):
        with self.assertRaisesRegex(DunkError, "never became a process"):
            self.flock._verify_agent_started("probe", "%9", "claude")

    def test_default_backend_confirms_a_live_kernel_match(self):
        with open("/proc/self/comm", encoding="utf-8") as handle:
            kind = handle.read().strip()
        self.pane_pid = os.getpid()
        pid = self.flock._verify_agent_started("probe", "%9", kind)
        self.assertEqual(os.getpid(), pid)


if __name__ == "__main__":
    unittest.main()
