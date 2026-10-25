"""The IDLE-WITH-FOOTER frame: the echo is the last prompt row, the hint is below it.

Live capture from pane %17 at 06:52 (main 94b822c, daemon 2.70.22), between turns:
the composer holds nothing, the last prompt-marked row is the echo of a message
already submitted, the status footer sits under it, and the harness hint
'tab to queue message' is drawn two rows below. The 2.70.21 rule looked for a hint
below the CANDIDATE and gave up when any prompt row followed it - which is exactly
this frame - so the echo came back as a draft (cos's live measurement).

The rule added here is structural: a hint BELOW the last prompt row, and not the row
directly under it, means that prompt is transcript. No new wording is matched, which
is the point - three live frames have now each cost a probe when the fix keyed on
exact strings.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

FIXTURE = os.path.join(os.path.dirname(__file__), "fixtures",
                       "codex-idle-footer-pane17.ansi")


class IdleFooterTests(unittest.TestCase):
    def test_the_live_idle_frame_reads_as_an_empty_box(self):
        with open(FIXTURE, encoding="utf-8") as handle:
            screen = handle.read()
        text, detail = dunk_core.box_text(screen)
        self.assertEqual("", text, f"the submitted echo was read as a draft: {detail}")

    def test_the_live_idle_frame_is_not_foreign_text(self):
        with open(FIXTURE, encoding="utf-8") as handle:
            self.assertEqual("", dunk_core._typed_text(handle.read()))

    def test_a_hint_directly_under_a_draft_still_belongs_to_that_draft(self):
        """The one row of separation that matters: a hint one row below the line
        being typed belongs to it (test_the_hint_does_not_mask_a_real_draft), a hint
        two or more rows below with a footer between does not."""
        screen = "\n".join([
            "\u203a half a sentence",
            "  \x1b[2mtab to queue message\x1b[0m",
        ])
        text, detail = dunk_core.box_text(screen)
        self.assertEqual("half a sentence", text, detail)


if __name__ == "__main__":
    unittest.main()
