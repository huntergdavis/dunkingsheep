"""The tmux pane separator must survive tmux 3.4 and free text (t1605, t3357 B1).

tmux 3.4 writes a control character in format output as a backslash escape: the 0x1f separator
came back as the four characters \\037, so the parser split nothing and the daemon saw zero panes
on the runner. The separator is therefore printable ASCII. A fixed printable token is not enough
either: a pane title, cwd, command or name can carry the same text and shift every later field.
So the token is random per process, and these tests pin its shape, check that two processes draw
different tokens, and check that titles and cwds carrying the literal old token keep their fields.
"""
import os
import re
import subprocess
import sys
import unittest

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO)

import dunk_terminal  # noqa: E402

SEP = dunk_terminal.TmuxBackend.PANE_SEP
TOKEN_SHAPE = re.compile(r"@@ds[0-9a-f]{8}@@")


def control_characters(text):
    return [c for c in text if ord(c) < 0x20 or ord(c) == 0x7F]


def make_backend():
    backend = dunk_terminal.TmuxBackend.__new__(dunk_terminal.TmuxBackend)
    backend.socket_name = None
    backend.registry_path = "/nonexistent/registry.json"
    backend._load_registry = lambda: {"panes": {}}
    return backend


def pane_line(sep, cwd="/work/app", title="lane-seven", session="cos", window="tab"):
    return sep.join([
        "%7", "$3", "@4", cwd, "claude", title, "1",
        session, window, "2", "1", "9911", "200", "50",
    ])


class SeparatorShape(unittest.TestCase):
    def test_the_separator_is_a_random_token_of_the_pinned_shape(self):
        self.assertIsNotNone(TOKEN_SHAPE.fullmatch(SEP), repr(SEP))

    def test_every_character_of_the_separator_is_printable_ascii(self):
        self.assertTrue(all(0x21 <= ord(c) <= 0x7E for c in SEP), repr(SEP))

    def test_each_process_draws_its_own_token(self):
        child = subprocess.run(
            [sys.executable, "-c",
             "import dunk_terminal; print(dunk_terminal.TmuxBackend.PANE_SEP)"],
            cwd=REPO, capture_output=True, text=True, timeout=120, check=True)
        other = child.stdout.strip()
        self.assertIsNotNone(TOKEN_SHAPE.fullmatch(other), repr(other))
        self.assertNotEqual(SEP, other)

    def test_no_format_string_carries_a_control_character(self):
        backend = dunk_terminal.TmuxBackend
        for name, fmt in (("PANE_FORMAT", backend.PANE_FORMAT),
                          ("TAB_FORMAT", backend.TAB_FORMAT),
                          ("WORKSPACE_FORMAT", backend.WORKSPACE_FORMAT)):
            self.assertEqual([], control_characters(fmt), name)

    def test_the_pane_format_still_has_fourteen_fields(self):
        self.assertEqual(14, len(dunk_terminal.TmuxBackend.PANE_FORMAT.split(SEP)))


class ParseAFullPaneLine(unittest.TestCase):
    def test_fourteen_fields_parse_as_one_pane(self):
        panes = make_backend()._parse_panes(pane_line(SEP))
        self.assertEqual(1, len(panes))
        pane = panes[0]
        self.assertEqual("%7", pane["pane_id"])
        self.assertEqual(9911, pane["pane_pid"])
        self.assertEqual("/work/app", pane["cwd"])
        self.assertEqual((200, 50), (pane["cols"], pane["rows"]))
        self.assertTrue(pane["focused"])

    def test_several_lines_each_keep_their_own_pane(self):
        backend = make_backend()
        rows = [
            backend.PANE_SEP.join(["%1", "$1", "@1", "/a", "sh", "one", "0",
                                   "s", "w", "0", "0", "101", "80", "24"]),
            backend.PANE_SEP.join(["%2", "$1", "@1", "/b", "sh", "two", "1",
                                   "s", "w", "0", "0", "102", "80", "24"]),
        ]
        panes = backend._parse_panes("\n".join(rows))
        self.assertEqual(["%1", "%2"], [pane["pane_id"] for pane in panes])


class FreeTextThatCarriesTheOldToken(unittest.TestCase):
    """t3357 B1: a title or cwd that holds the literal old token shifted every later field."""

    def test_a_title_with_the_literal_token_keeps_every_field(self):
        panes = make_backend()._parse_panes(pane_line(SEP, title="Review PR752 @@ds@@ fix"))
        self.assertEqual(1, len(panes))
        pane = panes[0]
        self.assertEqual(9911, pane["pane_pid"])
        self.assertEqual((200, 50), (pane["cols"], pane["rows"]))
        self.assertTrue(pane["focused"])
        self.assertEqual("Review PR752 @@ds@@ fix", pane["terminal_title"])
        self.assertEqual("Review PR752 @@ds@@ fix", pane["label"])
        self.assertEqual("cos", pane["workspace_label"])
        self.assertEqual("tab", pane["tab_label"])

    def test_a_cwd_with_the_literal_token_keeps_every_field(self):
        panes = make_backend()._parse_panes(pane_line(SEP, cwd="/work/@@ds@@/app"))
        self.assertEqual(1, len(panes))
        pane = panes[0]
        self.assertEqual("/work/@@ds@@/app", pane["cwd"])
        self.assertEqual(9911, pane["pane_pid"])
        self.assertEqual((200, 50), (pane["cols"], pane["rows"]))
        self.assertTrue(pane["focused"])

    def test_a_name_with_a_token_shaped_string_keeps_every_field(self):
        panes = make_backend()._parse_panes(
            pane_line(SEP, session="cos @@ds1a2b3c4d@@", window="tab @@ds@@"))
        self.assertEqual(1, len(panes))
        pane = panes[0]
        self.assertEqual("cos @@ds1a2b3c4d@@", pane["workspace_label"])
        self.assertEqual("tab @@ds@@", pane["tab_label"])
        self.assertEqual(9911, pane["pane_pid"])
        self.assertEqual((200, 50), (pane["cols"], pane["rows"]))


if __name__ == "__main__":
    unittest.main()
