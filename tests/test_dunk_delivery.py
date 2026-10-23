"""Can this lane receive a message, and if not why.

The VP's defect: a nudge that never submitted leaves a draft in the input box,
every later send is held as "someone is typing", and the lane is alive, has
unread mail, and can never receive anything - indistinguishable from busy. So
"blocked" is a state of its own, with a reason and a way out.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401  (isolate live state)

import dunk_core  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

LIVE = """\
│ ● wrote the phrase "server connection could not be restored" into a test file
│ ── Working ──────────────────────────────────────────────
│ ~/workspace/dunkingsheep (main)
"""

DEAD = """\
│ ■ Server connection could not be restored
│ • Reconnect failed - check the endpoint, then relaunch (41h 22m)
│ › [inbox] new message
│   ^c quit
"""

PAUSED = """\
│ ✻ Reading files
│ GPT-6-Astra high · ~/workspace/crackdown-switch · Port Crackdown   Goal paused (/goal resume)
"""

# A REAL full-context banner. The hint the harness shows beside an empty box
# ("new task? /clear to save ...") is NOT this, and reading it as this was the
# bug that made a healthy lane permanently unreachable.
CONTEXT_FULL = """\
│ ✻ Working
│ Context limit reached. Run /clear to continue.
"""


class ClassifyDeliveryTests(unittest.TestCase):
    def test_a_live_pane_that_quotes_the_marker_is_not_dead(self):
        # The false positive that made my own pane look dead: the phrase was in
        # its own output. A banner starts its line; prose does not.
        self.assertEqual(("ready", None), dunk_core.classify_delivery(LIVE, "idle"))

    def test_a_banner_line_is_a_dead_session(self):
        state, reason = dunk_core.classify_delivery(DEAD, "idle")
        self.assertEqual("dead", state)
        self.assertIn("server connection could not be restored", reason)

    def test_escape_sequences_do_not_hide_a_banner(self):
        state, _ = dunk_core.classify_delivery("\x1b[31m■ Server connection could not be restored\x1b[0m\n", "idle")
        self.assertEqual("dead", state, "a colour code before the glyph hid the banner")

    def test_a_paused_goal_blocks_delivery_and_says_so(self):
        state, reason = dunk_core.classify_delivery(PAUSED, "idle")
        self.assertEqual("blocked", state)
        self.assertIn("paused goal", reason)

    def test_a_full_context_blocks_delivery_and_names_the_fix(self):
        state, reason = dunk_core.classify_delivery(CONTEXT_FULL, "idle")
        self.assertEqual("blocked", state)
        self.assertIn("/clear", reason)

    def test_a_draft_in_the_box_blocks_delivery(self):
        state, reason = dunk_core.classify_delivery("│ › half a sentence", "idle",
                                                    typed="half a sentence")
        self.assertEqual("blocked", state)
        self.assertIn("unsubmitted draft", reason)

    def test_working_is_not_ready_and_not_blocked(self):
        self.assertEqual(("working", "working"),
                         dunk_core.classify_delivery("│ ✻ Working", "working"))


# The harness's empty-box hint, in the two ways it is drawn: Claude renders it in
# grey RGB (not SGR 2), so "is it dim?" alone cannot tell it from a banner.
HINT_GREY_RGB = ("\x1b[38;2;153;153;153mnew task? \x1b[0m"
                 "\x1b[38;2;177;185;249m/clear\x1b[0m"
                 "\x1b[38;2;153;153;153m to save 497.4k tokens\x1b[0m")
HINT_DIM = "\x1b[2mnew task? /clear to save 497.4k tokens\x1b[0m"


class PlaceholderTests(unittest.TestCase):
    def test_the_hint_is_not_a_state_however_it_is_drawn(self):
        for label, screen in (("grey rgb", HINT_GREY_RGB), ("dim", HINT_DIM)):
            self.assertEqual(("ready", None),
                             dunk_core.classify_delivery(f"❯\u00a0\n{screen}\n", "idle"), label)

    def test_the_hint_does_not_mask_a_real_draft(self):
        # A lane with an actual unsent draft is still blocked; the hint sitting
        # beside it must not launder that into 'ready'.
        screen = f"❯ half a sentence\n{HINT_GREY_RGB}\n"
        state, reason = dunk_core.classify_delivery(screen, "idle", typed="half a sentence")
        self.assertEqual("blocked", state)
        self.assertIn("unsubmitted draft", reason)

    def test_a_real_full_context_banner_still_blocks(self):
        state, reason = dunk_core.classify_delivery("│ Context limit reached. Run /clear to continue\n", "idle")
        self.assertEqual("blocked", state)
        self.assertIn("/clear", reason)

    def test_the_hint_grammar(self):
        self.assertTrue(dunk_core._is_placeholder("new task? /clear to save 497.4k tokens"))
        self.assertTrue(dunk_core._is_placeholder("  new task? /clear to save 1.2M tokens  "))
        self.assertFalse(dunk_core._is_placeholder("Context limit reached"))
        self.assertTrue(dunk_core._is_placeholder("type your request"))


def bordered_draft_above_an_empty_prompt(draft="check your inbox"):
    """The literal screen that broke flush, kept as a fixture.

    Row 1 is a BORDERED draft ('│ ❯ check your inbox'): the first character of the
    row is the box border, so a reader testing the raw first character for the sigil
    says "not a prompt". Row 3 is the live prompt and it is empty. Reading only the
    last prompt line therefore reported "the input box is already clear" over a real
    draft - and it failed in the SAFE direction, which is why it survived so long.
    """
    return (f"│ ❯ {draft}\n"
            "│\n"
            "❯\u00a0\n"
            "─────────────────────────────────────────────\n"
            "  ⏵⏵ bypass permissions on · 1 shell\n")


def multi_line_draft():
    """A pasted two-line draft: the harness draws a sigil on BOTH lines.

    Reading the LAST text-bearing prompt line reported '-la' as if it were the whole
    draft. The draft starts at the first one, and the rest are continuations.
    """
    return ("│ ❯ make test\n"
            "│ ❯ -k delivery\n"
            "❯\u00a0\n"
            "─────────────────────────────────────────────\n")


def draft_that_looks_like_a_prompt():
    """The draft's own text begins with the sigil: '❯ ls' typed INTO the box.

    The screen shows the prompt sigil twice, and the second one is the message.
    """
    return ("❯ ❯ ls\n"
            "❯\u00a0\n"
            "─────────────────────────────────────────────\n")


class BoxReaderTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.tmp.cleanup()

    """One reader for 'what is in the box', and it says what it read.

    flush reported 'the input box is already clear' over a visible draft because
    the draft sat on a BORDERED row ('│ ❯ check your inbox') above an empty prompt,
    and only the last sigil line was consulted. Same disease as send believing
    claude's claim: a component answering a question about the pane without reading
    it properly.
    """

    def test_a_draft_above_an_empty_prompt_is_still_found(self):
        text, detail = dunk_core.box_text(bordered_draft_above_an_empty_prompt())
        self.assertEqual("check your inbox", text)
        self.assertIn("text starts on row 1", detail)

    def test_the_same_fixture_proves_every_asker(self):
        """One screen, three components: the reader, flush, and the send gate.

        send believed a claim, delivery and the queue asked different questions, and
        flush misread this row - all three on the same day. They now ask one reader,
        so one fixture can prove all three.
        """
        screen = bordered_draft_above_an_empty_prompt()
        text, detail = dunk_core.box_text(screen)
        self.assertEqual("check your inbox", text)
        self.assertIn("row 1", detail)

        self.flock.herdr.screens["w1:p2"] = screen
        flushed = self.flock.flush_box("w1:p2", self_pane_id="w1:p1")
        self.assertEqual("check your inbox", flushed["text"])

        state = self.flock.delivery_state("w1:p2")
        self.assertEqual("blocked", state["state"])
        self.assertIn("draft", state["reason"])
        blocker = self.flock.send_blocker("w1:p2")
        self.assertEqual(blocker[1], state["reason"])

    def test_a_multi_line_draft_starts_at_its_first_line(self):
        text, detail = dunk_core.box_text(multi_line_draft())
        self.assertEqual("make test", text, "the draft starts at the first prompt line")
        self.assertIn("prompt line(s) below it", detail)

    def test_a_draft_whose_text_begins_with_the_sigil_still_reads(self):
        text, detail = dunk_core.box_text(draft_that_looks_like_a_prompt())
        self.assertEqual("❯ ls", text,
                         "the inner sigil is part of the message, not a second box")
        self.assertIn("text starts on row 1", detail)

    def test_all_three_fixtures_agree_across_the_askers(self):
        """One reader means one wrong answer everywhere, so the fixture set is the guard."""
        for fixture, expected in ((bordered_draft_above_an_empty_prompt(), "check your inbox"),
                                  (multi_line_draft(), "make test"),
                                  (draft_that_looks_like_a_prompt(), "❯ ls")):
            text, _detail = dunk_core.box_text(fixture)
            self.assertEqual(expected, text)

            self.herdr.screens["w1:p2"] = fixture
            flushed = self.flock.flush_box("w1:p2", self_pane_id="w1:p1")
            self.assertEqual(expected, flushed["text"], "flush submits what the reader read")

            state = self.flock.delivery_state("w1:p2")
            self.assertEqual("blocked", state["state"], "a draft blocks delivery")
            self.assertEqual(self.flock.send_blocker("w1:p2")[1], state["reason"],
                             "the gate and delivery must use the same words")

    def test_a_bordered_draft_is_found(self):
        text, _detail = dunk_core.box_text("│ ❯ check your inbox\n❯\u00a0\n")
        self.assertEqual("check your inbox", text)

    def test_an_empty_box_says_what_it_read(self):
        text, detail = dunk_core.box_text("│ doing work\n❯\u00a0\n──────────\n")
        self.assertEqual("", text)
        self.assertIn("nothing after the sigil", detail)
        self.assertIn("row 2 of 3", detail)

    def test_no_box_says_so_instead_of_claiming_empty(self):
        text, detail = dunk_core.box_text("$ ls\nfoo\n")
        self.assertEqual("", text)
        self.assertIn("no input box found", detail)

    def test_a_dim_hint_is_not_a_draft(self):
        text, _detail = dunk_core.box_text("❯\u00a0\n\x1b[2mnew task? /clear to save 1k tokens\x1b[0m\n")
        self.assertEqual("", text)


class ClearBoxTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.superusers = dunk_core.Superusers(path=os.path.join(self.tmp.name, "su.json"))
        self.superusers.add("w1:p1")
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                     superusers=self.superusers)

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_non_superuser_cannot_clear_someone_elses_box(self):
        with self.assertRaises(dunk_core.DunkError):
            self.flock.clear_box("w1:p2", self_pane_id="w1:p2", reason="test")
        self.assertEqual([], self.herdr.keys, "a refused clear must press nothing")

    def test_a_superuser_clear_sends_ctrl_u_and_escape_and_reports_both_states(self):
        # The VP's stuck pane: dead session, draft left in the box, nothing can
        # be delivered. Clearing is what unblocks it.
        self.herdr.screens["w1:p2"] = DEAD
        self.herdr.statuses["w1:p2"] = "idle"
        result = self.flock.clear_box("w1:p2", self_pane_id="w1:p1", reason="test")
        self.assertTrue(result["flushed"])
        self.assertEqual([("w1:p2", ["C-u"]), ("w1:p2", ["Escape"])], self.herdr.keys)
        self.assertIn("before", result)
        self.assertIn("after", result)
        self.assertEqual("test", result["reason"])

    def test_the_clear_outcome_prints_without_a_label(self):
        # The live crash: two lanes had no 'target_label', the CLI printer
        # indexed it, and the one command that unblocks a lane could not run.
        from importlib.machinery import SourceFileLoader
        cli = SourceFileLoader(
            "dunkcli_print", str(Path(__file__).resolve().parent.parent / "dunkingsheep")).load_module()
        result = {"pane_id": "wW:p1", "flushed": True, "reason": "context full",
                  "before": {"state": "blocked", "reason": "context full"},
                  "after": {"state": "ready", "reason": None}}
        import contextlib, io
        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            cli.print_clear(result)
        self.assertIn("cleared wW:p1", buffer.getvalue())
        self.assertIn("blocked -> ready", buffer.getvalue())
        self.assertIn("context full", buffer.getvalue())

    def test_a_pane_record_without_a_label_falls_back_to_the_pane_id(self):
        self.herdr.screens["w1:p9"] = DEAD
        self.herdr.statuses["w1:p9"] = "idle"
        result = self.flock.clear_box({"pane_id": "w1:p9"}, self_pane_id="w1:p1", reason="probe")
        self.assertEqual("w1:p9", result["target_label"])

    def test_clear_box_works_on_the_case_it_exists_for_a_full_context(self):
        """The mechanism exists for a lane blocked on context, not for a dead one.

        Testing only the dead-pane path would be testing a convenient neighbour:
        the reason we built clear-box was par-a/par-d, both blocked on a full
        context with a draft in the box.
        """
        self.herdr.screens["w1:p2"] = CONTEXT_FULL
        self.herdr.statuses["w1:p2"] = "idle"
        before = self.flock.delivery_state("w1:p2")
        self.assertEqual("blocked", before["state"])
        self.assertIn("context full", before["reason"])
        result = self.flock.clear_box("w1:p2", self_pane_id="w1:p1", reason="context full")
        self.assertTrue(result["flushed"], result)
        self.assertEqual([("w1:p2", ["C-u"]), ("w1:p2", ["Escape"])], self.herdr.keys)
        self.assertEqual("blocked", result["before"]["state"])

    def test_flush_reports_the_evidence_when_the_box_reads_clear(self):
        self.herdr.screens["w1:p2"] = "│ ❯ check your inbox\n❯\u00a0\n"
        # A draft IS there, so flush submits it rather than reporting 'already clear'.
        result = self.flock.flush_box("w1:p2", self_pane_id="w1:p1")
        self.assertEqual("check your inbox", result.get("text"),
                         "flush must submit what the pane shows, not report it clear")
        self.assertTrue(any("Enter" in str(keys) for _pane, keys in self.herdr.keys),
                        "submitting means pressing Enter, not just reading")

    def test_flush_says_what_it_read_when_there_is_nothing_to_submit(self):
        self.herdr.screens["w1:p2"] = "│ doing work\n❯\u00a0\n──────────\n"
        result = self.flock.flush_box("w1:p2", self_pane_id="w1:p1")
        self.assertFalse(result["flushed"])
        self.assertIn("nothing to submit", result["message"])
        self.assertIn("nothing after the sigil", result["message"])

    def test_a_working_lane_is_not_interrupted_by_a_clear(self):
        self.herdr.screens["w1:p2"] = "│ ✻ Working"
        self.herdr.statuses["w1:p2"] = "working"
        # `statuses` is what the pane reports; a clear must never interrupt work.
        result = self.flock.clear_box("w1:p2", self_pane_id="w1:p1", reason="test")
        self.assertFalse(result["flushed"])
        self.assertIn("working", result["reason"])
        self.assertEqual([], self.herdr.keys)


if __name__ == "__main__":
    unittest.main()
