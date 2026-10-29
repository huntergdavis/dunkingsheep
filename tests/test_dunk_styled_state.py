"""t130/t394: the SGR effective-state table for the style-state classifier.

The table is %19's i31913 specification, encoded as EXECUTED unit cases
against the real captured %33 frame kept as the positive (not a synthetic
construction). Semantics: SGR parameters apply left to right within one
escape; 22 cancels BOTH bold and dim; the state that matters is the one in
effect AT THE SIGIL CHARACTER (a row that opens dim and brightens after the
sigil is dim-at-sigil = transcript; a row that opens bright and dims after
the sigil is bold-at-sigil = candidate).
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402

# The effective-state table (SGR prefix -> expected classification):
#   classification: "bold" = composer candidate, "dim" = transcript echo,
#   "unknown" = fail closed.
SGR_TABLE = [
    ("\x1b[1m", "bold"),        # bold -> composer candidate
    ("\x1b[1;2m", "dim"),       # bold + dim -> transcript echo (excluded)
    ("\x1b[2;1m", "dim"),       # dim then bold -> bold AND dim -> MEASURED echo
    ("\x1b[2m", "unknown"),     # dim ONLY: a styling fact is not transcript
                                # provenance (%19 i31918) -> REFUSE
    ("\x1b[0m", "unknown"),     # reset -> normal -> REFUSE
    ("\x1b[22m", "unknown"),    # normal intensity -> REFUSE
    ("\x1b[1;0m", "unknown"),   # bold then reset -> normal -> REFUSE
    ("\x1b[1;22m", "unknown"),  # bold then normal-intensity -> normal -> REFUSE
    ("\x1b[2;22m", "unknown"),  # dim then normal-intensity -> normal -> REFUSE
    ("\x1b[3m", "unknown"),     # italics -> neither -> REFUSE
    ("\x1b[32m", "unknown"),    # colour only -> neither -> REFUSE
    ("", "unknown"),            # no SGR at all -> unknown -> REFUSE
]


def _sigil_row(prefix):
    return f"{prefix}\u203a the composer text"


class EffectiveStateTableTests(unittest.TestCase):
    def test_table(self):
        for prefix, expected in SGR_TABLE:
            with self.subTest(sgr=prefix or "(none)"):
                got = dunk_core._styled_sigil_state(_sigil_row(prefix))
                self.assertEqual(expected, got,
                                 f"SGR {prefix!r}: expected {expected}, got {got}")

    def test_nested_bright_then_dim_is_dim_at_sigil(self):
        """A row that opens bright and dims after the sigil is bold-AT-SIGIL:
        the dim word later in the row must not throw the composer away."""
        self.assertEqual("bold", dunk_core._styled_sigil_state(
            "\x1b[1m\u203a text \x1b[2mdim word"))

    def test_nested_dim_then_bright_is_refused(self):
        """A row that opens dim and brightens after the sigil is dim-AT-SIGIL:
        dim-only provenance is NOT the transcript signature (%19 i31918) ->
        the row classifies unknown and the frame refuses. A future reader who
        sees 'dim -> excluded' would re-widen this hole; the narrow measured
        signature is bold+dim only."""
        self.assertEqual("unknown", dunk_core._styled_sigil_state(
            "\x1b[2m\u203a \x1b[1mbright body"))

    def test_colon_form_is_documented_safe_refusal(self):
        """The colon SGR form (38:5:242m) is not matched by the semicolon
        regex: the row classifies unknown and the frame refuses. Documented
        safe refusal, not silent support."""
        self.assertEqual("unknown", dunk_core._styled_sigil_state(
            "\x1b[38:5:242m\u203a the composer text"))


if __name__ == "__main__":
    unittest.main()
