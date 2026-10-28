"""Bounded timer evidence: metric reason agrees with the skip classifier."""
import json
import unittest
from unittest.mock import Mock, patch

import _bootstrap  # noqa: F401
import dunk_core as core
import dunk_metrics as metrics
from fakes import FakeHerdr, claude_box_pending

PANE = "w2:p1"
PRIVATE_TEXT = "fixture-only private message never belongs in telemetry"


class BoundedStop:
    """Run synchronously until one metric, with no sleeps or timer thread."""
    def __init__(self):
        self.stopped = False
        self.waits = 0

    def is_set(self):
        return self.stopped

    def set(self):
        self.stopped = True

    def wait(self, _seconds):
        self.waits += 1
        if self.waits > 4:
            raise AssertionError("timer failed to reach a metric in bounded steps")
        return self.stopped


class BackpressureMetricTests(unittest.TestCase):
    def setUp(self):
        self.terminal = FakeHerdr()
        self.flock = core.Flock(herdr=self.terminal, persist=False, restore=False)
        self.addCleanup(self.flock.close)
        self.dunk = core.Dunker(
            "d-test", target_pane_id=PANE, text=PRIVATE_TEXT,
            running=True, interval_minutes=1, next_send_at=999,
            send_count=1, last_sent_text=PRIVATE_TEXT, last_sent_at=900,
            awaiting_consumption=True, skip_if_unconsumed=True,
            hold_while_typing=True, skip_count=2, total_skips=7)
        self.flock.dunkers = [self.dunk]
        self.stop = BoundedStop()
        self.addCleanup(patch.stopall)
        for target in ("subprocess.Popen", "os.system", "os.execv", "os.execve", "os.kill"):
            patch(target, side_effect=AssertionError("external operation forbidden")).start()

    def run_one_metric(self):
        record = self.flock.metrics.record
        def record_and_stop(*args, **kwargs):
            record(*args, **kwargs)
            self.stop.set()
        with patch.object(core.time, "time", return_value=1000), \
                patch.object(self.flock.metrics, "record", side_effect=record_and_stop):
            self.flock._timer_loop(self.dunk, self.stop)
        rows = [dict(row) for row in self.flock.metrics._rows(0, 2000)]
        self.assertEqual(1, len(rows), self.dunk.last_error)
        self.assertNotEqual("Error", self.dunk.status)
        self.assertNotIn(PRIVATE_TEXT, json.dumps(rows))
        self.assertEqual([], self.terminal.sends)
        self.assertEqual([], self.terminal.keys)
        return rows[0]

    def check_skip(self, screen, reason):
        self.terminal.set_screen(PANE, screen)
        row = self.run_one_metric()
        self.assertEqual("dunk.skipped", row["kind"])
        self.assertEqual("held", row["outcome"])
        self.assertEqual((PANE, self.dunk.id), (row["pane_id"], row["source"]))
        self.assertEqual(reason, self.dunk.last_skip_reason)
        self.assertEqual((3, 8), (self.dunk.skip_count, self.dunk.total_skips))
        self.assertEqual((1, 900), (self.dunk.send_count, self.dunk.last_sent_at))
        self.assertEqual((1000, 1060), (self.dunk.last_skipped_at, self.dunk.next_send_at))
        self.assertTrue(self.dunk.running)
        self.assertTrue(self.dunk.hold_while_typing)
        self.assertEqual(reason, row["reason"])

    def test_unconsumed_skip_names_the_actual_classification(self):
        self.check_skip(claude_box_pending(PRIVATE_TEXT), "unconsumed")

    def test_unrecognised_input_is_not_claimed_unconsumed_or_stranded(self):
        self.check_skip("no recognised input box", "unrecognised")
        self.assertEqual(1, self.dunk.unknown_skips)

    def test_actual_stranded_text_remains_a_guard_hold(self):
        self.dunk.skip_if_unconsumed = False
        self.terminal.set_screen(PANE, claude_box_pending(PRIVATE_TEXT))
        self.flock.sent_text_by_pane[PANE] = PRIVATE_TEXT
        # The terminal refusing to submit is the boundary, not a mocked classifier.
        with patch.object(self.flock, "_flush_once", return_value=False):
            row = self.run_one_metric()
        self.assertEqual(("dunk.held", "stranded-will-not-submit"),
                         (row["kind"], row["reason"]))
        self.assertEqual((2, 7, 1),
                         (self.dunk.skip_count, self.dunk.total_skips, self.dunk.send_count))

    def test_dead_session_still_refuses_without_a_backpressure_event(self):
        self.terminal.set_screen(PANE, core.DEAD_SESSION_MARKERS[0])
        send = Mock(side_effect=AssertionError("must not send"))
        sent, ok, detail = self.flock.send_guarded(PANE, send)
        self.assertIs(False, sent)
        self.assertIsNone(ok)
        self.assertIn("dead session", detail)
        send.assert_not_called()
        self.assertEqual([], self.flock.metrics._rows(0, 2**40))

    def test_existing_reason_rows_and_schema_are_preserved(self):
        for reason in ("stranded", "unconsumed", "unrecognised"):
            self.flock.metrics.record("dunk.skipped", reason=reason, at=1000)
        counts, _, _ = self.flock.metrics.summary(0, 2000)
        self.assertEqual({"stranded": 1, "unconsumed": 1, "unrecognised": 1}, counts["by_reason"])
        self.assertEqual(1, metrics.SCHEMA_VERSION)
        for reason in counts["by_reason"]:
            self.assertIn(reason, metrics.REASONS)


if __name__ == "__main__":
    unittest.main()
