"""t130 (Owner2277): the safe queued-message submit on an ambiguous frame.

The VP wedge, measured 2026-10-28 13:56: our FEDERATE prompt sat visibly in
%15's composer (prompt row 15, footer row 23, two blank rows) and delivery
refused forever with "ambiguous composer/transcript ... no keys are authorised".
The geometry rule cannot tell pending input from transcript - but CONTENT can
when the pending text is one we typed ourselves.

The repair lives in ONE shared decision path (Flock._composer_ownership) used
by BOTH the delivery guard (_ensure_box_clear) and the public watchdog seam
(Flock.composer_region_is_owned, t377/Owner2282), so the two entry points
cannot drift. Ownership is STRICT full-composer EQUALITY after collapse:
- the whole composer region (last prompt-sigil row + every non-blank
  continuation line below it, footer excluded) must equal our owned text;
- the status footer must be the LAST non-blank row (a human line holding a
  middle dot is content, not a footer - CoS review of 7d9f6b8);
- immediately-adjacent sigil rows refuse (nested/unresolvable - CoS i31512);
- a duplicate matched echo above the composer refuses (same review).

Everything else - human continuation, human edits, truncation ("FEDERATE NOW",
"F"), foreign drafts, empty memory - stays refused with zero keys.
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

# The live FEDERATE prompt from the VP wedge (evidence: `dunkingsheep read %15
# --lines 80` at 2026-10-28 13:56 PDT), as it sat on the composer's sigil row.
FEDERATE_TEXT = (
    "FEDERATE NOW (owner standing order #2061). Run ~/.config/dunkingsheep/bin/"
    "dispatch-pass.sh, then: (1) every open/in_progress task with no working owner"
)


def _ambiguous_frame(composer_text, continuation_lines=()):
    """The wedge geometry: transcript WITHOUT prompt sigils (plain output
    lines), one sigil row holding the composer text, optional continuation
    lines below it, then TWO blank rows before the status footer (prompt row
    separated from footer by 2 blanks -> ambiguous by the footer-adjacency
    rule). With exactly ONE sigil row the composer boundary is unambiguous,
    so ownership-by-content may flush; frames that ALSO carry a sigil'd
    transcript echo have no delitable boundary and fail closed (see the
    echo-limit test)."""
    lines = [
        "  \u2514 earlier dispatch output",
        "  an earlier submitted prompt, echoed as plain transcript output",
        "",
        "\u203a " + composer_text,
    ]
    lines.extend(continuation_lines)
    lines.extend(["", ""])
    lines.append("  gpt-6-astra high \u00b7 ~/workspace/vp-of-agentry \u00b7 Review t132 test receipt")
    return "\n".join(lines) + "\n"


class SafeQueuedSubmitTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.poll = mock.patch.object(dunk_core, "SUBMIT_POLL_S", (0.0,))
        self.poll.start()

    def tearDown(self):
        self.poll.stop()
        self.flock.close()

    def _prime_ours(self, text):
        """The daemon's own memory of what it typed to this pane."""
        self.flock.sent_text_by_pane["w2:p1"] = [text]

    def test_ambiguous_frame_holding_our_queued_text_is_submitted(self):
        """The wedge case: geometry ambiguous, content is OUR queued message ->
        the guard submits it with Enter instead of refusing forever."""
        self._prime_ours(FEDERATE_TEXT)
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        # After the flush Enter, the box no longer holds anything of ours.
        with mock.patch.object(type(self.flock), "_box_holds", return_value=False):
            ok = self.flock._ensure_box_clear("w2:p1")
        self.assertTrue(ok)
        self.assertEqual("stranded", self.flock._last_block_reason.get("w2:p1"))
        pressed = [keys for pane, keys in self.herdr.keys if pane == "w2:p1"]
        self.assertIn(["Enter"], pressed, "the queued message was never submitted")

    def test_human_continuation_below_our_body_is_refused(self):
        """CoS's blocking regression (red-first): our queued body on the sigil
        row, a HUMAN continuation line below it. The whole region is no longer
        equal to our text -> refused, no Enter."""
        self._prime_ours(FEDERATE_TEXT)
        self.herdr.set_screen("w2:p1", _ambiguous_frame(
            FEDERATE_TEXT,
            continuation_lines=("  HUMAN EDIT: do not run this",)))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"],
                         "a key was pressed into a frame with a human continuation")

    def test_owned_multi_line_nudge_still_submits(self):
        """The control that proves the fix did not trade a false submit for a
        lane that never delivers: our OWN multi-line message, wrapped across
        the sigil row and continuation rows, is fully ours -> submitted."""
        multi = (FEDERATE_TEXT + "\n(3) never leave a task unowned. Report only "
                 "exceptions and one line per assignment.")
        self._prime_ours(multi)
        self.herdr.set_screen("w2:p1", _ambiguous_frame(
            FEDERATE_TEXT,
            continuation_lines=("(3) never leave a task unowned. Report only",
                                "exceptions and one line per assignment.")))
        with mock.patch.object(type(self.flock), "_box_holds", return_value=False):
            ok = self.flock._ensure_box_clear("w2:p1")
        self.assertTrue(ok)
        self.assertEqual("stranded", self.flock._last_block_reason.get("w2:p1"))
        pressed = [keys for pane, keys in self.herdr.keys if pane == "w2:p1"]
        self.assertIn(["Enter"], pressed)

    def test_human_edit_of_our_line_is_refused(self):
        """A human edited the middle of our line: the region is no longer equal
        to anything we sent -> refused."""
        self._prime_ours(FEDERATE_TEXT)
        edited = FEDERATE_TEXT.replace("standing order #2061",
                                       "standing order #2061 BUT ONLY IF I SAY SO")
        self.herdr.set_screen("w2:p1", _ambiguous_frame(edited))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))

    def test_single_character_draft_is_refused(self):
        """CoS i31447: an arbitrary short prefix ("F") must not accept - a human
        deletion or a one-key draft is not our message. Equality only."""
        self._prime_ours(FEDERATE_TEXT)
        self.herdr.set_screen("w2:p1", _ambiguous_frame("F"))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], self.herdr.keys)

    def test_removed_suffix_is_refused(self):
        """CoS i31447/%19: our text with the suffix deleted by a human - the
        visible composer no longer equals the owned text -> refused, no keys.
        Both named shapes: a short prefix ("FEDERATE NOW") and a longer cut."""
        self._prime_ours(FEDERATE_TEXT)
        for truncated in ("FEDERATE NOW", FEDERATE_TEXT[:60]):
            self.herdr.set_screen("w2:p1", _ambiguous_frame(truncated))
            ok = self.flock._ensure_box_clear("w2:p1")
            self.assertFalse(ok, truncated)
            self.assertEqual("ambiguous-composer",
                             self.flock._last_block_reason.get("w2:p1"), truncated)
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"],
                         "a key was pressed into a truncated human draft")

    def test_queued_outbox_text_also_counts_as_ours(self):
        """The daemon may have restarted since the text was typed (memory wiped);
        the queued outbox row for this pane is the second source of ownership."""
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        probe = dunk_core.Message("mx", "w2:p1", "claude", FEDERATE_TEXT,
                                  "send_text", lambda: None)
        probe.status = "queued"
        self.flock.outbox["w2:p1"] = [probe]
        with mock.patch.object(type(self.flock), "_box_holds", return_value=False):
            ok = self.flock._ensure_box_clear("w2:p1")
        self.assertTrue(ok)
        self.assertEqual("stranded", self.flock._last_block_reason.get("w2:p1"))

    def test_frame_without_our_text_and_empty_memory_stays_refused(self):
        """No memory at all (fresh daemon, nothing queued): the refusal stands
        exactly as before the repair."""
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], self.herdr.keys)

    def test_partial_draft_that_merely_overlaps_is_refused(self):
        """A human started typing something that does NOT equal our send: not
        ours, refused, no keys."""
        self._prime_ours(FEDERATE_TEXT)
        partial = "FEDERATE tomorrow instead; today I want to look at the logs"
        self.herdr.set_screen("w2:p1", _ambiguous_frame(partial))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])


class CosReviewNegatives(unittest.TestCase):
    """CoS's independent review of 7d9f6b8: two reproduced parser false accepts."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.poll = mock.patch.object(dunk_core, "SUBMIT_POLL_S", (0.0,))
        self.poll.start()

    def tearDown(self):
        self.poll.stop()
        self.flock.close()

    def test_human_middle_dot_line_is_not_a_footer(self):
        """CoS false accept 1: the old footer detector treated any middle-dot
        line as the footer, so a human line 'HUMAN EDIT · do not run this'
        ended the region and was silently excluded. The footer must be the
        LAST non-blank row; the human line stays inside the region -> refused."""
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        frame = "\n".join([
            "  \u2514 earlier dispatch output",
            "",
            "\u203a " + FEDERATE_TEXT,
            "  HUMAN EDIT \u00b7 do not run this",
            "",
            "",
            "  gpt-6-astra high \u00b7 ~/workspace/vp-of-agentry \u00b7 Review t132",
        ]) + "\n"
        self.herdr.set_screen("w2:p1", frame)
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])

    def test_nested_sigil_duplicate_of_our_text_is_refused(self):
        """CoS false accept 2: a second sigil row holding our known text below
        the real composer made the LAST-sigil extraction yield exactly our text
        while the real composer content was discarded. The duplicate matched
        echo is a refusal signal, not an ownership proof."""
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        frame = "\n".join([
            "  \u2514 earlier dispatch output",
            "",
            "\u203a " + FEDERATE_TEXT,
            "\u203a " + FEDERATE_TEXT,
            "",
            "",
            "  gpt-6-astra high \u00b7 ~/workspace/vp-of-agentry \u00b7 Review t132",
        ]) + "\n"
        self.herdr.set_screen("w2:p1", frame)
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])

    def test_transcript_sigil_not_matching_ours_keeps_positive_alive(self):
        """The duplicate guard must not poison the positive: a transcript sigil
        whose body does NOT match our send leaves the flush working."""
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        with mock.patch.object(type(self.flock), "_box_holds", return_value=False):
            ok = self.flock._ensure_box_clear("w2:p1")
        self.assertTrue(ok)


class CosSecondReviewNegatives(unittest.TestCase):
    """CoS i31512: the public seam bypassed the nested-sigil guard, and the
    duplicate-equality special case still accepted a real human draft when our
    text was INSERTED as a second sigil row below it."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def _wedge_with_inserted_echo(self):
        """The real composer holds 'HUMAN DRAFT'; our known text was inserted
        below it as a second ADJACENT sigil row (no blank between)."""
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        frame = _ambiguous_frame("HUMAN DRAFT",
                                 continuation_lines=("\u203a " + FEDERATE_TEXT,))
        self.herdr.set_screen("w2:p1", frame)
        return frame

    def test_guard_refuses_human_draft_with_inserted_echo(self):
        self._wedge_with_inserted_echo()
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])

    def test_public_seam_refuses_human_draft_with_inserted_echo(self):
        """The seam must refuse the SAME frame - one shared decision path."""
        frame = self._wedge_with_inserted_echo()
        self.assertFalse(self.flock.composer_region_is_owned("w2:p1", frame))

    def test_blank_then_inserted_owned_sigil_is_refused(self):
        """CoS i31543: blank insertion must not make a human-edited draft safe.
        Real composer 'HUMAN DRAFT', blank row, then our text inserted as a
        second BRIGHT sigil row: two live-content candidates, geometry
        indistinguishable -> FAIL CLOSED, no keys."""
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        frame = _ambiguous_frame("HUMAN DRAFT",
                                 continuation_lines=("", "\u203a " + FEDERATE_TEXT,))
        self.herdr.set_screen("w2:p1", frame)
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])
        self.assertFalse(self.flock.composer_region_is_owned("w2:p1", frame))



class PublicSeamTests(unittest.TestCase):
    """Owner2282/t377: the watchdog depends on composer_region_is_owned."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_public_wrapper_is_exact_and_names_stable(self):
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        self.assertTrue(self.flock.composer_region_is_owned(
            "w2:p1", self.herdr.screens["w2:p1"]))
        self.herdr.set_screen("w2:p1", _ambiguous_frame(
            FEDERATE_TEXT, continuation_lines=("  HUMAN EDIT: do not run",)))
        self.assertFalse(self.flock.composer_region_is_owned(
            "w2:p1", self.herdr.screens["w2:p1"]))

    def test_public_wrapper_is_false_without_ownership(self):
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        self.assertFalse(self.flock.composer_region_is_owned(
            "w2:p1", self.herdr.screens["w2:p1"]))


if __name__ == "__main__":
    unittest.main()


class DocumentedLimitTests(unittest.TestCase):
    """CoS i31555: the conservative boundary rule's stated limit, kept as a
    test rather than deleted - a frame that ALSO carries a sigil'd transcript
    echo of a previously submitted prompt is indistinguishable from an edited
    composer, so it FAILS CLOSED. The wedge positive above deliberately uses a
    sigil-free transcript to model the unambiguous boundary."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_sigild_transcript_echo_fails_closed_even_for_our_text(self):
        self.flock.sent_text_by_pane["w2:p1"] = [FEDERATE_TEXT]
        frame = "\n".join([
            "  \u2514 earlier dispatch output",
            "\u203a an earlier submitted prompt (transcript echo)",
            "",
            "\u203a " + FEDERATE_TEXT,
            "",
            "",
            "  gpt-6-astra high \u00b7 ~/workspace/vp-of-agentry \u00b7 Review t132",
        ]) + "\n"
        self.herdr.set_screen("w2:p1", frame)
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertFalse(self.flock.composer_region_is_owned("w2:p1", frame))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])
