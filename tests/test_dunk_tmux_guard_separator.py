"""The tmux guard reads pane ids with a separator tmux 3.4 cannot escape (t1605, t3238).

tmux 3.4 writes a control character in format output as a backslash escape. A tab would come back
as \\011, so the guard would parse no panes and refuse every close. The guard's separator is a
printable token drawn per process. These tests run with subprocess patched, so no tmux is touched.
"""
import os
import re
import subprocess
import sys
import unittest
from types import SimpleNamespace
from unittest import mock

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)

import dunk_tmux_guard  # noqa: E402

SEP = dunk_tmux_guard.PANE_SEP
TOKEN_SHAPE = re.compile(r"@@dg[0-9a-f]{8}@@")
GUARD_COPIES = (
    os.path.join(REPO, "dunk_tmux_guard.py"),
    os.path.join(REPO, "rust", "crates", "ds-cli", "guard", "dunk_tmux_guard.py"),
)


def control_characters(text):
    return [c for c in text if ord(c) < 0x20 or ord(c) == 0x7F]


def listing(stdout, returncode=0):
    """Patch the guard's tmux call. Returns the mock so tests can read the argv it was given."""
    fake = mock.Mock(return_value=SimpleNamespace(returncode=returncode, stdout=stdout))
    return fake


class SeparatorShape(unittest.TestCase):
    def test_the_separator_is_a_random_token_of_the_pinned_shape(self):
        self.assertIsNotNone(TOKEN_SHAPE.fullmatch(SEP), repr(SEP))

    def test_every_character_of_the_separator_is_printable_ascii(self):
        self.assertTrue(all(0x21 <= ord(c) <= 0x7E for c in SEP), repr(SEP))


class ListingAndParsing(unittest.TestCase):
    def test_the_listing_asks_for_ids_joined_by_the_separator(self):
        fake = listing("")
        with mock.patch.object(dunk_tmux_guard, "real_tmux", return_value="/fake/tmux"), \
                mock.patch.object(dunk_tmux_guard.subprocess, "run", fake):
            dunk_tmux_guard.tmux_panes()
        argv = fake.call_args[0][0]
        fmt = argv[argv.index("-F") + 1]
        self.assertEqual(SEP.join(["#{pane_id}", "#{session_id}", "#{window_id}"]), fmt)
        self.assertEqual([], control_characters(fmt))

    def test_the_parser_reads_three_ids_per_pane(self):
        out = "\n".join([
            SEP.join(["%1", "$1", "@1"]),
            SEP.join(["%2", "$1", "@2"]),
        ]) + "\n"
        with mock.patch.object(dunk_tmux_guard, "real_tmux", return_value="/fake/tmux"), \
                mock.patch.object(dunk_tmux_guard.subprocess, "run", listing(out)):
            panes = dunk_tmux_guard.tmux_panes()
        self.assertEqual(
            [{"pane_id": "%1", "workspace_id": "$1", "tab_id": "@1"},
             {"pane_id": "%2", "workspace_id": "$1", "tab_id": "@2"}],
            panes)

    def test_an_escaped_listing_yields_no_panes_and_the_guard_refuses_the_close(self):
        """If the separator ever arrives escaped (the tmux 3.4 behaviour), the guard fails closed."""
        escaped = "%1\\011$1\\011@1\n"
        with mock.patch.object(dunk_tmux_guard, "real_tmux", return_value="/fake/tmux"), \
                mock.patch.object(dunk_tmux_guard.subprocess, "run", listing(escaped)):
            panes = dunk_tmux_guard.tmux_panes()
        self.assertEqual([], panes)
        decision = dunk_tmux_guard.close_decision(
            "pane", "%1", panes=panes, protected=(), managed=("%1",))
        self.assertFalse(decision.allowed)


class CopiesStayInStep(unittest.TestCase):
    def test_the_two_copies_of_the_listing_code_are_identical(self):
        blocks = []
        for path in GUARD_COPIES:
            with open(path, encoding="utf-8") as handle:
                text = handle.read()
            start = text.index("# The guard reads pane ids")
            end = text.index("def _target_panes")
            blocks.append(text[start:end])
        self.assertEqual(blocks[0], blocks[1])


if __name__ == "__main__":
    unittest.main()
