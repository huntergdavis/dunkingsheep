"""Drafts vs echoes in codex: the composer is the prompt row that touches the footer.

Five frames are now captured, and the two that broke in the DANGEROUS direction came
last: a human's 3-line draft read as an empty box would have typed over it. The
discriminator is structural and fits all five:

  idle echo frame      last prompt 19, footer 22, 2 blank rows -> transcript, empty
  transcript+hint      last prompt 15, footer 22, 2 blank rows -> transcript, empty
  busy lane            last prompt 20, footer 23, 1 blank row  -> composer (hint)
  working + draft x3   last prompt 34, footer 38, 1 blank row  -> composer (draft)
  idle + draft x3      last prompt 34, footer 38, 1 blank row  -> composer (draft)
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

HERE = os.path.dirname(__file__)


def captured(name):
    with open(os.path.join(HERE, "fixtures", name), encoding="utf-8") as handle:
        return handle.read()


class EchoTests(unittest.TestCase):
    def test_the_idle_echo_frame_is_empty(self):
        text, detail = dunk_core.box_text(captured("codex-idle-footer-pane17.ansi"))
        self.assertEqual("", text, detail)

    def test_the_transcript_plus_hint_frame_is_empty(self):
        text, detail = dunk_core.box_text(
            captured("codex-pane17-transcript-plus-hint.ansi"))
        self.assertEqual("", text, detail)

    def test_the_busy_frame_is_empty(self):
        text, detail = dunk_core.box_text(captured("codex-busy-frame-pane17.ansi"))
        self.assertEqual("", text, detail)


class HumanDraftTests(unittest.TestCase):
    """The direction that matters: a false 'empty' types over a person."""

    def test_a_working_lane_with_a_three_line_draft_is_a_draft(self):
        text, detail = dunk_core.box_text(captured("codex-working-human-draft3.ansi"))
        self.assertEqual("my human draft line one", text, detail)

    def test_an_idle_lane_with_a_three_line_draft_is_a_draft(self):
        text, detail = dunk_core.box_text(captured("codex-idle-human-draft3.ansi"))
        self.assertEqual("this is a half typed human draft", text, detail)

    def test_a_draft_is_also_foreign_text_so_the_guard_holds(self):
        self.assertTrue(dunk_core._typed_text(captured("codex-working-human-draft3.ansi")))


if __name__ == "__main__":
    unittest.main()
