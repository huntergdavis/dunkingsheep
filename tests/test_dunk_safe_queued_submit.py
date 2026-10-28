"""t130 (Owner2277): the safe queued-message submit on an ambiguous frame.

The VP wedge, measured 2026-10-28 13:56: our FEDERATE prompt sat visibly in
%15's composer (prompt row 15, footer row 23, two blank rows) and delivery
refused forever with "ambiguous composer/transcript ... no keys are authorised".
The geometry rule cannot tell pending input from transcript - but CONTENT can
when the pending text is one we typed ourselves: _ensure_box_clear refused on
the ambiguous branch BEFORE it ever reached the stranded-ours logic.

The repair: in the ambiguous branch, the live composer row (the LAST prompt
row) is matched against what we typed or are trying to deliver (_looks_like_ours,
the same control the non-ambiguous path already uses). Ours -> submit it.
Anything else -> the refusal stands.

The negative control here is a same-geometry frame whose composer holds a
human-style draft we never sent: it must still be refused, with no keys sent.
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
    """The measured wedge geometry: transcript, one sigil row holding the
    composer text, optional continuation lines below it, then TWO blank rows
    before the status footer (prompt row separated from footer by 2 blanks ->
    ambiguous by the footer-adjacency rule)."""
    lines = [
        "  \u2514 earlier dispatch output",
        "  \u203a an earlier submitted prompt line",
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
        row, a HUMAN continuation line below it. The whole region is no longer a
        prefix of our text -> refused, no Enter."""
        self._prime_ours(FEDERATE_TEXT)
        self.herdr.set_screen("w2:p1", _ambiguous_frame(
            FEDERATE_TEXT,
            continuation_lines=("  HUMAN EDIT: do not run this yet",)))
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
        """A human edited the middle of our line: the region is no longer a
        prefix of anything we sent -> refused."""
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

    def test_ambiguous_frame_with_a_foreign_draft_is_still_refused(self):
        """Negative control (Owner2277): same geometry, but the composer holds a
        human-style draft we never sent. The refusal must stand and no key may
        be pressed - arbitrary draft text is not made safe by the repair."""
        draft = "delete numb and refactor the whole module by hand"
        self._prime_ours(FEDERATE_TEXT)                 # we sent something ELSE
        self.herdr.set_screen("w2:p1", _ambiguous_frame(draft))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"],
                         "a key was pressed into an unverified human draft")

    def test_queued_outbox_text_also_counts_as_ours(self):
        """The daemon may have restarted since the text was typed (memory wiped);
        the queued outbox row for this pane is the second source of ownership."""
        self.herdr.set_screen("w2:p1", _ambiguous_frame(FEDERATE_TEXT))
        # Prime the OUTBOX, not sent_text_by_pane: a queued row we are trying to
        # deliver right now. (No daemon restart in the fixture; the outbox path
        # is what a restart-and-retry sequence exercises.)
        import dunk_core as dc
        probe = dc.Message("mx", "w2:p1", "claude", FEDERATE_TEXT, "send_text",
                           lambda: None)
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
        """A human started typing something that does NOT begin with the first
        80 chars of our send: not ours, refused, no keys."""
        self._prime_ours(FEDERATE_TEXT)
        partial = "FEDERATE tomorrow instead; today I want to look at the logs"
        self.herdr.set_screen("w2:p1", _ambiguous_frame(partial))
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok)
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))
        self.assertEqual([], [keys for pane, keys in self.herdr.keys
                              if pane == "w2:p1"])


if __name__ == "__main__":
    unittest.main()
