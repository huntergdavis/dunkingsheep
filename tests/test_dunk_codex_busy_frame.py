"""A BUSY codex lane: the placeholder is on the LAST prompt row, not below it.

Captured from pane %17 at 06:35 while the lane was Working and an inbox nudge had
just been echoed into the transcript. The earlier #17 discriminator looked for a
hint BELOW the candidate with no prompt after it; in this frame the composer is the
LAST prompt row and it holds the placeholder 'Ask Codex to do anything', with the
status line ('<- for agents . ? for shortcuts') underneath - so the rule missed it
and a plain send to %17 still queued as "someone is typing" (m1977).

The rule that covers both frames: when the LAST prompt row's text is a known harness
placeholder, the box is EMPTY and every earlier prompt-marked line is transcript. A
BLANK last prompt row is not a placeholder, so claude's draft-above-an-empty-prompt
fixture keeps its meaning.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

FIXTURE = os.path.join(os.path.dirname(__file__), "fixtures",
                       "codex-busy-frame-pane17.ansi")


def captured():
    with open(FIXTURE, encoding="utf-8") as handle:
        return handle.read()


class BusyCodexFrameTests(unittest.TestCase):
    def test_the_busy_capture_reads_as_an_empty_box(self):
        text, detail = dunk_core.box_text(captured())
        self.assertEqual("", text, f"a transcript echo was read as the draft: {detail}")

    def test_the_busy_capture_is_not_foreign_text(self):
        self.assertEqual("", dunk_core._typed_text(captured()))

    def test_a_blank_last_prompt_row_is_not_a_placeholder(self):
        """The distinction the fixture set depends on: blank != placeholder."""
        screen = "\n".join([
            "\u276f an earlier instruction",
            "  some answer",
            "\u276f check your inbox",
            "\u276f ",
            "\u2500" * 20,
        ])
        text, detail = dunk_core.box_text(screen)
        # Which line wins is the old first-prompt-row rule and not this change's
        # business; the claim here is only that a BLANK last row is not mistaken for
        # a placeholder, i.e. the box is NOT reported empty.
        self.assertNotEqual("", text, f"a blank last prompt row emptied the box: {detail}")

if __name__ == "__main__":
    unittest.main()
