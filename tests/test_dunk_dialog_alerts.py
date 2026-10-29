import os
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import dunk_dialog_alerts as alerts


class DialogAlertTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = os.path.join(self.tmp.name, 'alerts.db')
        self.owner = Mock()
        self.vp = Mock()

    def notify(self, now=100, generation='pid:1:start:2', signature='Allow command?'):
        return alerts.notify('%2', generation, signature, owner_sink=self.owner,
                             vp_sink=self.vp, path=self.path, now=now)

    def test_durable_dedupe_and_new_dialog_or_generation(self):
        self.assertEqual(len(self.notify()), 2)
        self.assertEqual(self.notify(now=999), [])
        self.assertEqual(len(self.notify(now=1000, signature='Approve network?')), 2)
        self.assertEqual(len(self.notify(now=1001, generation='pid:2:start:3')), 2)
        self.assertEqual(self.owner.call_count, 3)
        self.assertEqual(self.vp.call_count, 3)

    def test_failed_owner_retries_without_repeating_successful_vp(self):
        self.owner.side_effect = RuntimeError('offline')
        self.assertEqual(self.notify()[0], ('owner', False, 'RuntimeError'))
        self.assertEqual(self.notify(now=159), [])
        self.owner.side_effect = None
        self.assertEqual(self.notify(now=160), [('owner', True, None)])
        self.vp.assert_called_once()

    def test_undeliverable_inbox_is_retried(self):
        self.vp.return_value = {"outcome": "undeliverable"}
        self.assertEqual(self.notify()[1], ('vp', False, 'RuntimeError'))
        self.vp.return_value = {"message_id": "i1"}
        self.assertEqual(self.notify(now=160), [('vp', True, None)])
        self.owner.assert_called_once()

    def test_unidentified_pane_does_not_notify(self):
        self.assertEqual(self.notify(generation=None), [])
        self.owner.assert_not_called()

    @patch('dunk_dialog_alerts.subprocess.run')
    @patch.dict(os.environ, {'DUNKINGSHEEP_VPCTL': '/test/vpctl'})
    def test_owner_uses_direct_brief_with_no_shell(self, run):
        alerts.post_owner('Please inspect dialog')
        run.assert_called_once_with(['/test/vpctl', 'brief', '--channel', 'cos'],
                                    input='Please inspect dialog', text=True,
                                    capture_output=True, timeout=15, check=True)


if __name__ == '__main__':
    unittest.main()
