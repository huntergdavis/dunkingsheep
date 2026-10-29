"""t405 offline regressions: fake terminal only; no native pane inputs."""
import os
import sys
import unittest
from unittest import mock
sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import _bootstrap
import dunk_core as dc
from dunk_core import Flock
from fakes import FakeHerdr
from dunk_terminal import TmuxBackend

PANE = 'w2:p1'
COMMAND = '/model gpt-6.1-sol --reasoning=high'
SUFFIX = 't-6.1-sol --reasoning=high'


class ModelSuffixTests(unittest.TestCase):
    def setUp(self):
        self.h = FakeHerdr()
        self.f = Flock(herdr=self.h, persist=False, restore=False)
        self.addCleanup(self.f.close)

    def test_known_model_command_suffix_is_not_submitted_by_notification_preparation(self):
        self.f._remember_typed(PANE, COMMAND)
        self.h.screens[PANE] = '› ' + SUFFIX + '\n  GPT-6-Astra high · ~/work\n'
        self.h.box[PANE] = SUFFIX
        with mock.patch.object(self.f, '_flush_once', return_value=True) as flush:
            self.assertFalse(self.f._ensure_box_clear(PANE))
        flush.assert_not_called()
        self.assertEqual('partial-model-command', self.f._last_block_reason[PANE])
        self.assertEqual([], self.h.keys)

    def test_suffix_from_pending_command_is_also_held(self):
        message = self.f._new_message(PANE, 'fixture', COMMAND, 'send_text', lambda: (True, 'sent'))
        self.f.outbox[PANE] = [message]
        self.h.screens[PANE] = '› ' + SUFFIX + '\n'
        with mock.patch.object(self.f, '_flush_once', return_value=True) as flush:
            self.assertFalse(self.f._ensure_box_clear(PANE))
        flush.assert_not_called()

    def test_complete_owned_command_remains_eligible_for_owned_recovery(self):
        self.f._remember_typed(PANE, COMMAND)
        self.h.screens[PANE] = '› ' + COMMAND + '\n'
        with mock.patch.object(self.f, '_flush_once', return_value=True) as flush:
            self.assertTrue(self.f._ensure_box_clear(PANE))
        flush.assert_called_once()

    def test_human_draft_is_preserved_when_requested(self):
        self.h.screens[PANE] = '› unfinished human draft\n'
        self.assertFalse(self.f._ensure_box_clear(PANE, preserve_draft=True))
        self.assertEqual([], self.h.keys)

    def test_model_picker_up_emits_only_up_not_preparatory_enter(self):
        self.h.screens[PANE] = ('Select Model and Effort\n'
                               '› 2. GPT-6-Astra (current)\n'
                               'enter select · esc back\n')
        with mock.patch.object(self.f, '_flush_once', return_value=True) as flush:
            sent, ok, _ = self.f.send_guarded(PANE, lambda: self.f.press_keys(PANE, ['Up']), allow_modal=True)
        self.assertTrue(sent and ok)
        flush.assert_not_called()
        self.assertEqual([(PANE, ['Up'])], self.h.keys)

    def test_reasoning_picker_blocks_text_but_allows_explicit_escape(self):
        self.h.screens[PANE] = ('Select Reasoning Level for GPT-6-Astra\n'
                               '› 2. Medium (default)\n'
                               'enter default · s session · esc back\n')
        sent, _, _ = self.f.send_guarded(PANE, lambda: self.h.send_text(PANE, 'notification'))
        self.assertFalse(sent)
        sent, ok, _ = self.f.send_guarded(PANE, lambda: self.f.press_keys(PANE, ['Escape']), allow_modal=True)
        self.assertTrue(sent and ok)
        self.assertEqual([(PANE, ['Escape'])], self.h.keys)
        self.assertEqual([], self.h.text_only)

    def test_old_picker_footer_above_current_composer_is_transcript(self):
        screen = ('Select Model and Effort\nenter select · esc back\n'
                  '› new human draft\nGPT-6-Astra high · ~/work\n')
        self.assertFalse(dc._modal_open(screen))

    def test_busy_marker_does_not_authorize_suffix_submission(self):
        self.f._remember_typed(PANE, COMMAND)
        self.h.screens[PANE] = '• Working (esc to interrupt)\n\n› ' + SUFFIX + '\n'
        with mock.patch.object(self.f, '_flush_once', return_value=True) as flush:
            self.assertFalse(self.f._ensure_box_clear(PANE))
        flush.assert_not_called()

    def test_backend_emits_complete_model_payload_as_one_literal_argument(self):
        backend = TmuxBackend(tmux_bin='/bin/false')
        with mock.patch.object(backend, '_tmux', return_value=(0, '', '')) as emit:
            self.assertTrue(backend.send_text('%fixture', COMMAND)[0])
        emit.assert_called_once_with('send-keys', '-t', '%fixture', '-l', '--', COMMAND)
        # This checks emitted argv, not the TUI's interpretation or live bytes.


if __name__ == '__main__':
    unittest.main()
