"""The live-server guard must fire for BOTH ways tmux says "no server".

reviewer pi-review-1 on PR #37: the predicate matched only "no server running", which is what
tmux says when a socket exists with no server. A FRESH socket says

    error connecting to /tmp/tmux-1000/dsh-test-abc (No such file or directory)

and exits 1 - so the four live tests still failed red in the bare sandbox the guard exists for.
This feeds both spellings (and the exit-0 case) straight to the predicate, with no tmux needed.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import test_terminal  # noqa: E402

# Bound through the module, not by importing the class: importing TmuxLiveTests into this
# namespace would make unittest discover the live tests a second time.
server_missing = test_terminal.TmuxLiveTests._server_missing


class RequireServerPredicateTests(unittest.TestCase):
    def test_a_fresh_socket_is_a_missing_server(self):
        err = ("error connecting to /tmp/tmux-1000/dsh-test-abc "
               "(No such file or directory)\n")
        self.assertTrue(server_missing(1, err))

    def test_an_existing_socket_with_no_server_is_a_missing_server(self):
        self.assertTrue(server_missing(1, "no server running on /tmp/tmux-1000/default\n"))

    def test_silent_failure_is_a_missing_server(self):
        self.assertTrue(server_missing(1, ""))

    def test_a_working_server_is_not_a_missing_server(self):
        self.assertFalse(server_missing(0, "main: 1 windows (created Mon)\n"))

    def test_a_real_error_still_skips_rather_than_asserts(self):
        # A permission error is also "no server I can use"; the reason carries the text.
        self.assertTrue(server_missing(1, "permission denied"))


if __name__ == "__main__":
    unittest.main()
