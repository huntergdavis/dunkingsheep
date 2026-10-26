"""GATE 5's refusal must name the backend the daemon would actually use.

The message defaulted to "herdr" long after tmux became the code default (2.70.29), so a refusal
could name a backend nobody was running - in a message whose entire purpose is to say what to fix.
The name now comes from dunk_terminal, which owns the default.
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_terminal  # noqa: E402


class BackendNameTests(unittest.TestCase):
    def test_the_default_is_tmux(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("DUNKINGSHEEP_TERMINAL_BACKEND", None)
            self.assertEqual("tmux", dunk_terminal.backend_name())

    def test_an_override_is_honoured_and_normalised(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "  HerdR "}):
            self.assertEqual("herdr", dunk_terminal.backend_name())

    def test_an_empty_value_falls_back_to_the_default(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": ""}):
            self.assertEqual("tmux", dunk_terminal.backend_name())

    def test_a_whitespace_only_value_falls_back_to_the_default(self):
        """Reviewer on #40: "   " strips to "", get_backend() maps "" to tmux, so reporting ""
        would name a backend that is not what runs."""
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "   "}):
            self.assertEqual("tmux", dunk_terminal.backend_name())

    def test_the_constant_and_the_resolver_agree(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("DUNKINGSHEEP_TERMINAL_BACKEND", None)
            self.assertEqual(dunk_terminal.DEFAULT_BACKEND_NAME, dunk_terminal.backend_name())

    def test_the_server_asks_for_the_name_instead_of_repeating_it(self):
        """A grep is the honest test here: the bug was a literal default in a second place."""
        source = open(os.path.join(os.path.dirname(os.path.dirname(__file__)),
                                   "dunk_server.py")).read()
        self.assertNotIn('or "herdr"', source)
        self.assertIn("backend_name()", source)

    def test_no_module_carries_its_own_copy_of_the_default(self):
        """TREE-WIDE (reviewer on #40): the first version of this test only looked at
        dunk_server.py, so it could not see dunk_client.py carrying the same literal - where it
        silently disabled the tmux-socket guard on auto-start.

        The legitimate occurrences are named here rather than pattern-excluded, so adding a new
        one is a deliberate act with a line to edit.
        """
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        allowed = {
            # the refusal's own vocabulary: "expected 'tmux' or 'herdr'"
            os.path.join("dunk_terminal.py", "expected 'tmux' or 'herdr'"),
        }
        offenders = []
        for name in sorted(os.listdir(root)):
            if not name.endswith(".py") or name.startswith("test_"):
                continue
            path = os.path.join(root, name)
            if os.path.isdir(path):
                continue
            with open(path, encoding="utf-8", errors="replace") as handle:
                for number, line in enumerate(handle, 1):
                    if 'or "herdr"' not in line:
                        continue
                    if any(name == file and snippet in line for file, snippet in
                           ((f.split(os.sep)[0], f.split(os.sep, 1)[1]) for f in allowed)):
                        continue
                    offenders.append(f"{name}:{number}: {line.strip()}")
        self.assertEqual([], offenders,
                         "a second copy of the backend default drifts from dunk_terminal's")

    def test_the_client_asks_for_the_name_too(self):
        """The copy that mattered: with the variable unset, a client that assumed 'herdr' never
        applied the tmux-socket guard before auto-starting a daemon."""
        source = open(os.path.join(os.path.dirname(os.path.dirname(__file__)),
                                   "dunk_client.py")).read()
        self.assertNotIn('or "herdr"', source)
        self.assertIn("backend_name", source)


if __name__ == "__main__":
    unittest.main()


class SpawnGuardOrderingTests(unittest.TestCase):
    """The tmux-socket guard belongs to the SPAWN path, not the systemd handoff.

    The historical failure was a client SPAWNING a trial daemon with a guessed environment. A unit
    started by systemd carries its own environment, so requiring DUNKINGSHEEP_TMUX_SOCKET in the
    caller would refuse the handoff to a correctly configured daemon - a fleet-wide outage caused
    by a fix. Ordering is the whole test.
    """

    def _source(self):
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        return open(os.path.join(root, "dunk_client.py"), encoding="utf-8").read()

    def test_the_systemd_handoff_comes_before_the_spawn_guard(self):
        source = self._source()
        body = source[source.index("def start_daemon"):]
        handoff = body.index("_systemd_installed()")
        guard = body.index('not os.environ.get("DUNKINGSHEEP_TMUX_SOCKET")')
        self.assertLess(handoff, guard,
                        "the guard must not refuse a handoff to the supervised unit")

    def test_the_guard_still_guards_the_spawn_path(self):
        source = self._source()
        body = source[source.index("def start_daemon"):]
        guard = body.index('not os.environ.get("DUNKINGSHEEP_TMUX_SOCKET")')
        spawn = body.index("subprocess.Popen(")
        self.assertLess(guard, spawn, "the guard must still run before a spawned daemon")

    def test_the_guard_refuses_a_spawn_without_the_socket(self):
        """Behavioural, not just positional: an unsupervised client with no socket refuses."""
        import sys as _sys
        _sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        from dunk_client import DaemonUnavailable, FlockClient
        client = FlockClient(socket_path="/tmp/definitely-not-the-default.sock",
                             self_pane_id=None)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("DUNKINGSHEEP_TMUX_SOCKET", None)
            os.environ.pop("DUNKINGSHEEP_TERMINAL_BACKEND", None)
            with self.assertRaises(DaemonUnavailable) as caught:
                client.start_daemon()
        self.assertIn("DUNKINGSHEEP_TMUX_SOCKET", str(caught.exception))
