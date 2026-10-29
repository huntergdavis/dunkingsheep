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

    def test_recurrent_identical_dialog_is_not_suppressed_forever(self):
        self.notify()
        self.assertEqual(self.notify(now=1899), [])
        self.assertEqual(len(self.notify(now=1900)), 2)

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

class DialogScanTests(unittest.TestCase):
    def test_scan_survives_failure_when_key_recovery_disabled(self):
        import dunk_core
        flock = Mock()
        flock.closed = False
        flock.STUCK_OBSERVE_TICK_S = 5
        flock.stuck_config.enabled = False
        flock._task_stop.wait.side_effect = [False, False, True]
        flock._dialog_observer_step.side_effect = [RuntimeError('transient'), None]
        with patch.object(dunk_core.log, 'exception'):
            dunk_core.Flock._stuck_observer_loop(flock)
        self.assertEqual(flock._dialog_observer_step.call_count, 2)
        flock._stuck_observer_step.assert_not_called()

    def test_no_pending_message_required_and_closed_dialog_resets_interval(self):
        import dunk_core
        flock = Mock()
        flock._dialog_scan_at = 0
        flock._dialog_observations = {}
        flock.herdr.list_panes.return_value = [{'pane_id': '%2', 'agent': 'codex'}]
        flock.read_screen.return_value = 'approval dialog'
        flock._pane_generation.return_value = 'pid:1:start:2'
        with patch.object(dunk_core, '_modal_open', return_value=True):
            dunk_core.Flock._dialog_observer_step(flock, now=100)
            dunk_core.Flock._dialog_observer_step(flock, now=110)
            dunk_core.Flock._dialog_observer_step(flock, now=130)
        self.assertEqual(flock.herdr.list_panes.call_count, 2)
        self.assertEqual(flock._note_stale_panel.call_args.args[2], 100)
        with patch.object(dunk_core, '_modal_open', return_value=False):
            dunk_core.Flock._dialog_observer_step(flock, now=160)
        with patch.object(dunk_core, '_modal_open', return_value=True):
            dunk_core.Flock._dialog_observer_step(flock, now=190)
        self.assertEqual(flock._note_stale_panel.call_args.args[2], 190)
        flock.herdr.send_keys.assert_not_called()


if __name__ == '__main__':
    unittest.main()
