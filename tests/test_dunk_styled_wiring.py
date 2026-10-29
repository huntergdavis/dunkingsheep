"""t130: the style-state boundary is WIRED into delivery with dim-body
provenance accounting - the control that closes the forged-human-content gap.

The %33 wedge frame (real captured styled frame) now flows: plain boundary
fails closed (2 sigil rows) -> styled boundary delimits (exactly one bold-
non-dim composer, dim echoes excluded) -> EVERY excluded dim body must match
a message the daemon demonstrably sent (sent history or outbox) -> region
through UNCHANGED strict ownership equality -> flush.

Controls: an unaccounted dim body (human text in a transcript-styled row)
refuses the frame; accounting via outbox rows works like sent-history
accounting; the genuine-shell and named-agent paths are unchanged.
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

# The real captured %33 frame (chief-of-staff/rollout/t393, sha 9877d597...):
# row 0 = bold+DIM sigil (transcript echo of a previously delivered message),
# row 15 = bold composer sigil holding the d71 text.
D71_TEXT = ("Design/web check-in d71: read your inbox and pane-assigned task queue.\n"
            "  Continue the current bounded t230 web-editor interface and remaining assigned\n"
            "  product fixes; honor blocked dependencies and current exact-head reviewer\n"
            "  gates. No global task claim, self-merge, owner-game control or live success\n"
            "  inferred from fixture tests. Report exact artifact/head and next action.")
D71_FIRST_LINE = ("Design/web check-in d71: read your inbox and pane-assigned task queue.")

E = "\x1b["
# Faithful styled geometry: dim transcript echo (bold+DIM sigil `1;2`), bright
# composer sigil (`1`), footer last non-blank row.
FRAME_33 = "\n".join([
    E + "1;2m\u203a \x1b[0m[inbox] new message for codex-design from %19. Read it with: dunkingsheep in\u2026",
    "    0ccc348ed97dd171319285fc7b18d289cfe9acb6; t318 closed.",
    "",
    "  \u2022 PR80 reviewer browser provenance verified; both review tasks closed.",
    "",
    "  \u2022 t62 inventory found /api/terminals returns 404. Next: adapt the existing",
    "    authenticated terminal transport.",
    "",
    "  Not verified: deployment, live pickup, new game loading, terminal stream/",
    "  resize, remote access, or full accessibility. t230 and t66 remain open for",
    "  owner acceptance.",
    "",
    "  Worked for 11m 33s \u00b7 1:19 AM",
    "",
    "",
    E + "1m\u203a\x1b[0m " + D71_TEXT.splitlines()[0],
    "  " + D71_TEXT.splitlines()[1],
    "  " + D71_TEXT.splitlines()[2],
    "  " + D71_TEXT.splitlines()[3],
    "  " + D71_TEXT.splitlines()[4],
    "",
    "",
    "  gpt-6-astra high \u00b7 ~/workspace/vp-of-agentry-design-lane \u00b7 Implement dashboar\u2026",
]) + "\n"


class StyledWiringTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.herdr.set_screen("w2:p1", FRAME_33)
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.0, 0.0, 0.0))
        self.verify = mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.0, 0.0))
        self.poll = mock.patch.object(dunk_core, "SUBMIT_POLL_S", (0.0,))
        for p in (self.gaps, self.verify, self.poll):
            p.start()

    def tearDown(self):
        for p in (self.gaps, self.verify, self.poll):
            p.stop()
        self.flock.close()

    def test_dim_body_accounted_via_sent_history_green(self):
        """The wedge case, closed: the dim echo body matches a message the
        daemon demonstrably sent, so the exclusion is provenance-backed and
        the frame delimits to our owned d71 text."""
        self.flock.sent_text_by_pane["w2:p1"] = [
            "[inbox] new message for codex-design from %19. Read it with: dunkingsheep inbox read",
            D71_TEXT,
        ]
        owned, region = self.flock._composer_ownership("w2:p1", FRAME_33)
        self.assertTrue(owned, f"region={region[:120]!r}")
        self.assertTrue(region.startswith("Design/web check-in d71:"), region[:120])
        # And the guard flushes it:
        self.herdr.set_screen("w2:p1", FRAME_33)
        self.flock.sent_text_by_pane["w2:p1"] = [
            "[inbox] new message for codex-design from %19. Read it with: dunkingsheep inbox read",
            D71_TEXT,
        ]
        with mock.patch.object(type(self.flock), "_box_holds", return_value=False):
            ok = self.flock._ensure_box_clear("w2:p1")
        self.assertTrue(ok, "the provenance-backed frame must flush")
        self.assertEqual("stranded", self.flock._last_block_reason.get("w2:p1"))

    def test_unaccounted_dim_body_refuses(self):
        """The forged-human-content control: a dim sigil body that matches
        NOTHING the daemon sent is human content in a transcript-styled row -
        the frame REFUSES rather than silently excluding the text."""
        self.flock.sent_text_by_pane["w2:p1"] = [D71_TEXT]  # echo body NOT in history
        owned, region = self.flock._composer_ownership("w2:p1", FRAME_33)
        self.assertFalse(owned)
        self.assertEqual("", region)
        ok = self.flock._ensure_box_clear("w2:p1")
        self.assertFalse(ok, "unaccounted dim body must refuse")
        self.assertEqual("ambiguous-composer",
                         self.flock._last_block_reason.get("w2:p1"))

    def test_dim_body_accounted_via_outbox_row(self):
        """The second provenance source: a queued outbox row for the pane
        accounts the dim echo body; the d71 region must ALSO be owned (sent
        history), since strict equality applies to the region regardless."""
        self.flock.sent_text_by_pane["w2:p1"] = [D71_TEXT]
        msg = dunk_core.Message("mx", "w2:p1", "claude",
                                "[inbox] new message for codex-design from %19. "
                                "Read it with: dunkingsheep inbox read",
                                "send_text", lambda: None)
        msg.status = "queued"
        self.flock.outbox["w2:p1"] = [msg]
        owned, region = self.flock._composer_ownership("w2:p1", FRAME_33)
        self.assertTrue(owned, f"outbox-provenanced dim body must account: {region[:80]!r}")
        self.assertTrue(region.startswith("Design/web check-in d71:"), region[:80])

    def test_plain_frames_unaffected(self):
        """The wiring must not change the plain-path behaviour: single-sigil
        and multi-sigil PLAIN (no ANSI) frames keep their existing verdicts."""
        plain_single = "\n".join([
            "  \u2514 output", "", "\u203a our queued text", "", "",
            "  gpt-6-astra high \u00b7 ~/workspace \u00b7 task",
        ]) + "\n"
        self.flock.sent_text_by_pane["w2:p1"] = ["our queued text"]
        self.herdr.set_screen("w2:p1", plain_single)
        owned, region = self.flock._composer_ownership("w2:p1", plain_single)
        self.assertTrue(owned and region == "our queued text")
        # Multi-sigil PLAIN frame: still fail closed (no ANSI -> no styled path).
        plain_multi = "\n".join([
            "\u203a an echo", "", "\u203a our queued text", "", "",
            "  gpt-6-astra high \u00b7 ~/workspace \u00b7 task",
        ]) + "\n"
        self.herdr.set_screen("w2:p1", plain_multi)
        owned, _region = self.flock._composer_ownership("w2:p1", plain_multi)
        self.assertFalse(owned, "plain multi-sigil still fails closed")


if __name__ == "__main__":
    unittest.main()
