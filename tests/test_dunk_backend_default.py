"""t50 step 1: tmux is the default backend; herdr is an explicit choice.

The flip used to depend on one systemd drop-in setting DUNKINGSHEEP_TERMINAL_BACKEND.
Any unit that lost that drop-in silently moved the daemon back onto herdr - a backend
with no agents left on it (herdr pane list: 47 panes, 0 agents). A default that needs a
drop-in to be correct is not a default.
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_terminal import HerdrBackend, TmuxBackend, get_backend  # noqa: E402


class BackendDefaultTests(unittest.TestCase):
    def test_unset_env_is_tmux(self):
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("DUNKINGSHEEP_TERMINAL_BACKEND", None)
            self.assertIsInstance(get_backend(), TmuxBackend)

    def test_an_empty_value_is_tmux_too(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": ""}):
            self.assertIsInstance(get_backend(), TmuxBackend)

    def test_herdr_is_still_selectable_for_the_rollback(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "herdr"}):
            self.assertIsInstance(get_backend(), HerdrBackend)

    def test_tmux_can_still_be_named_explicitly(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "tmux"}):
            self.assertIsInstance(get_backend(), TmuxBackend)

    def test_an_unknown_backend_is_a_named_refusal(self):
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "screen"}):
            with self.assertRaisesRegex(ValueError, "expected 'tmux' or 'herdr'"):
                get_backend()


if __name__ == "__main__":
    unittest.main()
