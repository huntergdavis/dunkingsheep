"""Native queue submission must not wait for an agent's active turn."""
import os
import sys
import unittest
from unittest import mock
sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import _bootstrap
import dunk_core
from fakes import FakeHerdr


class BusyNativeQueueTests(unittest.TestCase):
    def setUp(self):
        self.h = FakeHerdr()
        self.pane = 'w2:p1'
        self.f = dunk_core.Flock(herdr=self.h, persist=False, restore=False)
        self.h.statuses[self.pane] = 'working'
        self.h.screens[self.pane] = '• Working (1m · esc to interrupt)\n› Ask Codex to do anything\n'

    def tearDown(self):
        self.f.close()

    def test_codex_busy_is_deliverable(self):
        self.assertIsNone(self.f.send_blocker(self.pane))

    def test_codex_uses_tab_and_recomputes_after_turn_finishes(self):
        self.assertEqual((True, 'sent', 'Tab'), self.f._send_submit_key(self.pane))
        self.h.statuses[self.pane] = 'idle'
        self.h.screens[self.pane] = '› Ask Codex to do anything\n'
        self.f._submit_key[self.pane] = 'Tab'
        self.assertEqual('Enter', self.f._send_submit_key(self.pane)[2])

    def test_busy_claude_and_pi_keep_enter(self):
        for harness in ('claude', 'pi'):
            for p in self.h.panes:
                if p['pane_id'] == self.pane:
                    p['agent'] = harness
            self.assertIsNone(self.f.send_blocker(self.pane))
            self.assertEqual('Enter', self.f._send_submit_key(self.pane)[2])

    def test_dialog_opening_before_retry_gets_no_key(self):
        self.h.screens[self.pane] = 'Would you like to run the following command?\n❯ 1. Yes\n  2. No\n'
        with mock.patch.object(dunk_core, '_modal_open', return_value=True):
            ok, detail, key = self.f._send_submit_key(self.pane)
        self.assertFalse(ok)
        self.assertIn('dialog', detail)
        self.assertEqual([], self.h.keys)

    def test_native_queue_accepts_text_through_guarded_and_full_paths(self):
        # Model the native contract: while working only Tab queues the text.
        self.h.screens.pop(self.pane)
        original = self.h.send_keys
        queued = []
        def native_keys(pane, key):
            if self.h.statuses[pane] == 'working':
                self.assertEqual('Tab', key)
                queued.append(self.h.box.get(pane))
                return original(pane, 'Enter')
            return original(pane, key)
        self.h.send_keys = native_keys
        with mock.patch.object(dunk_core.time, 'sleep'):
            self.assertTrue(self.f.submit_text(self.pane, 'first')[0])
            self.f._box_ctx = __import__('threading').local()
            self.f._box_ctx.enabled = True
            self.f._box_ctx.checked = True
            self.assertTrue(self.f.submit_text(self.pane, 'second')[0])
            self.h.send_text(self.pane, 'third')
            self.assertTrue(self.f._flush_once(self.pane, 'third'))
        self.assertEqual(['first', 'second', 'third'], queued)
        self.assertNotIn(self.pane, self.h.box)

    def test_long_busy_composer_with_unknown_backend_status_queues(self):
        self.h.statuses[self.pane] = None
        self.h.screens[self.pane] = ('• Working (1m · esc to interrupt)\n'
                                     '› message\n' + '  continuation\n' * 12 +
                                     '  GPT-6-Astra high · ~/project\n')
        self.assertEqual('Tab', self.f._send_submit_key(self.pane)[2])

    def test_pre_paste_busy_hint_survives_marker_scrolling_out(self):
        self.h.statuses[self.pane] = None
        original = self.h.send_text
        def paste(pane, text):
            result = original(pane, text)
            self.h.screens[pane] = '› long composer\n' + '  continuation\n' * 24
            return result
        self.h.send_text = paste
        with mock.patch.object(self.f, '_box_holds', return_value=False), \
             mock.patch.object(dunk_core.time, 'sleep'):
            self.assertTrue(self.f.submit_text(self.pane, 'long message')[0])
        self.assertEqual(['Tab'], self.h.keys[-1][1])

    def test_guarded_retry_queues_again_after_swallowed_first_tab(self):
        self.h.screens.pop(self.pane)
        original = self.h.send_keys
        keys = []
        def swallow_first(pane, key):
            keys.append(key)
            if len(keys) == 1:
                return True, 'key swallowed during paste'
            self.assertEqual('Tab', key)
            return original(pane, 'Enter')
        self.h.send_keys = swallow_first
        self.f._box_ctx = __import__('threading').local()
        self.f._box_ctx.enabled = True
        self.f._box_ctx.checked = True
        with mock.patch.object(dunk_core.time, 'sleep'):
            self.assertTrue(self.f.submit_text(self.pane, 'retry text')[0])
        self.assertEqual(['Tab', 'Tab'], keys)
        self.assertEqual([(self.pane, 'retry text')], self.h.sends)

    def test_guarded_send_and_flush_use_native_queue_key(self):
        with mock.patch.object(self.f, '_box_holds', return_value=False), \
             mock.patch.object(dunk_core.time, 'sleep'):
            self.assertTrue(self.f._flush_once(self.pane, 'queued text'))
            self.assertEqual('Tab', self.h.keys[-1][1][-1])
            self.f._box_ctx = __import__('threading').local()
            self.f._box_ctx.enabled = True
            self.f._box_ctx.checked = True
            self.assertTrue(self.f.submit_text(self.pane, 'queued text')[0])
            self.assertEqual('Tab', self.h.keys[-1][1][-1])


if __name__ == '__main__':
    unittest.main()
