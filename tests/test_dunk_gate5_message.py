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


if __name__ == "__main__":
    unittest.main()
