"""codex's composer: a transcript line is not a draft, and a hint means empty.

Captured from pane %17 on 2026-10-24 after two sends (m1843, m1844) were held for
5+ minutes with "someone is typing" while `delivery %17` said ready and the lane
sat idle for eight hours.

The captured screen is real, and it is the bug: codex echoes the user's message
into the transcript with the SAME '›' sigil its composer uses, so the reader took a
wrapped line of the previous instruction for a human draft. Two rows above the
status footer codex draws a bare hint ('tab to queue message') - no sigil - which
is decisive evidence that nothing has been typed.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

FIXTURE = os.path.join(os.path.dirname(__file__), "fixtures",
                       "codex-pane17-transcript-plus-hint.ansi")


def captured():
    with open(FIXTURE, encoding="utf-8") as handle:
        return handle.read()


class CodexComposerTests(unittest.TestCase):
    def test_the_captured_screen_reads_as_an_empty_box(self):
        screen = captured()
        text, detail = dunk_core.box_text(screen)
        self.assertEqual("", text, f"the transcript was read as a draft: {detail}")
        self.assertTrue(detail, "the reader must say why the box is empty")

    def test_the_captured_screen_is_deliverable_and_not_foreign(self):
        screen = captured()
        state = dunk_core.delivery_state_for_screen(screen, "idle") \
            if hasattr(dunk_core, "delivery_state_for_screen") else None
        self.assertIsNone(state)          # the helper does not exist; see below
        typed = dunk_core._typed_text(screen)
        self.assertEqual("", typed)
        self.assertEqual(("ready", None), dunk_core.classify_delivery(screen, "idle", typed=typed))

    def test_both_codex_hints_count_as_empty(self):
        for hint in ("Ask Codex to do anything", "tab to queue message", "tab to send"):
            row = f"  \x1b[2m{hint}\x1b[0m"
            self.assertTrue(dunk_core._is_placeholder(row), hint)

    def test_a_bordered_draft_is_unaffected(self):
        """The codex fix must not change how a BORDERED composer reads: that path is
        covered by tests/test_dunk_delivery.py (37 fixtures, green), and this test
        exists so the coupling is visible in one place rather than assumed."""
        screen = "\n".join([
            "\u2502 \u276f check your inbox                     \u2502",
            "\u2500" * 30,
        ])
        self.assertEqual("check your inbox", dunk_core.box_text(screen)[0])


if __name__ == "__main__":
    unittest.main()
