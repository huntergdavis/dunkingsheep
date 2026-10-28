"""t372: isolated SQL stores, real Flock handlers, fake terminal only.

No daemon, live database, account mutation or live pane is used. Independent
connections and process death test durability, rather than trusting return values.
"""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import _bootstrap  # noqa: F401
import dunk_core
import dunk_dispatch
from dunk_core import Flock
from dunk_inbox import Inbox
from dunk_tasks import Tasks
from fakes import FakeHerdr
from dunk_callers import caller_from_peer

PANE = 'w1:p1'
CALLER = caller_from_peer(os.getpid())


class WiringTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='t372-private-')
        self.addCleanup(self.tmp.cleanup)
        self.tasks_path = os.path.join(self.tmp.name, 'tasks.db')
        self.inbox_path = os.path.join(self.tmp.name, 'inbox.db')
        self.herdr = FakeHerdr()
        self.herdr.panes[0].update(agent='pi', agent_status='idle')
        self.herdr.screens[PANE] = '\u276f\u00a0\n'
        self.f = self.flock()
        self.tasks = self.f.tasks
        self.a = self.add('A')
        self.tasks.claim_next(pane_id=PANE, caller=CALLER)
        self.b = self.add('B')

    def flock(self):
        f = Flock(herdr=self.herdr, state_file=os.path.join(self.tmp.name, 'state.json'),
                  launches_file=os.path.join(self.tmp.name, 'launches.json'),
                  persist=False, restore=False,
                  tasks=Tasks(self.tasks_path),
                  inbox=Inbox(self.inbox_path, blob_dir=os.path.join(self.tmp.name, 'blobs')))
        f._caller_state.value = CALLER
        self.addCleanup(f.inbox.close)
        self.addCleanup(f.tasks.close)
        self.addCleanup(f.metrics.close)
        self.addCleanup(f.close)
        return f

    def add(self, title, **kw):
        kw.setdefault('assignees', [PANE])
        return self.f.tasks.add('global', title, body='execute the fixture', **kw)['id']

    def complete(self, f=None):
        return (f or self.f).task_complete(self.a, self_pane_id=PANE)

    def handoff(self, f=None):
        return (f or self.f).tasks.next_job_handoffs(PANE)[0]

    def test_commit_visible_before_return_and_duplicate_does_not_advance(self):
        c = self.add('C')
        result = self.complete()
        other = sqlite3.connect(self.tasks_path)
        self.addCleanup(other.close)
        self.assertEqual('done', other.execute('SELECT status FROM tasks WHERE id=?', (self.a,)).fetchone()[0])
        lease = other.execute('SELECT pane_id,caller_pid,caller_start FROM task_leases WHERE task_id=?', (self.b,)).fetchone()
        self.assertEqual((PANE, CALLER['pid'], CALLER['start']), lease)
        h = self.handoff()
        self.assertEqual(result['completed_at'], h['completion_at'])
        self.assertEqual(self.a, h['from_task'])
        self.assertEqual('queued', self.f.inbox.status(h['message_id'])['state'])
        self.assertEqual('already_completed', self.complete()['next_job']['reason'])
        self.assertEqual('open', self.tasks.get(c)['status'])
        self.assertEqual(1, self.tasks.get(self.b)['attempts'])

    def test_crash_after_commit_before_inbox_replays_one_intent(self):
        with self.assertLogs('dunkingsheep.next_job', level='ERROR'), \
             mock.patch.object(self.f.next_jobs, 'materialise', side_effect=RuntimeError('crash boundary')):
            self.complete()
        h = self.handoff()
        fresh = self.flock()
        fresh.next_jobs.materialise(h)
        fresh.next_jobs.materialise(h)
        self.assertEqual(1, fresh.inbox.db.execute('SELECT COUNT(*) FROM messages').fetchone()[0])
        self.assertEqual(h['message_id'], self.handoff(fresh)['message_id'])
        self.assertEqual(1, fresh.tasks.get(self.b)['attempts'])

    def test_claim_and_handoff_roll_back_together(self):
        with mock.patch.object(self.tasks, 'record_next_job_handoff', side_effect=RuntimeError('disk full')):
            with self.assertRaises(RuntimeError):
                self.complete()
        self.assertEqual('in_progress', self.tasks.get(self.a)['status'])
        self.assertEqual('open', self.tasks.get(self.b)['status'])
        self.assertEqual([], self.tasks.leases_for(self.b))

    def test_process_exit_after_commit_is_recoverable_without_python_cleanup(self):
        code = """
import os,sys,json
from dunk_tasks import Tasks
s=Tasks(sys.argv[1])
s.advance_next_job(sys.argv[2], trigger='completion', from_task=sys.argv[3],
                   caller=json.loads(sys.argv[4]), unread=0)
os._exit(0)
"""
        subprocess.run([sys.executable, '-c', code, self.tasks_path, PANE, self.a,
                        json.dumps(CALLER)], check=True)
        self.assertEqual('done', self.tasks.get(self.a)['status'])
        self.assertEqual([PANE], self.tasks.get(self.b)['claimed_by'])
        h = self.handoff()
        self.f.next_jobs.recover()
        self.assertEqual(h['message_id'], self.handoff()['message_id'])
        self.assertEqual('sent', self.handoff()['state'])
        self.assertEqual(1, self.f.inbox.db.execute('SELECT COUNT(*) FROM messages').fetchone()[0])

    def test_background_worker_executes_completion_handoff_without_manual_tick(self):
        turn = self.f.turn_start(PANE, source='pi-test')
        worker = threading.Thread(target=self.f._task_push_loop)
        worker.start()
        self.addCleanup(worker.join, 5)
        self.addCleanup(self.f.close)
        self.complete()
        self.f.turn_end(PANE, turn_id=turn['turn_id'], authenticated=True)
        self.assertTrue(self.herdr.sent.wait(5), 'background worker never delivered B')
        b_turn = self.f.turn_start(PANE, source='pi-test', handoff_id=self.handoff()['message_id'])
        deadline = time.monotonic() + 5
        while self.handoff()['state'] != 'started' and time.monotonic() < deadline:
            time.sleep(.01)
        h = self.handoff()
        self.assertEqual('started', h['state'])
        self.assertEqual(b_turn['turn_id'], h['turn_id'])
        self.f.inbox.mark_read([h['message_id']])
        self.f.task_complete(self.b, self_pane_id=PANE)
        self.assertEqual('done', self.tasks.get(self.b)['status'])

    def test_concurrent_completion_end_restart_have_one_claim(self):
        self.add('C')
        barrier = threading.Barrier(3)
        def run(trigger):
            store = Tasks(self.tasks_path)
            barrier.wait()
            try:
                return store.advance_next_job(PANE, trigger=trigger,
                                              from_task=self.a if trigger == 'completion' else None,
                                              caller=CALLER, unread=0)
            finally:
                store.close()
        with ThreadPoolExecutor(max_workers=3) as pool:
            list(pool.map(run, ('completion', 'turn.end', 'restart')))
        self.assertEqual(1, len(self.tasks.next_job_handoffs(PANE)))
        self.assertEqual(1, self.tasks.get(self.b)['attempts'])

    def test_priority_dependency_assignment_and_stable_tie_order(self):
        self.tasks.block(self.b, 'owner hold')
        self.add('unowned', assignees=[], priority='urgent')
        self.add('foreign', assignees=['%15'], priority='urgent')
        dep = self.add('dependency', assignees=['%15'])
        self.add('gated', priority='urgent', depends_on=[dep])
        winner = self.add('eligible', priority='high', depends_on=[self.a])
        self.assertEqual(winner, self.complete()['next_job']['task_id'])
        self.assertEqual([], self.tasks.get(winner)['blocked_by'])

    def test_quiet_no_assignment_records_explicit_reason(self):
        self.tasks.assign(self.b, ['%15'], mode='set')
        out = self.complete()
        self.assertEqual('no_assigned_work', out['next_job']['reason'])
        self.f.next_jobs.recover()
        self.assertEqual([], self.herdr.sends)
        self.assertEqual([], self.tasks.next_job_handoffs())

    def test_unknown_unread_is_not_permission(self):
        with mock.patch.object(self.f, 'inbox_count', side_effect=RuntimeError('db busy')):
            self.assertEqual('unread_unknown', self.complete()['next_job']['reason'])
        self.assertEqual('open', self.tasks.get(self.b)['status'])
        self.f.next_jobs.recover()
        self.assertEqual('in_progress', self.tasks.get(self.b)['status'])

    def test_delivery_receipt_and_actual_new_turn_are_distinct(self):
        old = self.f.turn_start(PANE, source='pi-test')
        self.complete()
        # Genuine end of A, not a fabricated busy/idle proxy.
        self.f.turn_end(PANE, turn_id=old['turn_id'], authenticated=True)
        self.f.next_jobs.tick()
        h = self.handoff()
        self.assertEqual('sent', h['state'], h)
        self.assertEqual('sent', self.f.inbox.status(h['message_id'])['notify']['state'])
        self.assertNotIn('turn_id', h)
        turn = self.f.turn_start(PANE, source='pi-test', handoff_id=h['message_id'])
        h = self.handoff()
        self.assertEqual(turn['turn_id'], h['turn_id'])
        self.assertNotEqual(old['turn_id'], h['turn_id'])
        self.f.inbox.mark_read([h['message_id']])
        self.assertIsNotNone(self.f.inbox.status(h['message_id'])['read_at'])
        self.assertEqual(1, len(self.herdr.sends))
        self.f.task_complete(self.b, self_pane_id=PANE)
        self.assertEqual('done', self.tasks.get(self.b)['status'])

    def test_human_draft_is_preserved_and_retries_are_bounded(self):
        self.complete()
        draft = 'a human is composing this sentence'
        self.herdr.screens[PANE] = '\u276f ' + draft + '\n'
        with mock.patch.object(dunk_dispatch, 'TURN_END_MIN_GAP_S', 0):
            for _ in range(20):
                self.f.next_jobs.tick()
        h = self.handoff()
        self.assertEqual(dunk_core.SUBMIT_ATTEMPTS, h['attempt'])
        self.assertEqual('undeliverable', h['state'])
        self.assertEqual('retry_exhausted', h['reason'])
        self.assertEqual('undeliverable', self.f.inbox.status(h['message_id'])['notify']['state'])
        self.assertEqual([], self.herdr.sends)
        self.assertEqual([], self.herdr.keys)
        self.assertIn(draft, self.herdr.screens[PANE])
        self.assertEqual('in_progress', self.tasks.get(self.b)['status'])

    def test_no_busy_retry_and_restart_keeps_attempt_count(self):
        self.complete()
        self.herdr.screens[PANE] = '\u276f human draft\n'
        self.f.next_jobs.tick()
        self.assertEqual(1, self.handoff()['attempt'])
        fresh = self.flock()
        fresh.next_jobs.recover()
        for _ in range(10):
            fresh.next_jobs.tick()
        self.assertEqual(1, self.handoff(fresh)['attempt'])

    def test_restart_in_uncertain_send_window_does_not_resend(self):
        self.complete()
        h = self.handoff()
        self.tasks.update_next_job(h, state='sending', attempt=1, attempt_at=1.)
        self.f.inbox.record_notify(h['message_id'], 'queued', attempt=1,
                                   transport_id=h['message_id'] + ':1')
        fresh = self.flock()
        fresh.next_jobs.recover()
        self.assertEqual('delivery_interrupted', self.handoff(fresh)['reason'])
        self.assertEqual([], self.herdr.sends)
        self.assertEqual('in_progress', fresh.tasks.get(self.b)['status'])

    def test_crash_after_attempt_reservation_before_inbox_is_recoverable(self):
        with self.assertLogs('dunkingsheep.next_job', level='ERROR'), \
             mock.patch.object(self.f.next_jobs, 'materialise', side_effect=RuntimeError('crash boundary')):
            self.complete()
        self.tasks.update_next_job(self.handoff(), state='sending', attempt=1, attempt_at=1.)
        fresh = self.flock()
        fresh.next_jobs.recover()
        self.assertEqual('interrupted_before_inbox', self.handoff(fresh)['reason'])
        fresh.next_jobs.tick()
        self.assertEqual('sent', self.handoff(fresh)['state'])
        self.assertEqual(1, len(self.herdr.sends))

    def test_receipt_survives_crash_before_journal_update(self):
        self.complete()
        h = self.handoff()
        self.tasks.update_next_job(h, state='sending', attempt=1, attempt_at=1.)
        self.f.inbox.record_notify(h['message_id'], 'sent', transport_id=h['message_id'] + ':1', attempt=1)
        fresh = self.flock()
        fresh.next_jobs.recover()
        self.assertEqual('sent', self.handoff(fresh)['state'])
        self.assertEqual([], self.herdr.sends)

    def test_preexisting_working_status_never_invents_a_turn_id(self):
        self.complete()
        self.f.next_jobs.tick()
        self.herdr.statuses[PANE] = 'working'
        self.f.next_jobs.bind_turn(PANE)
        self.assertEqual('sent', self.handoff()['state'])
        self.assertNotIn('turn_id', self.handoff())

    def test_adjacent_unrelated_turn_cannot_claim_execution_of_b(self):
        self.complete()
        self.f.next_jobs.tick()
        self.f.turn_start(PANE, source='pi-test')
        self.assertEqual('sent', self.handoff()['state'])
        self.assertNotIn('turn_id', self.handoff())
        self.f.turn_start(PANE, source='pi-test', handoff_id='nj-' + '0' * 32)
        self.assertEqual('sent', self.handoff()['state'])

    def test_fast_turn_evidence_survives_until_delivery_is_confirmed(self):
        self.complete()
        actual = {}
        def submit(pane, text):
            token = self.f.turn_start(pane, source='pi-test', handoff_id=self.handoff()['message_id'])
            actual.update(token)
            # This generation ended and was replaced before the submit ladder
            # returned. The persisted observed turn must still name B exactly.
            self.f.turns[pane]['ended_at'] = time.time()
            self.f.turn_start(pane, source='unrelated')
            self.assertEqual('sending', self.handoff()['state'])
            self.assertNotIn('turn_id', self.handoff())
            return True, 'fixture submitted'
        with mock.patch.object(self.f, 'submit_text', side_effect=submit):
            self.f.next_jobs.tick()
        self.assertEqual('started', self.handoff()['state'])
        self.assertEqual(actual['turn_id'], self.handoff()['turn_id'])

    def test_unconfirmed_submit_never_becomes_success(self):
        self.complete()
        with mock.patch.object(self.f, 'submit_text', return_value=(False, 'submission unconfirmed')):
            self.f.next_jobs.tick()
        self.assertEqual('undeliverable', self.handoff()['state'])
        self.f.next_jobs.tick()
        self.assertEqual(1, self.handoff()['attempt'])
        self.assertNotIn('turn_id', self.handoff())

    def test_genuine_end_claims_assigned_job_but_duplicate_end_does_not(self):
        self.tasks.complete(self.a, by=PANE)
        t = self.f.turn_start(PANE, source='pi-test')
        out = self.f.turn_end(PANE, turn_id=t['turn_id'], authenticated=True)
        self.assertEqual('claim', out['dispatch']['action'])
        self.assertEqual(self.b, out['dispatch']['task_id'])
        again = self.f.turn_end(PANE, turn_id=t['turn_id'], authenticated=True)
        self.assertEqual('already_ended', again['reason'])
        self.assertEqual(1, self.tasks.get(self.b)['attempts'])

    def test_cancelled_next_job_is_never_executed(self):
        self.complete()
        self.tasks.cancel(self.b)
        self.f.next_jobs.tick()
        self.assertEqual('cancelled', self.handoff()['state'])
        self.assertEqual([], self.herdr.sends)


class RetryPolicyTests(unittest.TestCase):
    def test_retry_schedule_is_a_pure_rate_input(self):
        fn = dunk_dispatch.delivery_retry_plan
        self.assertEqual('attempt', fn(attempt=0, last_at=None, now=100, max_attempts=4)['action'])
        self.assertEqual('wait', fn(attempt=2, last_at=100, now=159, max_attempts=4, min_gap_s=30)['action'])
        self.assertEqual('attempt', fn(attempt=2, last_at=100, now=160, max_attempts=4, min_gap_s=30)['action'])
        self.assertEqual('stop', fn(attempt=4, last_at=100, now=10000, max_attempts=4)['action'])


if __name__ == '__main__':
    unittest.main()
