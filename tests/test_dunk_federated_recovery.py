"""Owner-authorized federated input: native clipped composers are pending input."""
import os
import sys
import unittest
from unittest import mock
sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap
import dunk_core
from fakes import FakeHerdr

PANE = 'w2:p1'
FRAME = ('\x1b[1;2m›\x1b[0m previous truncated request…\n'
         '  An arbitrary agent response, not equal to the previous request.\n\n'
         '\x1b[1m›\x1b[0m visible tail of an owner message    \x1b[2m↑\x1b[0m\n'
         '  continuation and reply receipt\n\n\n'
         '  GPT-6-Astra high · ~/workspace/chief-of-staff · Read the inbox message\n')
EMPTY = '\x1b[1m›\x1b[0m Ask Codex to do anything\n\n  GPT-6-Astra high · ~/workspace/chief-of-staff · Title\n  ← for agents · ? for shortcuts\n'

class FederatedTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
            stuck_config={'enabled': True, 'threshold_s': 1, 'max_attempts': 1,
                          'cooldown_s': 1, 'observation_interval_s': .1})
        self.flock.federated_input = True
        self.herdr.set_screen(PANE, FRAME)
        self.flock.stuck_candidates[PANE] = dict(message_id='m1', press_enter=True,
            kind='text', status='sent', generation='g1', text='full earlier text')
        self.patches = [mock.patch.object(self.flock, '_pane_generation', return_value='g1'),
                        mock.patch.object(self.flock, '_dead_session', return_value=None),
                        mock.patch.object(dunk_core, 'SUBMIT_POLL_S', (0,))]
        for p in self.patches: p.start()
    def tearDown(self):
        for p in self.patches: p.stop()
        self.flock.close()
    def test_native_scrolled_pending_and_empty(self):
        self.assertIn('visible tail', self.flock._federated_composer(FRAME))
        self.assertEqual('', self.flock._federated_composer(EMPTY))
    def test_strict_mode_unchanged(self):
        self.flock.federated_input = False
        self.assertIsNone(self.flock._federated_composer(FRAME))
        self.assertFalse(self.flock.composer_region_is_owned(PANE, FRAME))
    def test_unknown_or_second_bold_sigil_refuses(self):
        for prefix in ('› unknown\n', '\x1b[1m›\x1b[0m second\n'):
            self.assertIsNone(self.flock._federated_composer(prefix + FRAME))
    def test_explicit_preserve_draft_never_flushes(self):
        self.assertFalse(self.flock._ensure_box_clear(PANE, preserve_draft=True))
        self.assertFalse(self.herdr.keys)
    def test_pending_native_never_falsely_confirms_sent(self):
        self.assertTrue(self.flock._box_holds(PANE, 'full hidden text'))
    def test_watchdog_waits_then_presses_once_without_replaying_text(self):
        self.assertEqual(0, self.flock._federated_recovery_step(10))
        self.assertEqual(1, self.flock._federated_recovery_step(12))
        self.flock._federated_recovery_step(14)
        self.assertEqual([(PANE, ['Enter'])], self.herdr.keys)
        self.assertFalse(self.herdr.sent.is_set())
    def test_edit_resets_stable_timer(self):
        self.flock._federated_recovery_step(10)
        self.herdr.set_screen(PANE, FRAME.replace('visible tail', 'edited tail'))
        self.assertEqual(0, self.flock._federated_recovery_step(12))
        self.assertFalse(self.herdr.keys)
    def test_no_enter_cancelled_or_replaced_never_press(self):
        for changes in ({'press_enter':False}, {'status':'cancelled'}, {'generation':'other'}):
            self.flock.stuck_candidates[PANE].update(changes)
            self.flock._federated_recovery_step(10)
            self.flock._federated_recovery_step(12)
            self.assertFalse(self.herdr.keys)
    def test_dialog_never_authorizes_keys(self):
        with mock.patch.object(dunk_core, '_modal_open', return_value=True):
            self.flock._federated_recovery_step(10)
            self.flock._federated_recovery_step(12)
        self.assertFalse(self.herdr.keys)
    def test_failed_durable_reservation_never_presses(self):
        self.flock._federated_recovery_step(10)
        with mock.patch.object(self.flock, '_save', return_value=False):
            self.flock._federated_recovery_step(12)
        self.assertFalse(self.herdr.keys)

if __name__ == '__main__': unittest.main()
