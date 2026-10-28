"""Synthetic geometry controls; NOT an original capture of the t339 incident."""
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

import _bootstrap  # noqa: F401
import dunk_core as core
from fakes import FakeHerdr, codex_approval_dialog

NOTICE = "[inbox] new message for fixture from reviewer. Read it with: dunkingsheep i…"
FOOTER = "  model · /tmp/synthetic-fixture · task"


def frame(body, continuation=(), hint=None):
    """Prompt row19, then any continuations, two blank rows, footer, optional hint."""
    rows = ["synthetic transcript"] + [""] * 17
    rows += ["› " + body, *continuation, "", "", FOOTER]
    if hint is not None:
        rows.append(hint)
    return "\n".join(rows) + "\n"


class AmbiguousComposerTests(unittest.TestCase):
    def setUp(self):
        self.terminal = FakeHerdr()
        self.flock = core.Flock(herdr=self.terminal, persist=False, restore=False)
        self.addCleanup(self.flock.close)
        self.addCleanup(patch.stopall)
        for target in ("subprocess.Popen", "os.system", "os.execv", "os.execve", "os.kill"):
            patch(target, side_effect=AssertionError("external operation forbidden")).start()

    def check_ambiguous(self, screen):
        self.terminal.set_screen("w2:p1", screen)
        self.terminal.box["w2:p1"] = "preserve this synthetic pending content"
        before_box = dict(self.terminal.box)
        text, detail = core.box_text(screen)
        # Empty text is not evidence of an empty composer when the detail is unknown.
        self.assertIn("ambiguous composer", detail)
        self.assertNotIn("box is empty", detail)
        self.assertIsNone(self.flock._box_holds("w2:p1", NOTICE))
        state = self.flock.delivery_state("w2:p1")
        self.assertEqual("unknown", state["state"])
        self.assertIs(False, state["deliverable"])
        send = Mock(return_value=(True, "sent"))
        sent, ok, reason = self.flock.send_guarded("w2:p1", send)
        self.assertIs(False, sent, reason)
        self.assertIsNone(ok)
        send.assert_not_called()
        flushed = self.flock.flush_box("w2:p1")
        self.assertIs(False, flushed["flushed"])
        self.assertEqual("unknown", flushed["state"])
        self.assertIn("ambiguous", flushed["message"])
        self.assertEqual([], self.terminal.keys)
        self.assertEqual([], self.terminal.sends)
        self.assertEqual(before_box, self.terminal.box)

    def test_ellipsised_notice_is_ambiguous_not_empty(self):
        self.check_ambiguous(frame(NOTICE))

    def test_wrapped_own_notice_does_not_authorise_a_flush(self):
        self.flock._remember_typed("w2:p1", NOTICE)
        self.check_ambiguous(frame(NOTICE[:48], ["  " + NOTICE[48:]]))

    def test_wrapped_human_draft_is_preserved(self):
        self.check_ambiguous(frame("my unfinished draft", ["  second line", "  third line"]))

    def test_notice_prefix_with_appended_human_text_stays_unknown(self):
        self.check_ambiguous(frame(NOTICE, ["  do not submit this human addition"]))

    def test_placeholder_with_footer_shaped_human_continuation_is_unknown(self):
        self.check_ambiguous(frame("\x1b[2mAsk Codex to do anything\x1b[0m",
                                   ["  human draft · still unfinished"]))

    def test_whitespace_and_ansi_only_rows_do_not_prove_empty(self):
        screen = frame(NOTICE).replace("\n\n\n" + FOOTER,
                                       "\n \x1b[0m\n\t\n" + FOOTER)
        self.check_ambiguous(screen)

    def test_empty_placeholder_is_still_explicitly_empty(self):
        screen = frame("\x1b[2mAsk Codex to do anything\x1b[0m")
        self.terminal.set_screen("w2:p1", screen)
        text, detail = core.box_text(screen)
        self.assertEqual("", text)
        self.assertNotIn("ambiguous", detail)
        self.assertIs(False, self.flock._box_holds("w2:p1", NOTICE))
        self.assertIs(False, self.flock.flush_box("w2:p1")["flushed"])
        self.assertEqual([], self.terminal.keys)

    def test_historical_transcript_and_hint_stays_out_of_draft_text(self):
        root = Path(__file__).parent / "fixtures"
        for name in ("codex-idle-footer-pane17.ansi", "codex-pane17-transcript-plus-hint.ansi"):
            with self.subTest(fixture=name):
                self.assertEqual("", core.box_text((root / name).read_text())[0])

    def test_distant_prompt_does_not_expand_the_guard_input_region(self):
        screen = "\n".join(["› old transcript", *["response row"] * 20, FOOTER])
        self.terminal.set_screen("w2:p1", screen)
        self.assertIsNone(core._typed_text(screen))
        self.assertIs(True, self.flock._ensure_box_clear("w2:p1"))
        self.assertEqual([], self.terminal.keys)

    def test_dialog_guard_still_refuses_without_keys(self):
        self.terminal.set_screen("w2:p1", codex_approval_dialog())
        send = Mock(return_value=(True, "sent"))
        sent, _, _ = self.flock.send_guarded("w2:p1", send)
        self.assertIs(False, sent)
        send.assert_not_called()
        self.assertEqual([], self.terminal.keys)


if __name__ == "__main__":
    unittest.main()
