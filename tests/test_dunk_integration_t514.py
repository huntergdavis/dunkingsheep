"""t514: seams between PR113/114 (federated owner delivery, now in main) and the
integrated PR112 native-bound recovery executor, on the real captured CoS frame.

Main's `_stuck_observer_step` skips a pane in the legacy loop whenever the
federated parser recognises a styled native composer there ("the explicit
policy has one observer for this pane"). PR112's native-owned evidence lives
inside that legacy loop. These tests pin the consequence: in federated mode
there is still exactly ONE actor and at most ONE key per pane, whichever
parser owns the frame, and the pre-typing observations both sides added to
`submit_text` (busy hint, attempt generation) both happen before any text is
typed. Fixtures and stores are the private ones from test_dunk_recovery_wiring.
"""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap  # noqa: F401,E402
import dunk_core
import test_dunk_recovery_wiring as wiring

FRAME, PANE, TEXT = wiring.FRAME, wiring.PANE, wiring.TEXT
# Reached through the module, never imported by name: a class bound in this
# module's namespace would make the loader run the parent suite a second time.
RecoveryWiringTests = wiring.RecoveryWiringTests

# The live daemon reads styled screens; the captured fixture is plain text. Style
# the two sigils the way Codex draws them (dim for the submitted transcript prompt,
# bold for the live composer) so the federated parser recognises the frame.
STYLED_FRAME = (FRAME.replace('› Owner2278', '\x1b[1;2m›\x1b[0m Owner2278', 1)
                     .replace('› Owner2370', '\x1b[1m›\x1b[0m Owner2370', 1))


class T514IntegrationTests(RecoveryWiringTests):
    def setUp(self):
        super().setUp()
        self.flock.federated_input = True
        sleep = mock.patch.object(dunk_core.time, 'sleep')
        sleep.start()
        self.addCleanup(sleep.stop)

    def sources(self):
        return [e.get('source') for e in self.metrics.events if e.get('event') == 'stuck.recover']

    def test_federated_mode_plain_incident_frame_is_recovered_by_native_path_once(self):
        """The federated parser does not own the plain captured frame, so the
        legacy loop runs and PR112's native evidence recovers it: one key."""
        self.assertIsNone(self.flock._federated_composer(FRAME))
        self.failed()
        self.assertEqual(0, self.tick(1000))
        self.assertEqual(0, self.tick(1100))
        self.assertEqual(1, self.tick(1120))
        self.assertEqual([(PANE, ['Enter'])], self.backend.keys)
        self.assertEqual(['stuck_watchdog'], self.sources())
        self.assertTrue(self.flock.stuck_candidates[PANE].get('native_recovery'))
        self.tick(1300)
        self.tick(1500)
        self.assertEqual(1, len(self.backend.keys), 'no second actor, no second key')

    def test_federated_mode_styled_frame_has_one_observer_and_one_key(self):
        """When the federated parser DOES own the frame, main's actor acts and
        the legacy loop (and with it the native path) must stand down."""
        self.backend.screens[PANE] = STYLED_FRAME
        self.assertEqual(TEXT, self.flock._federated_composer(STYLED_FRAME))
        self.failed()
        self.assertEqual(0, self.tick(1000))
        self.assertEqual(1, self.tick(1120))
        self.assertEqual(1, len(self.backend.keys))
        self.assertEqual(['federated_watchdog'], self.sources())
        self.assertFalse(self.flock.stuck_candidates[PANE].get('native_recovery'),
                         'the native path never engaged on a federated-owned frame')
        self.tick(1300)
        self.tick(1500)
        self.assertEqual(1, len(self.backend.keys), 'federated budget is one key')

    def test_strict_mode_unchanged_by_federated_code(self):
        self.flock.federated_input = False
        self.backend.screens[PANE] = STYLED_FRAME
        self.assertIsNone(self.flock._federated_composer(STYLED_FRAME))
        self.failed()
        self.tick(1000)
        self.assertEqual(1, self.tick(1120))
        self.assertEqual(['stuck_watchdog'], self.sources())

    def test_recovery_box_text_prefers_federated_reader_then_native_evidence(self):
        """Both PRs extended `_recovery_box_text`; the merged order is federated
        parser, delimited composer, native evidence, legacy. Pin it."""
        self.failed()
        self.assertEqual(TEXT, self.flock._recovery_box_text(PANE, STYLED_FRAME).strip())
        self.flock.federated_input = False
        self.assertEqual(TEXT, self.flock._recovery_box_text(PANE, FRAME).strip(),
                         'native evidence still answers when the federated reader is off')

    def test_submit_binds_attempt_generation_and_busy_hint_before_typing(self):
        """The merge conflict: PR113's Codex busy-hint capture and PR112's attempt
        generation binding are both pre-typing observations in one mutex hold."""
        self.backend.screens[PANE] = ('• Working (1m · esc to interrupt)\n'
                                      '› Ask Codex to do anything\n'
                                      '  tab to queue message\n')
        self.backend.box.clear()
        self.backend.statuses[PANE] = 'working'
        message = self.flock._new_message(PANE, 'CoS', TEXT, 'send_text', lambda: (True, 'sent'))
        message.attempt_generation = None
        generation_at_typing = []
        send_text = self.backend.send_text
        def type_text(pane, text):
            generation_at_typing.append(message.attempt_generation)
            return send_text(pane, text)
        self.backend.send_text = type_text
        hints = []
        real_submit_key = self.flock._send_submit_key
        def submit_key(pane, fallback='Enter', busy_hint=False):
            hints.append(busy_hint)
            return real_submit_key(pane, fallback, busy_hint=busy_hint)
        with mock.patch.object(self.flock, '_send_submit_key', side_effect=submit_key), \
                self.flock._attempt_context(message):
            self.flock.submit_text(PANE, TEXT)
        self.assertEqual([self.flock._pane_generation(PANE)], generation_at_typing,
                         'generation bound BEFORE send_text, not from the aftermath')
        self.assertIsNotNone(generation_at_typing[0])
        self.assertTrue(hints and hints[0] is True, 'busy hint captured before typing reached the key')
        self.assertEqual('Tab', self.backend.keys[0][1][0] if isinstance(self.backend.keys[0][1], list)
                         else self.backend.keys[0][1], 'Codex busy turn queues with Tab (PR113)')


# The parent class's own tests already run in their module; do not rerun them here.
for _name in dir(RecoveryWiringTests):
    if _name.startswith('test_') and _name not in T514IntegrationTests.__dict__:
        setattr(T514IntegrationTests, _name, None)


if __name__ == '__main__':
    unittest.main()
