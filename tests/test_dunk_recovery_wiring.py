"""t508: real incident geometry through the daemon observer and guarded transport.

Native files and stores are private fixtures; only process discovery/environment
are faked. The decision, native reader, ledger, restore and transport are real.
Incident source: author4 i33867 /tmp/t504-artifacts/frame-incident-vp0841.txt.
"""
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap  # noqa: F401,E402
import dunk_core
import dunk_recovery_native
from dunk_callers import process_start
from dunk_inbox import Inbox
from dunk_tasks import Tasks
from fakes import FakeHerdr
from test_dunk_stuck_recovery import _RecordingMetrics

PANE = 'w2:p1'
SESSION = '01a1830e-b76d-7dd1-913b-528c44afbb42'
FRAME = Path(__file__).with_name('fixtures').joinpath('t508-cos-ambiguous.txt').read_text()
TEXT = dunk_core._trailing_prompt_region(FRAME).strip()
PREFIX = '\n'.join(FRAME.splitlines()[:10]).replace('› ', '').replace('• ', '').replace('…', '')


class RecoveryWiringTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.rollout = self.home / f'rollout-{SESSION}.jsonl'
        self.cwd = '/home/x/site'
        self.write_native()
        db = sqlite3.connect(self.home / 'state_5.sqlite')
        db.execute('create table threads (id text primary key, rollout_path text, cwd text)')
        db.execute('insert into threads values (?,?,?)', (SESSION, str(self.rollout), self.cwd))
        db.commit()
        db.close()
        self.backend = FakeHerdr()
        self.backend.panes[2]['pane_pid'] = os.getpid()
        self.backend.agent_child_pids = lambda pid: [{
            'pid': os.getpid(), 'start': process_start(os.getpid()), 'comm': 'codex',
            'state': 'S', 'argv': ['codex', 'resume', SESSION]}]
        env = mock.patch.object(dunk_recovery_native, '_process_environment',
                                return_value={'CODEX_HOME': str(self.home)})
        env.start()
        self.addCleanup(env.stop)
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=str(self.home / 'inbox.db'), blob_dir=str(self.home / 'blobs'))
        self.tasks = Tasks(path=str(self.home / 'tasks.db'))
        self.addCleanup(self.inbox.close)
        self.addCleanup(self.tasks.close)
        self.flock = self.make()
        self.addCleanup(lambda: self.flock.close())
        self.backend.screens[PANE] = FRAME
        self.backend.box[PANE] = TEXT
        # The fake renderer redraws a wrapped composer on a successful key.
        original_send = self.backend.send_keys
        def press(pane, keys):
            result = original_send(pane, keys)
            if pane not in self.backend.box:
                self.backend.screens[pane] = '› \n\n  GPT-6-Astra high · ~/workspace · task\n'
            return result
        self.backend.send_keys = press

    def make(self, restore=False):
        return dunk_core.Flock(
            herdr=self.backend, persist=True, restore=restore,
            state_file=str(self.home / 'state.json'), inbox=self.inbox, tasks=self.tasks,
            metrics=self.metrics, stuck_config={'threshold_s': 120, 'max_attempts': 2,
                'observation_interval_s': 20, 'cooldown_s': 60})

    def write_native(self, submitted=False, active=False, authenticated=True):
        events = []
        if authenticated:
            events.append({'type': 'session_meta', 'payload': {'id': SESSION, 'cwd': self.cwd if hasattr(self, 'cwd') else '/home/x/site'}})
        events.append({'type': 'response_item', 'payload': {'type': 'message', 'role': 'assistant',
                       'content': [{'type': 'output_text', 'text': PREFIX}]}})
        if submitted:
            events.append({'type': 'response_item', 'payload': {'type': 'message', 'role': 'user',
                           'content': [{'type': 'input_text', 'text': TEXT}]}})
        if active:
            events.append({'type': 'event_msg', 'payload': {'type': 'task_started', 'turn_id': 'turn'}})
        self.rollout.write_text(''.join(json.dumps(e) + '\n' for e in events))

    def failed(self, scheduled=False):
        self.message = self.flock._new_message(PANE, 'CoS', TEXT, 'd82' if scheduled else 'send_text',
                                               lambda: (False, 'text did not submit; it is still in the input box'))
        self.message.typed_at = time.time()  # original transport successfully typed this fixture
        if scheduled:
            dunk = dunk_core.Dunker(id='d82', text=TEXT, target_pane_id=PANE)
            dunk.running = True
            self.flock.dunkers.append(dunk)
            self.message.scheduled = True
            self.message.schedule_template = TEXT
        # The real failed-delivery bookkeeping and candidate registration, no
        # mocked recovery matcher. The original submit is the injected failure.
        with mock.patch.object(self.flock, 'send_guarded', side_effect=lambda pane, call, **kw: (True, *call())):
            self.flock._deliver(self.message)
        self.assertEqual('failed', self.message.status)
        self.assertNotIn(PANE, self.flock.outbox)
        return self.message

    def tick(self, at):
        return self.flock._stuck_observer_step(now=at)

    def test_real_628_char_incident_reaches_guarded_executor_once(self):
        self.assertEqual(628, len(TEXT))
        self.failed()
        self.assertFalse(self.flock.composer_region_is_owned(PANE, FRAME))
        self.assertEqual('unknown', self.flock.delivery_state(PANE)['state'])
        self.assertEqual(0, self.tick(1000))
        self.assertEqual(0, self.tick(1100))
        self.assertEqual(1, self.tick(1120))
        self.assertEqual([(PANE, ['Enter'])], self.backend.keys)
        self.assertEqual([(PANE, TEXT)], self.backend.sends)
        entry = self.flock.stuck_candidates[PANE]
        self.assertEqual('consumed', entry['recovery_outcome'])
        self.assertFalse(entry['eligible'])
        self.assertEqual('failed', self.flock.decided[self.message.id]['status'], 'retain initial failure')
        self.tick(1300)
        self.assertEqual(1, len(self.backend.keys))

    def test_schedule_false_sent_with_real_frame_recovers(self):
        self.failed(scheduled=True)
        self.message.status = 'sent'
        self.flock._note_stuck_candidate(self.message)
        self.tick(1000)
        self.assertEqual(1, self.tick(1120))

    def test_failure_candidate_and_spent_budget_survive_restart(self):
        self.failed()
        self.backend.enters_needed[PANE] = 99
        self.tick(1000)
        self.tick(1120)
        self.assertEqual(1, len(self.backend.keys))
        self.flock.close()
        self.flock = self.make(restore=True)
        for at in (1300, 1420, 1600):
            self.tick(at)
        self.assertEqual(1, len(self.backend.keys), 'PR109 bound remains one despite legacy config two')

    def test_negative_controls_never_press(self):
        self.failed()
        cases = {
            'human continuation': lambda: self.backend.screens.update({PANE: FRAME.replace('\n\n  GPT', '\ncall Hunter\n\n  GPT')}),
            'human edit': lambda: self.backend.screens.update({PANE: FRAME.replace('messages FIRST', 'messages first')}),
            'inserted sigil': lambda: self.backend.screens.update({PANE: FRAME.replace('› Owner2370', '› my draft\n› Owner2370')}),
            'human line before prompt': lambda: self.backend.screens.update({PANE: FRAME.replace('› Owner2370', 'do not send my unfinished draft\n› Owner2370')}),
            'native consumed': lambda: self.write_native(submitted=True),
            'native active': lambda: self.write_native(active=True),
            'native missing auth': lambda: self.write_native(authenticated=False),
            'native missing file': lambda: self.rollout.unlink(),
            'busy': lambda: self.backend.statuses.update({PANE: 'working'}),
            'dialog': lambda: self.backend.screens.update({PANE: FRAME + '\nDo you want to proceed?\n  1. Yes\n  2. No\n'}),
            'consumed attempt': lambda: self.flock.stuck_candidates[PANE].update(eligible=False, status='consumed'),
            'stale state': lambda: self.flock.stuck_candidates[PANE].update(status='stale'),
            'wrong generation': lambda: self.flock.stuck_candidates[PANE].update(generation='pid:old:start:old'),
            'no durable attempt': lambda: self.flock.stuck_candidates.clear(),
            'untyped intent': lambda: self.flock.stuck_candidates[PANE].update(typed_at=None),
        }
        original = dict(self.flock.stuck_candidates[PANE])
        for name, change in cases.items():
            with self.subTest(name=name):
                self.backend.screens[PANE] = FRAME
                self.backend.statuses[PANE] = 'idle'
                self.write_native()
                self.flock.stuck_candidates[PANE] = dict(original)
                self.flock.stuck.forget(PANE)
                change()
                self.tick(1000)
                self.tick(1120)
                self.assertEqual([], self.backend.keys)

    def test_stale_or_edited_reminders_never_replay_and_schedule_keeps_running(self):
        self.failed(scheduled=True)
        self.message.created_at = time.time() - 3600
        self.flock.stuck_candidates[PANE]['created_at'] = self.message.created_at
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)
        self.assertTrue(self.flock.dunkers[0].running)
        self.message.created_at = time.time()
        self.flock.stuck_candidates[PANE]['created_at'] = self.message.created_at
        self.flock.dunkers[0].text = 'current owner-priority instruction'
        self.tick(1300)
        self.tick(1420)
        self.assertEqual([], self.backend.keys)

    def test_final_boundary_native_start_or_edit_prevents_key(self):
        self.failed()
        self.tick(1000)
        save = self.flock._save
        def race():
            self.write_native(active=True)
            return save()
        with mock.patch.object(self.flock, '_save', side_effect=race):
            self.assertEqual(0, self.tick(1120))
        self.assertEqual([], self.backend.keys)

    def test_reservation_failure_never_presses(self):
        self.failed()
        self.tick(1000)
        with mock.patch.object(self.flock, '_save', return_value=False):
            self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_different_native_process_binding_not_adopted(self):
        self.failed()
        self.tick(1000)
        self.flock.stuck_candidates[PANE]['native_binding'] = [1, 'old', SESSION]
        self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_real_submit_persists_typed_receipt_before_enter_then_observer_recovers(self):
        self.backend.screens[PANE] = '› \n\n GPT-6 high · ~/workspace · task\n'
        self.backend.box.clear()
        send_text = self.backend.send_text
        def type_text(pane, text):
            result = send_text(pane, text)
            self.backend.screens[pane] = FRAME
            return result
        self.backend.send_text = type_text
        self.backend.enters_needed[PANE] = 99
        self.message = self.flock._new_message(PANE, 'CoS', TEXT, 'send_text',
                            lambda: self.flock.submit_text(PANE, TEXT))
        seen = []
        press = self.backend.send_keys
        def inspect_receipt(pane, keys):
            data = json.loads((self.home / 'state.json').read_text())
            seen.append(data['stuck_candidates'][pane]['status'])
            return press(pane, keys)
        self.backend.send_keys = inspect_receipt
        with mock.patch.object(dunk_core.time, 'sleep'):
            self.flock._deliver(self.message)
        self.assertTrue(seen)
        self.assertEqual({'typed'}, set(seen))
        self.assertEqual('failed', self.message.status)
        self.backend.send_keys = press
        self.backend.enters_needed[PANE] = 1
        before = len(self.backend.keys)
        self.tick(1000)
        self.assertEqual(1, self.tick(1120))
        self.assertEqual(before + 1, len(self.backend.keys))

    def test_crash_after_typing_before_enter_restores_the_bound_attempt(self):
        class Crash(BaseException):
            pass
        self.backend.screens[PANE] = '› \n\n GPT-6 high · ~/workspace · task\n'
        self.backend.box.clear()
        send_text = self.backend.send_text
        def type_text(pane, text):
            result = send_text(pane, text)
            self.backend.screens[pane] = FRAME
            return result
        self.backend.send_text = type_text
        message = self.flock._new_message(PANE, 'CoS', TEXT, 'send_text',
                                         lambda: self.flock.submit_text(PANE, TEXT))
        with mock.patch.object(self.backend, 'send_keys', side_effect=Crash):
            with self.assertRaises(Crash):
                self.flock._deliver(message)
        saved = json.loads((self.home / 'state.json').read_text())
        self.assertEqual('typed', saved['stuck_candidates'][PANE]['status'])
        self.assertEqual([], self.backend.keys)
        self.flock.close()
        self.flock = self.make(restore=True)
        self.tick(1000)
        self.assertEqual(1, self.tick(1120))
        self.assertEqual([(PANE, ['Enter'])], self.backend.keys)

    def test_failed_notification_reconciles_after_restart_without_replay(self):
        inbox_id = self.inbox.send('CoS', 'real notification')['message_id']
        self.failed()
        self.message.notify_for = [inbox_id]
        self.flock._note_stuck_candidate(self.message)
        self.inbox.record_notify(inbox_id, 'failed', 'text did not submit')
        self.tick(1000)
        with mock.patch.object(self.inbox, 'record_notify', side_effect=RuntimeError('store busy')):
            self.tick(1120)
        self.assertEqual('failed', self.inbox.status(inbox_id)['notify']['state'])
        self.flock.close()
        self.flock = self.make(restore=True)
        self.tick(1300)
        self.assertEqual('sent', self.inbox.status(inbox_id)['notify']['state'])
        self.assertEqual(1, len(self.backend.keys))

    def test_legacy_bare_codex_requires_generation_bound_registration(self):
        self.failed()
        self.backend.agent_child_pids = lambda pid: [{
            'pid': os.getpid(), 'start': process_start(os.getpid()), 'comm': 'codex',
            'state': 'S', 'argv': ['codex', '-s', 'danger-full-access', '-a', 'never']}]
        bindings = self.home / 'bindings.json'
        record = {'pid': os.getpid(), 'start': process_start(os.getpid()),
                  'cwd': self.cwd, 'codex_home': str(self.home), 'session_id': SESSION}
        bindings.write_text(json.dumps({PANE: record}))
        with mock.patch.object(dunk_recovery_native, '_process_environment', return_value={}):
            self.assertIsNone(self.flock._owned_recovery_evidence(PANE, FRAME))
            with mock.patch.dict(os.environ, {'DUNKINGSHEEP_RECOVERY_BINDINGS': str(bindings)}):
                self.assertIsNotNone(self.flock._owned_recovery_evidence(PANE, FRAME))
                record['start'] = 'stale'
                bindings.write_text(json.dumps({PANE: record}))
                self.assertIsNone(self.flock._owned_recovery_evidence(PANE, FRAME))

    def test_hook_session_bound_to_root_pid_start_and_pane(self):
        self.backend.agent_child_pids = lambda pid: [{
            'pid': os.getpid(), 'start': process_start(os.getpid()), 'comm': 'codex',
            'state': 'S', 'argv': ['codex']}]
        state = self.home / 'hooks'
        state.mkdir()
        record = {'session_id': SESSION, 'pane_id': PANE, 'root_pid': os.getpid(),
                  'root_start': process_start(os.getpid()), 'cwd': self.cwd}
        path = state / f'{SESSION}.json'
        path.write_text(json.dumps(record))
        self.failed()
        with mock.patch.object(dunk_recovery_native, '_process_environment', return_value={
                'CODEX_HOME': str(self.home), 'DUNKINGSHEEP_CODEX_STATE_DIR': str(state)}):
            self.assertIsNotNone(self.flock._owned_recovery_evidence(PANE, FRAME))
            record['root_start'] = 'stale'
            path.write_text(json.dumps(record))
            self.assertIsNone(self.flock._owned_recovery_evidence(PANE, FRAME))

    def test_unbound_failure_has_one_named_diagnostic_and_never_recovers(self):
        self.failed()
        self.message.attempt_generation = None
        with self.assertLogs('dunkingsheep', level='WARNING') as captured:
            self.flock._note_stuck_candidate(self.message)
            self.flock._note_stuck_candidate(self.message)
        self.assertEqual(1, len(captured.output))
        self.assertIn('attempt generation unknown', captured.output[0])
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_missing_native_binding_has_one_named_diagnostic(self):
        self.failed()
        self.backend.agent_child_pids = lambda pid: [{
            'pid': os.getpid(), 'start': process_start(os.getpid()), 'comm': 'codex',
            'state': 'S', 'argv': ['codex']}]
        with mock.patch.object(dunk_recovery_native, '_process_environment', return_value={}):
            with self.assertLogs('dunkingsheep', level='WARNING') as captured:
                self.tick(1000)
                self.tick(1120)
            self.assertEqual(1, len(captured.output))
            self.assertIn('no process-bound native session id', captured.output[0])
        self.assertEqual([], self.backend.keys)

    def test_native_proof_cannot_downgrade_to_legacy_after_spending(self):
        self.failed()
        self.backend.enters_needed[PANE] = 99
        self.tick(1000)
        self.tick(1120)
        self.assertEqual(1, len(self.backend.keys))
        # The renderer now supplies a strict single-sigil frame, but native
        # evidence disappears. That must not buy the legacy actor a fresh key.
        self.backend.screens[PANE] = '› ' + TEXT + '\n\n GPT-6 high · ~/workspace · task\n'
        self.rollout.unlink()
        for at in (1300, 1420, 1600):
            self.tick(at)
        self.assertEqual(1, len(self.backend.keys))

    def test_native_upgrade_does_not_refund_an_existing_legacy_key(self):
        self.failed()
        fingerprint = dunk_core.dunk_stuck.fingerprint(TEXT)
        generation = self.flock._pane_generation(PANE)
        key = self.flock.stuck.ledger_key(PANE, self.message.id, generation, fingerprint)
        self.flock.stuck.spent[key] = 1
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)


if __name__ == '__main__':
    unittest.main()
