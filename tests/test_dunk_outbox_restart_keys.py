"""t3370: the fields a restart used to drop from a queued outbox row.

Python's save wrote thirteen keys for a queued message and nothing else, so press_enter,
attempt_generation, scheduled, schedule_template, detail, finished_at and typed_at came back as
the constructor defaults after a restart. A `send --no-enter` that was still queued was typed AND
submitted after a restart (the restored delivery always passed press_enter=True), and a scheduled
template lost its template. The owner's ruling: fix it after cutover in both implementations, with
a restart test for each key. This is the Python half; the Rust half is the same keys in
flock/sends.rs message_snapshot and message_from_snapshot.

Each test saves one held message with the production _save() and restores it in a SECOND Flock
from the same state file, the real restart shape. The restore runs through _restore_outbox with
_enqueue captured rather than started: a live enqueue starts a delivery thread, which types the
message and rewrites typed_at and finished_at before a test can look, so the test would be looking
at the delivery, not at the restore.
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class OutboxRestartKeysTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="outbox-keys-")
        self.state = os.path.join(self.tmp, "dunks.json")

    def _daemon(self, restore=True):
        return Flock(herdr=FakeHerdr(), state_file=self.state, restore=restore)

    def _save_held(self, **fields):
        """The first process: one held message with these fields, saved by the production _save."""
        first = self._daemon()
        try:
            message = first._new_message("w1:p2", "Claude Proj", "held text", "test",
                                         lambda: None)
            for name, value in fields.items():
                setattr(message, name, value)
            with first._lock:
                first.outbox.setdefault(message.pane_id, []).append(message)
            first._save()
            return message.id
        finally:
            first.close()

    def _restart(self):
        """The second process, with no delivery started: its restore is what the test inspects."""
        second = self._daemon(restore=False)
        second._enqueue = lambda message: None
        return second

    def _restored(self, second, message_id):
        """The message the restart brings back, found by id in the state file's outbox rows."""
        with open(self.state, encoding="utf-8") as handle:
            rows = json.load(handle).get("outbox") or []
        restored = [m for m in second._restore_outbox(rows) if m.id == message_id]
        self.assertEqual(1, len(restored), "the held message comes back after a restart")
        return restored[0]

    def _round_trip(self, name, value):
        message_id = self._save_held(**{name: value})
        second = self._restart()
        try:
            return getattr(self._restored(second, message_id), name)
        finally:
            second.close()

    # -- one restart test per key ----------------------------------------------------------

    def test_press_enter_false_survives_a_restart(self):
        """The reviewer's case: a no-Enter send must stay a no-Enter send after a restart."""
        self.assertIs(False, self._round_trip("press_enter", False))

    def test_scheduled_survives_a_restart(self):
        self.assertIs(True, self._round_trip("scheduled", True))

    def test_schedule_template_survives_a_restart(self):
        self.assertEqual("run the nightly check",
                         self._round_trip("schedule_template", "run the nightly check"))

    def test_attempt_generation_survives_a_restart(self):
        self.assertEqual("pane-gen-7", self._round_trip("attempt_generation", "pane-gen-7"))

    def test_typed_at_survives_a_restart(self):
        self.assertEqual(1793000123.5, self._round_trip("typed_at", 1793000123.5))

    def test_detail_survives_a_restart(self):
        self.assertEqual("waiting for the box", self._round_trip("detail", "waiting for the box"))

    def test_finished_at_survives_a_restart(self):
        self.assertEqual(1793000456.25, self._round_trip("finished_at", 1793000456.25))

    # -- the delivery honours what the restart restored -------------------------------------

    def test_a_restored_no_enter_send_is_typed_without_enter(self):
        """Restoring the flag is not enough: the restored delivery must pass it on."""
        message_id = self._save_held(press_enter=False)
        second = self._restart()
        try:
            calls = []
            second.submit_text = (lambda pane, text, press_enter=True, check_box=False:
                                  calls.append((pane, text, press_enter)) or (True, ""))
            message = self._restored(second, message_id)
            message.deliver()
            self.assertEqual([("w1:p2", "held text", False)], calls)
        finally:
            second.close()

    # -- a file from before this change keeps the old defaults ------------------------------

    def test_a_row_from_before_the_change_keeps_the_constructor_defaults(self):
        row = {"id": "m7", "pane_id": "w1:p2", "target_label": "Claude Proj", "text": "old row",
               "source": "test", "kind": "text", "hold_while_typing": True,
               "is_notice": False, "notify_for": [], "created_at": 1793000000.0}
        payload = {"version": 1, "dunkers": [], "next_id": 1, "next_message_id": 99,
                   "outbox": [row], "decided": {}}
        with open(self.state, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        second = self._restart()
        try:
            restored = self._restored(second, "m7")
            self.assertIs(True, restored.press_enter)
            self.assertIs(False, restored.scheduled)
            self.assertIsNone(restored.schedule_template)
            self.assertIsNone(restored.attempt_generation)
            self.assertIsNone(restored.typed_at)
            self.assertIsNone(restored.detail)
            self.assertIsNone(restored.finished_at)
        finally:
            second.close()


if __name__ == "__main__":
    unittest.main()
