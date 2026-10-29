"""t400: scheduled prompts must reach the existing two-minute recovery actor.

Use the real composer matcher, fake panes, a fake clock and private stores.
The only stub is the original send, simulating a harness swallowing Enter.
"""
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap  # noqa: F401,E402
import dunk_core
from dunk_inbox import Inbox
from dunk_tasks import Tasks
from fakes import FakeHerdr
from test_dunk_stuck_recovery import _RecordingMetrics

PANE = "w2:p1"
TEXT = "Scheduled check d7: read your inbox and finish the assigned work."
FRAME = "\n".join(["Previous response.", "", "› " + TEXT, "", "",
                    "  GPT-6-Astra high · ~/workspace · task", ""])


class ScheduledRecoveryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.backend = FakeHerdr()
        for p in self.backend.panes:
            if p['pane_id'] == PANE:
                p['pane_pid'] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.flock = dunk_core.Flock(
            herdr=self.backend, persist=True, restore=False,
            state_file=os.path.join(self.tmp.name, 'state.json'),
            inbox=Inbox(path=os.path.join(self.tmp.name, 'inbox.db'),
                        blob_dir=os.path.join(self.tmp.name, 'blobs')),
            tasks=Tasks(path=os.path.join(self.tmp.name, 'tasks.db')),
            metrics=self.metrics,
            stuck_config={'threshold_s': 120, 'max_attempts': 1,
                          'observation_interval_s': 20, 'cooldown_s': 60})
        self.addCleanup(self.flock.inbox.close)
        self.addCleanup(self.flock.tasks.close)
        self.addCleanup(self.flock.close)
        self.dunk = dunk_core.Dunker(id='d7', text=TEXT,
                                    target_pane_id=PANE)

    def failed_schedule(self):
        def swallowed(pane, text):
            self.flock._remember_typed(pane, text)
            self.backend.screens[pane] = FRAME
            self.backend.box[pane] = text
            return False, 'text did not submit; it is still in the input box'
        with mock.patch.object(self.flock, 'submit_text', side_effect=swallowed):
            self.flock._do_send(self.dunk)

    def tick(self, at):
        return self.flock._stuck_observer_step(now=at)

    def test_scheduled_failure_is_persisted_with_exact_expanded_text_and_generation(self):
        self.dunk.text = TEXT.replace('d7', '{id}')
        self.failed_schedule()
        entry = self.flock.stuck_candidates[PANE]
        self.assertTrue(entry['eligible'])
        self.assertEqual(TEXT, entry['text'])
        self.assertEqual(self.flock._pane_generation(PANE), entry['generation'])
        import json
        with open(self.flock.state_file) as f:
            self.assertEqual(entry, json.load(f)['stuck_candidates'][PANE])

    def test_real_ambiguous_composer_recovers_once_after_two_minutes(self):
        self.failed_schedule()
        self.assertTrue(self.flock.composer_region_is_owned(PANE, FRAME))
        self.assertEqual('unknown', self.flock.delivery_state(PANE)['state'])
        self.tick(1000)
        self.tick(1100)
        self.assertEqual([], self.backend.keys)
        self.assertEqual(1, self.tick(1120))
        self.assertEqual([(PANE, ['Enter'])], self.backend.keys)
        self.tick(1300)
        self.assertEqual(1, len(self.backend.keys))
        self.assertEqual(['consumed'], [e['outcome'] for e in self.metrics.events
                                       if e['event'] == 'stuck.recover'])

    def test_added_human_continuation_never_submits(self):
        self.failed_schedule()
        self.tick(1000)
        self.backend.screens[PANE] = FRAME.replace('\n\n  GPT', '\n  my unfinished thought\n\n  GPT')
        self.tick(1120)
        self.tick(1300)
        self.assertEqual([], self.backend.keys)
        self.assertNotIn(PANE, self.flock.stuck.records)
        self.assertIsNone(self.flock._box_text(PANE), 'ambiguous is not consumed')

    def test_busy_turn_does_not_submit_owned_prompt(self):
        self.failed_schedule()
        self.backend.statuses[PANE] = 'working'
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_unreadable_then_owned_requires_fresh_stability_window(self):
        self.failed_schedule()
        self.tick(1000)
        self.backend.unreadable.add(PANE)
        self.tick(1060)
        self.backend.unreadable.clear()
        self.tick(1120)
        self.assertEqual([], self.backend.keys)
        self.assertEqual(1, self.tick(1240))

    def test_outbox_witness_uses_same_full_text_and_budget_identity(self):
        self.failed_schedule()
        entry = self.flock.stuck_candidates[PANE]
        kind, content = self.flock._box_signature(PANE)
        self.assertEqual(('box', TEXT), (kind, content))
        message = self.flock._stranded_message(PANE, content)
        self.assertEqual(entry['message_id'], message.id)
        self.flock._unwedge(PANE, content, None, now=1000)
        self.assertTrue(self.flock._unwedge(PANE, content, None, now=1120))
        self.assertEqual(1, len(self.backend.keys))

    def test_unknown_native_status_stays_unknown(self):
        self.failed_schedule()
        self.backend.statuses[PANE] = 'unknown'
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_outbox_cannot_refund_budget_by_changing_to_standalone_observer(self):
        self.failed_schedule()
        # Ignore the recovery Enter so its budget is spent but text stays put.
        self.backend.enters_needed[PANE] = 99
        self.flock._unwedge(PANE, TEXT, None, now=1000)
        self.flock._unwedge(PANE, TEXT, None, now=1120)
        self.assertEqual(1, len(self.backend.keys))
        for at in (1300, 1420, 1600):
            self.tick(at)
        self.assertEqual(1, len(self.backend.keys))

    def test_newer_no_enter_intent_also_blocks_outbox_recovery(self):
        self.failed_schedule()
        newer = self.flock._new_message(PANE, 'Codex', TEXT, 'send_text', lambda: None)
        newer.status = 'sent'
        newer.press_enter = False
        newer.attempt_generation = self.flock._pane_generation(PANE)
        self.flock._remember(newer)
        for at in (1000, 1120, 1300):
            self.flock._unwedge(PANE, TEXT, None, now=at)
        self.assertEqual([], self.backend.keys)

    def test_restart_uses_saved_schedule_and_budget_without_new_send(self):
        self.failed_schedule()
        self.flock.close()
        fresh = dunk_core.Flock(
            herdr=self.backend, state_file=self.flock.state_file,
            persist=True, restore=True, metrics=self.metrics,
            inbox=self.flock.inbox, tasks=self.flock.tasks)
        self.addCleanup(fresh.close)
        # A single-sigil frame needs only the bound candidate, no guessed history.
        self.assertEqual(0, fresh._stuck_observer_step(now=1000))
        self.assertEqual(1, fresh._stuck_observer_step(now=1120))
        self.assertEqual(1, len(self.backend.keys))

    def test_schedule_records_pre_send_generation_not_replacement(self):
        original = self.flock._pane_generation(PANE)
        def changed(pane, text):
            self.flock._remember_typed(pane, text)
            self.backend.screens[pane] = FRAME
            self.backend.box[pane] = text
            return False, 'unconfirmed'
        with mock.patch.object(self.flock, 'submit_text', side_effect=changed):
            self.flock._do_send(self.dunk)
        self.assertEqual(original, self.flock.stuck_candidates[PANE]['generation'])
        with mock.patch.object(self.flock, '_pane_generation', return_value='replacement'):
            self.tick(1000)
            self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_styled_schedule_survives_restart_with_provenance_for_echo(self):
        echo = '[inbox] new message for Codex. Read it with: dunkingsheep inbox read'
        previous = self.flock._new_message(PANE, 'Codex', echo, 'send_text', lambda: None)
        previous.status = 'sent'
        previous.attempt_generation = self.flock._pane_generation(PANE)
        self.flock._remember(previous)
        self.failed_schedule()
        self.backend.screens[PANE] = (
            '\x1b[1;2m›\x1b[0m ' + echo + '\n\n'
            '\x1b[1m›\x1b[0m ' + TEXT + '\n\n\n'
            '  GPT-6-Astra high · ~/workspace · task\n')
        self.flock.close()
        fresh = dunk_core.Flock(
            herdr=self.backend, state_file=self.flock.state_file,
            persist=True, restore=True, metrics=self.metrics,
            inbox=self.flock.inbox, tasks=self.flock.tasks)
        self.addCleanup(fresh.close)
        self.assertEqual(0, fresh._stuck_observer_step(now=1000))
        self.assertEqual(1, fresh._stuck_observer_step(now=1120))
        self.assertEqual(1, len(self.backend.keys))

    def test_unaccounted_styled_human_line_remains_unknown(self):
        self.failed_schedule()
        self.backend.screens[PANE] = (
            '\x1b[1;2m›\x1b[0m a human line never sent by us\n\n'
            '\x1b[1m›\x1b[0m ' + TEXT + '\n\n\n'
            '  GPT-6-Astra high · ~/workspace · task\n')
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)
        self.assertIsNone(self.flock._box_text(PANE))

    def reported_sent_schedule(self, sigil='\x1b[1m›\x1b[0m'):
        def false_confirmation(pane, text):
            self.flock._remember_typed(pane, text)
            self.backend.screens[pane] = FRAME.replace('›', sigil)
            self.backend.box[pane] = text
            return True, 'sent'
        with mock.patch.object(self.flock, 'submit_text', side_effect=false_confirmation):
            self.flock._do_send(self.dunk)

    def test_reviewed_echo_edits_cannot_authorize_recovery(self):
        echo = 'Prior approved schedule: check your task and report the result.'
        for body in (echo + ' HUMAN EDIT', echo + '\nhuman continuation',
                     echo + '\n\nhuman continuation', echo[:5], echo[:5] + '…'):
            with self.subTest(body=body):
                self.flock.sent_text_by_pane[PANE] = [echo]
                self.failed_schedule()
                self.backend.screens[PANE] = (
                    '\x1b[1;2m›\x1b[0m ' + body + '\n\n'
                    '\x1b[1m›\x1b[0m ' + TEXT + '\n\n\n'
                    '  GPT-6-Astra high · ~/workspace · task\n')
                self.tick(1000)
                self.tick(1120)
                self.assertEqual([], self.backend.keys)
                self.assertFalse(self.flock.composer_region_is_owned(
                    PANE, self.backend.screens[PANE]))

    def test_reported_sent_but_bright_pending_schedule_recovers(self):
        # d55 was logged SENT; only watching failed outcomes misses the VP wedge.
        self.reported_sent_schedule()
        self.tick(1000)
        self.tick(1100)
        self.assertEqual([], self.backend.keys)
        self.assertEqual(1, self.tick(1120))
        self.assertEqual(1, len(self.backend.keys))

    def test_long_false_sent_composer_stays_observable(self):
        text = '\n'.join('scheduled line ' + str(i) for i in range(18))
        self.dunk.text = text
        frame = '\x1b[1m›\x1b[0m ' + text + '\n  GPT-6-Astra high · ~/workspace · task\n'
        original = self.backend.send_text
        def send(pane, body):
            result = original(pane, body)
            self.backend.screens[pane] = frame
            return result
        self.backend.send_text = send
        self.backend.enters_needed[PANE] = 999
        with mock.patch.object(dunk_core, 'SUBMIT_GAPS', [0]), \
                mock.patch.object(dunk_core, 'SUBMIT_POLL_S', [0]), \
                mock.patch.object(dunk_core, 'SUBMIT_VERIFY_GAPS', [0, 0]):
            self.flock._do_send(self.dunk)
        before = len(self.backend.keys)
        self.assertEqual('sent', self.flock.stuck_candidates[PANE]['status'])
        self.assertEqual(dunk_core._collapse(text), dunk_core._collapse(self.flock._box_text(PANE)))
        self.tick(1000)
        self.tick(1120)
        self.tick(1300)
        self.assertEqual(before + 1, len(self.backend.keys))
        self.assertFalse(any(e.get('outcome') == 'consumed' for e in self.metrics.events))

    def test_reported_sent_dim_transcript_is_not_submitted_again(self):
        self.reported_sent_schedule(sigil='\x1b[1;2m›\x1b[0m')
        for at in (1000, 1120, 1300):
            self.tick(at)
        self.assertEqual([], self.backend.keys)

    def test_reported_sent_unknown_style_is_not_submission_authority(self):
        self.reported_sent_schedule(sigil='›')
        for at in (1000, 1120, 1300):
            self.tick(at)
        self.assertEqual([], self.backend.keys)

    def test_sent_schedule_style_change_before_press_refuses(self):
        self.reported_sent_schedule()
        self.tick(1000)
        save = self.flock._save
        def raced_save():
            self.backend.screens[PANE] = FRAME.replace('›', '\x1b[1;2m›\x1b[0m')
            return save()
        with mock.patch.object(self.flock, '_save', side_effect=raced_save):
            self.tick(1120)
        self.assertEqual([], self.backend.keys)

    def test_genuinely_consumed_schedule_retires_without_a_recovery(self):
        self.reported_sent_schedule()
        self.backend.screens[PANE] = '› Ask Codex to do anything\n\n  GPT-6-Astra high · ~/workspace · task\n'
        self.backend.box[PANE] = ''
        self.tick(1000)
        self.tick(1120)
        self.assertEqual([], self.backend.keys)


if __name__ == '__main__':
    unittest.main()
