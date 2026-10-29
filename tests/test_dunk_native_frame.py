"""t377: the REAL native Codex frame must be delimitable (CoS i31833).

Captured live from the native canary pane by CoS and embedded verbatim below, so the
fixture cannot drift from the program it describes. Two product shapes broke the
conservative matcher and are handled here, each by its EXACT shape:

  * the native banner ("  >_ OpenAI Codex (v0.165.0)") is a product header, not a prompt
    - it was being counted as a second sigil, which refused every real native frame;
  * the native footer is TWO rows (model/cwd status, then the shortcuts/warning hint),
    where the old rule assumed one.

Everything else is untouched: a second sigil row that is not the banner is still
ambiguous (fail closed), and a human line holding a middle dot is still composer content.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402

# THE CAPTURED FRAME (rows kept verbatim: banner, cwd, permissions, composer, both
# footer rows). Do not hand-edit: it is evidence, not a mock.
NATIVE_FRAME = """  >_ OpenAI Codex (v0.165.0)
     ~/workspace/chief-of-staff/rollout/t377/native-canary
  permissions: YOLO mode

› Ask Codex to do anything

  GPT-6-Astra high · ~/workspace/chief-of-staff/rollout/t377/native-canary
  ? for shortcuts                                      ⚠ 1 warning · f2 to view
"""

TEXT = "T377-CANARY one known-owned line"


def with_composer(composer_rows):
    rows = NATIVE_FRAME.splitlines()
    rows[4:6] = [composer_rows]          # replace the placeholder composer row
    return "\n".join(rows) + "\n"


class NativeFrameTests(unittest.TestCase):
    def test_the_actual_empty_native_frame_is_delimited_with_no_content(self):
        # Empty composer: the region is "" (nothing owned), NOT a refusal that hides the
        # frame, and NOT the status rows leaked in as content.
        self.assertEqual("", dunk_core._composer_region_text(NATIVE_FRAME))

    def test_exact_owned_text_is_the_region(self):
        screen = with_composer("\u203a " + TEXT)
        self.assertEqual(TEXT, dunk_core._composer_region_text(screen))

    def test_wrapped_known_text_is_tolerated_as_wrapping_not_continuation(self):
        screen = with_composer("\u203a T377-CANARY one\n  known-owned line")
        self.assertEqual("T377-CANARY one known-owned line",
                         dunk_core._composer_region_text(screen))

    def test_a_human_added_line_stays_in_the_region(self):
        screen = with_composer("\u203a " + TEXT + "\n  and one more thing I typed")
        region = dunk_core._composer_region_text(screen)
        self.assertIn("and one more thing", region,
                      "a human continuation is content, never a footer")

    def test_a_human_line_holding_a_middle_dot_is_not_a_footer(self):
        screen = with_composer("\u203a " + TEXT + "\n  a line with a \u00b7 inside")
        region = dunk_core._composer_region_text(screen)
        self.assertIn("inside", region, "the middle-dot rule was not loosened")
        self.assertIn(TEXT, region)

    def test_a_second_sigil_row_still_fails_closed(self):
        screen = with_composer("\u203a " + TEXT + "\n\u203a a transcript echo")
        self.assertEqual("", dunk_core._composer_region_text(screen))

    def test_a_status_row_without_the_hint_row_is_not_a_native_footer(self):
        rows = [r for r in NATIVE_FRAME.splitlines() if not r.strip().startswith("? for shortcuts")]
        screen = "\n".join(rows) + "\n"
        # one status row only: the single-footer rule applies, and with the placeholder
        # body empty the region is "" - but the status text never leaks into it
        self.assertEqual("", dunk_core._composer_region_text(screen))

    def test_a_forged_status_row_from_a_human_is_not_dropped(self):
        # A human line that looks like the status row, but there is no hint row after it:
        # it is composer content, so ownership equality fails.
        screen = with_composer("\u203a " + TEXT + "\n  GPT-6-Astra high \u00b7 ~/x")
        region = dunk_core._composer_region_text(screen)
        self.assertIn("GPT-6-Astra high", region)

    def test_the_old_supported_frame_is_unchanged(self):
        old = "\u2502 \u273b idle\n\u276f one two three\n\u2502 idle \u00b7 pi\n"
        self.assertEqual("one two three", dunk_core._composer_region_text(old))

    def test_a_banner_lookalike_row_is_not_excluded(self):
        # Only the EXACT product banner is exempt from the sigil count.
        rows = NATIVE_FRAME.splitlines()
        rows[0] = "  >_ some other tool"
        screen = "\n".join(rows) + "\n"
        self.assertEqual("", dunk_core._composer_region_text(screen))


if __name__ == "__main__":
    unittest.main()
