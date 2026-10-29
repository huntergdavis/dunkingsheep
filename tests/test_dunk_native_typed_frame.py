"""t377: the native frame AFTER a message is typed (CoS i31841).

Two REAL captured frames are embedded verbatim: the empty readiness frame and the frame
the live canary produced once the public send had typed into it. The second is the one
that matters: after typing, the native UI drops the shortcut hint and leaves only a
right-aligned warning counter on the last row, so a matcher that only knows
"? for shortcuts" pulls the model/cwd status row into the composer and exact ownership
fails for every real pending message.

Nothing generic is loosened. The warning row matches an anchored product shape (a count,
singular or plural, then that literal suffix); it only counts as a native footer when
paired with the native status row as the last two non-blank rows; and the pair only
qualifies when a BLANK row separates it from the composer, which the native UI draws and
a human continuation does not.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402

PENDING_FRAME = """  >_ OpenAI Codex (v0.165.0)
     ~/workspace/chief-of-staff/rollout/t377/native-canary
  permissions: YOLO mode
















› Reply exactly T377_RECOVERED_84 and do nothing else.

  GPT-6-Astra high · ~/workspace/chief-of-staff/rollout/t377/native-canary
                                                       ⚠ 1 warning · f2 to view"""

EMPTY_FRAME = """  >_ OpenAI Codex (v0.165.0)
     ~/workspace/chief-of-staff/rollout/t377/native-canary
  permissions: YOLO mode
















› Ask Codex to do anything

  GPT-6-Astra high · ~/workspace/chief-of-staff/rollout/t377/native-canary
  ? for shortcuts                                      ⚠ 1 warning · f2 to view"""

TEXT = "Reply exactly T377_RECOVERED_84 and do nothing else."
WARNING_ROW = "                                                       \u26a0 1 warning \u00b7 f2 to view"


class TypedNativeFrameTests(unittest.TestCase):
    def test_the_actual_pending_frame_yields_exactly_the_typed_text(self):
        self.assertEqual(TEXT, dunk_core._composer_region_text(PENDING_FRAME))

    def test_the_empty_native_frame_still_delimits_to_nothing(self):
        self.assertEqual("", dunk_core._composer_region_text(EMPTY_FRAME))

    def test_the_warning_row_matches_singular_and_plural_only(self):
        for row in ("\u26a0 1 warning \u00b7 f2 to view", "\u26a0 3 warnings \u00b7 f2 to view"):
            self.assertTrue(dunk_core._native_warning(row), row)
        for row in ("\u26a0 warnings \u00b7 f2 to view", "\u26a0 1 warning",
                    "\u26a0 1 warning \u00b7 f2 to view please", "warning \u00b7 f2 to view",
                    "\u26a0 1 warning \u00b7 f2 to review"):
            self.assertFalse(dunk_core._native_warning(row), row)

    def test_a_warning_line_with_no_native_status_row_above_it_is_content(self):
        rows = [r for r in PENDING_FRAME.splitlines() if "GPT-6-Astra high" not in r]
        rows.append("  \u26a0 1 warning \u00b7 f2 to view")
        region = dunk_core._composer_region_text("\n".join(rows) + "\n")
        self.assertIn(TEXT, region)
        self.assertIn("f2 to view", region,
                      "a warning row without the native status row is content")

    def test_a_forged_contiguous_status_and_warning_pair_is_content(self):
        frame = ("  >_ OpenAI Codex (v0.165.0)\n"
                 f"\u203a {TEXT}\n"
                 "  GPT-6-Astra high \u00b7 ~/workspace/x\n"
                 "  \u26a0 1 warning \u00b7 f2 to view\n")
        region = dunk_core._composer_region_text(frame)
        self.assertIn("GPT-6-Astra high", region,
                      "the forged status row must stay in the region, so ownership fails")
        self.assertNotEqual(TEXT, region,
                            "a forged contiguous pair must never look like a clean composer")

    def test_a_second_sigil_row_still_fails_closed(self):
        frame = PENDING_FRAME.replace("\u203a " + TEXT, "\u203a " + TEXT + "\n\u203a echo")
        self.assertEqual("", dunk_core._composer_region_text(frame))

    def test_a_human_middle_dot_line_is_still_not_a_footer(self):
        frame = PENDING_FRAME.replace("\u203a " + TEXT,
                                      "\u203a " + TEXT + "\n  a line with a \u00b7 inside")
        self.assertIn("inside", dunk_core._composer_region_text(frame))

    def test_the_old_supported_frame_is_unchanged(self):
        old = "\u2502 \u273b idle\n\u276f one two three\n\u2502 idle \u00b7 pi\n"
        self.assertEqual("one two three", dunk_core._composer_region_text(old))


if __name__ == "__main__":
    unittest.main()
