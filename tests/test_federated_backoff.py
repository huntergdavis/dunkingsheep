"""A resolved native composer hold must not retain its old retry deadline."""
import os
import sys
import unittest
from unittest import mock
sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap
import dunk_core
from fakes import FakeHerdr
from test_dunk_federated_recovery import EMPTY, FRAME

PANE = 'w2:p1'

class FederatedBackoffTests(unittest.TestCase):
    def setUp(self):
        self.h = FakeHerdr()
        self.f = dunk_core.Flock(herdr=self.h, persist=False, restore=False)
        self.addCleanup(self.f.close)
        self.f.federated_input = True
        self.h.set_screen(PANE, EMPTY)
        self.m = self.f._new_message(PANE, 'Codex', 'owner message', 'send_text',
                                    lambda: (True, 'sent'))
        self.m.next_attempt_at = 500
        self.m.hold_signature = self.f._box_signature(PANE)
        self.f.outbox[PANE] = [self.m]

    def step(self):
        with mock.patch.object(self.f, '_deliver', return_value=True) as deliver, \
             mock.patch.object(self.f, '_clear_to_send', return_value=True):
            result = self.f._outbox_step(PANE, now=100)
        return result, deliver.call_count

    def test_empty_native_composer_clears_old_identical_signature_delay(self):
        self.assertEqual(('box', ''), self.m.hold_signature)
        self.assertEqual(('delivered', 1), self.step())
        self.assertIsNone(self.m.next_attempt_at)

    def test_failed_submission_with_pending_text_still_backs_off(self):
        self.h.set_screen(PANE, FRAME)
        self.m.hold_signature = self.f._box_signature(PANE)
        self.assertEqual(('due', 0), self.step())

    def test_strict_mode_retains_backoff(self):
        self.f.federated_input = False
        self.assertEqual(('due', 0), self.step())

    def test_dialog_retains_backoff_and_sends_nothing(self):
        self.h.set_screen(PANE, 'Choose a model\n❯ 1. fast\nEnter to select · esc to cancel\n')
        self.m.hold_signature = self.f._box_signature(PANE)
        self.assertEqual(('due', 0), self.step())
        self.assertFalse(self.h.keys)

    def test_dead_session_cannot_clear_same_signature_backoff(self):
        with mock.patch.object(self.f, '_dead_session', return_value='session expired'):
            self.assertEqual(('due', 0), self.step())

    def test_unreadable_native_composer_does_not_clear_backoff(self):
        self.h.unreadable.add(PANE)
        self.m.hold_signature = self.f._box_signature(PANE)
        self.assertEqual(('due', 0), self.step())

if __name__ == '__main__':
    unittest.main()
