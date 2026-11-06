"""The tmux pane separator must survive tmux 3.4 (t1605).

tmux 3.4 writes a control character in format output as a backslash escape: the 0x1f separator
came back as the four characters \\037, so the parser split nothing and the daemon saw zero panes
on the runner. The separator is therefore printable ASCII, and these tests keep it that way.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_terminal


def control_characters(text):
    return [c for c in text if ord(c) < 0x20 or ord(c) == 0x7F]


class SeparatorIsPrintable(unittest.TestCase):
    def test_the_separator_is_the_printable_token(self):
        self.assertEqual("@@ds@@", dunk_terminal.TmuxBackend.PANE_SEP)

    def test_every_character_of_the_separator_is_printable_ascii(self):
        sep = dunk_terminal.TmuxBackend.PANE_SEP
        self.assertTrue(all(0x21 <= ord(c) <= 0x7E for c in sep), repr(sep))

    def test_no_format_string_carries_a_control_character(self):
        backend = dunk_terminal.TmuxBackend
        for name, fmt in (("PANE_FORMAT", backend.PANE_FORMAT),
                          ("TAB_FORMAT", backend.TAB_FORMAT),
                          ("WORKSPACE_FORMAT", backend.WORKSPACE_FORMAT)):
            self.assertEqual([], control_characters(fmt), name)

    def test_the_pane_format_still_has_fourteen_fields(self):
        sep = dunk_terminal.TmuxBackend.PANE_SEP
        self.assertEqual(14, len(dunk_terminal.TmuxBackend.PANE_FORMAT.split(sep)))


class ParseAFullPaneLine(unittest.TestCase):
    def backend(self):
        backend = dunk_terminal.TmuxBackend.__new__(dunk_terminal.TmuxBackend)
        backend.socket_name = None
        backend.registry_path = "/nonexistent/registry.json"
        backend._load_registry = lambda: {"panes": {}}
        return backend

    def test_fourteen_fields_joined_with_the_separator_parse_as_one_pane(self):
        backend = self.backend()
        line = backend.PANE_SEP.join([
            "%7", "$3", "@4", "/work/app", "claude", "lane-seven", "1",
            "cos", "tab", "2", "1", "9911", "200", "50",
        ])
        panes = backend._parse_panes(line)
        self.assertEqual(1, len(panes))
        pane = panes[0]
        self.assertEqual("%7", pane["pane_id"])
        self.assertEqual(9911, pane["pane_pid"])
        self.assertEqual("/work/app", pane["cwd"])

    def test_several_lines_each_keep_their_own_pane(self):
        backend = self.backend()
        rows = [
            backend.PANE_SEP.join(["%1", "$1", "@1", "/a", "sh", "one", "0",
                                   "s", "w", "0", "0", "101", "80", "24"]),
            backend.PANE_SEP.join(["%2", "$1", "@1", "/b", "sh", "two", "1",
                                   "s", "w", "0", "0", "102", "80", "24"]),
        ]
        panes = backend._parse_panes("\n".join(rows))
        self.assertEqual(["%1", "%2"], [pane["pane_id"] for pane in panes])


if __name__ == "__main__":
    unittest.main()
