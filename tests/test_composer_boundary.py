"""Submitted transcript prompts must not be mistaken for live drafts."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

BORDER = "─" * 64


def screen(composer):
    # The old prompt fits inside the final 14 nonblank rows, as on VP %15.
    return "\n".join((
        "❯ already answered acceptance test", "● I replied to the test.",
        "✻ Worked for 7s", BORDER, composer, BORDER,
        "  bypass permissions on",
    ))


class ComposerBoundaryTests(unittest.TestCase):
    def test_submitted_prompt_above_empty_composer_is_not_a_draft(self):
        self.assertEqual("", dunk_core.box_text(screen("❯ "))[0])
        self.assertEqual("", dunk_core._typed_text(screen("❯ ")))

    def test_actual_draft_wins_over_submitted_transcript(self):
        self.assertEqual("unfinished owner draft",
                         dunk_core.box_text(screen("❯ unfinished owner draft"))[0])

    def test_multiline_draft_survives_trailing_empty_prompt(self):
        draft = "│ ❯ make test\n│ ❯ -k delivery\n❯ "
        text, detail = dunk_core.box_text(screen(draft))
        self.assertEqual("make test", text)
        self.assertIn("prompt line(s) below it", detail)

    def test_ansi_borders_delimit_the_composer(self):
        styled = screen("❯ ").replace(BORDER, "\x1b[2m" + BORDER + "\x1b[0m")
        self.assertEqual("", dunk_core.box_text(styled)[0])

    def test_no_border_keeps_ambiguous_draft(self):
        self.assertEqual("preserve me",
                         dunk_core.box_text("│ ❯ preserve me\n❯ \n")[0])

    def test_a_bottom_border_alone_does_not_erase_draft(self):
        self.assertEqual("preserve me",
                         dunk_core.box_text("│ ❯ preserve me\n❯ \n" + BORDER)[0])

    def test_prompt_shaped_footer_is_outside_composer(self):
        self.assertEqual("", dunk_core.box_text(screen("❯ ") + "\n› footer label")[0])

    def test_long_bordered_draft_keeps_its_first_line(self):
        draft = "❯ first line\n" + "\n".join("│ ❯ continuation" for _ in range(16)) + "\n❯ "
        self.assertEqual("first line", dunk_core.box_text(screen(draft))[0])


if __name__ == "__main__":
    unittest.main()
