"""t92: identity when the environment is stripped or lying about the terminal.

codex's command runner strips TMUX_PANE, so every dunkingsheep call from a codex
lane reported self_pane_id null. The TTY knows; the environment may not, and when
the two disagree the terminal wins and the claim is refused rather than preferred.
"""

import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_client import IdentityRefused, resolve_identity  # noqa: E402


def no_detection():
    return None


def lookup(pane=None, note="test", error=None):
    def _lookup():
        if error:
            raise error
        return pane, note
    return _lookup


class IdentityTests(unittest.TestCase):
    def test_a_stripped_env_resolves_by_the_terminal(self):
        """The codex case: no TMUX_PANE, a real pane behind the pty."""
        pane, note = resolve_identity(environ={}, tty_lookup=lookup("%17", "resolved by /dev/pts/66"))
        self.assertEqual("%17", pane)
        self.assertIn("terminal", note)

    def test_a_forged_claim_that_disagrees_with_the_terminal_is_refused(self):
        with self.assertRaisesRegex(IdentityRefused, "refused"):
            resolve_identity(environ={"TMUX_PANE": "%9"},
                             tty_lookup=lookup("%4", "resolved by /dev/pts/58"))

    def test_an_agreeing_claim_is_accepted_and_said_so(self):
        pane, note = resolve_identity(environ={"TMUX_PANE": "%4"},
                                      tty_lookup=lookup("%4", "resolved by /dev/pts/58"))
        self.assertEqual("%4", pane)
        self.assertIn("confirmed", note)

    def test_an_ambiguous_terminal_is_refused_not_guessed(self):
        with self.assertRaisesRegex(IdentityRefused, "refused"):
            resolve_identity(environ={}, tty_lookup=lookup(None, "/dev/pts/70 is claimed by 2 panes"))

    def test_a_systemd_service_is_anonymous_not_refused(self):
        """A unit has no pane and no pty. Raising there took dispatch, health, state,
        watchdog, deadman and the desk down (VP i24050, reproduced with
        systemd-run --user): an absent identity is not a lying one."""
        pane, note = resolve_identity(environ={}, detect=no_detection,
                                      tty_lookup=lookup(None, "no controlling terminal"))
        self.assertIsNone(pane)
        self.assertIn("anonymous", note)

    def test_the_cli_answers_inside_a_real_systemd_unit(self):
        """The reviewer's reproduction: a systemd user unit has no pane and no pty.
        Run for real when systemd-run is available, because a subprocess started by
        an agent still inherits the agent in its parent chain, which is exactly the
        part a unit does not have.
        """
        import shutil
        import subprocess
        if not shutil.which("systemd-run"):
            self.skipTest("systemd-run is not available here")
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        # `inbox count` needs a mailbox when the caller has no lane of its own - that
        # is pre-existing behaviour and not part of this change, so the unit case
        # names one explicitly rather than asserting that a service has a mailbox.
        for argv in (["status"], ["panes"], ["inbox", "count", "nobody"]):
            proc = subprocess.run(
                ["systemd-run", "--user", "--wait", "--pipe", "--quiet",
                 "--setenv", "DUNKINGSHEEP_TTY_IDENTITY=1",
                 sys.executable, os.path.join(root, "dunkingsheep"), *argv],
                capture_output=True, text=True, timeout=180)
            self.assertEqual(0, proc.returncode, (proc.stdout or "") + (proc.stderr or ""))
            self.assertNotIn("identity refused", proc.stderr or "")

    def test_a_terminal_with_no_pane_is_refused(self):
        with self.assertRaisesRegex(IdentityRefused, "refused"):
            resolve_identity(environ={}, tty_lookup=lookup(None, "no tmux pane owns /dev/pts/70"))

    def test_an_unreadable_claim_with_no_terminal_keeps_the_claim(self):
        """A host without tmux, or a runner with no pty: refuse to INVENT an
        identity, but do not throw away the one the terminal exported."""
        pane, note = resolve_identity(environ={"TMUX_PANE": "%5"},
                                      tty_lookup=lookup(None, "no controlling terminal"))
        self.assertEqual("%5", pane)
        self.assertIn("env only", note)

    def test_the_tty_fallback_confers_no_capability(self):
        """Identity, not authority: the note says which it was, and nothing here
        can make a lane a superuser or a drainer."""
        _, note = resolve_identity(environ={}, tty_lookup=lookup("%17", "resolved by /dev/pts/66"))
        self.assertNotIn("superuser", note)
        self.assertNotIn("drains", note)

    def test_an_explicit_pane_wins(self):
        pane, note = resolve_identity(explicit="%2", environ={"TMUX_PANE": "%9"},
                                      tty_lookup=lookup("%4"))
        self.assertEqual("%2", pane)
        self.assertEqual("explicit", note)


class RealTerminalTests(unittest.TestCase):
    def test_this_process_chain_finds_its_pty(self):
        """Measured on the host: fd 0 is a pipe under the harness, while the agent
        process above it owns the pane's pty - so the search must walk parents."""
        from dunk_client import controlling_tty
        tty = controlling_tty()
        if tty is None:
            self.skipTest("no controlling terminal in this environment")
        self.assertRegex(tty, r"^/dev/pts/\d+$")

    @unittest.skipUnless(os.environ.get("TMUX_PANE"), "not running inside tmux")
    def test_this_lane_resolves_to_its_own_pane(self):
        pane, _note = resolve_identity(environ=dict(os.environ))
        self.assertEqual(os.environ["TMUX_PANE"], pane)


if __name__ == "__main__":
    unittest.main()
