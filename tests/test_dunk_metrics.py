"""Operations telemetry: the event store, the endpoint, and its privacy."""

import json
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import dunk_api  # noqa: E402
import dunk_metrics  # noqa: E402
from dunk_core import Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_metrics import Metrics  # noqa: E402
from fakes import FakeHerdr, claude_box_hint_ansi  # noqa: E402


class StoreIsolationTests(unittest.TestCase):
    """A test must never write to, or tidy up, the live telemetry store."""

    def test_a_persisting_flock_keeps_telemetry_beside_its_state_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            flock = Flock(herdr=FakeHerdr(),
                          state_file=os.path.join(tmp, "dunks.json"))
            try:
                self.assertTrue(flock.metrics.path.startswith(tmp),
                                f"telemetry escaped to {flock.metrics.path}")
            finally:
                flock.close()

    def test_a_non_persisting_flock_writes_no_file_at_all(self):
        flock = Flock(herdr=FakeHerdr(), persist=False, restore=False)
        try:
            self.assertEqual(":memory:", flock.metrics.path)
        finally:
            flock.close()

    def test_the_production_default_is_the_config_directory(self):
        self.assertTrue(dunk_metrics.METRICS_DB.endswith("metrics.db"))
        self.assertIn("dunkingsheep", dunk_metrics.METRICS_DB)


class EventStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.metrics = Metrics(path=os.path.join(self.tmp.name, "m.db"))

    def tearDown(self):
        self.metrics.close()
        self.tmp.cleanup()

    def test_events_are_durable_across_reopening(self):
        self.metrics.record("message.sent", pane_id="w1:p1", duration_ms=120)
        path = self.metrics.path
        self.metrics.close()
        again = Metrics(path=path)
        try:
            counts, _d, _r = again.summary(0, time.time() + 1)
            self.assertEqual(1, counts["events"])
        finally:
            again.close()

    def test_a_window_only_counts_what_is_inside_it(self):
        now = time.time()
        self.metrics.record("message.sent", duration_ms=10, at=now - 7200)
        self.metrics.record("message.sent", duration_ms=20, at=now - 60)
        counts, durations, _r = self.metrics.summary(now - 3600, now + 1)
        self.assertEqual(1, counts["events"])
        self.assertEqual(20, durations["delivery"]["p50"])

    def test_durations_are_null_not_zero_when_nothing_measures_them(self):
        self.metrics.record("message.held", outcome="held", reason="draft")
        _c, durations, retries = self.metrics.summary(0, time.time() + 1)
        self.assertIsNone(durations["delivery"], "null means not measured")
        self.assertIsNone(durations["handling"])
        self.assertIsNone(retries["first_press_success_rate"])

    def test_percentiles_use_nearest_rank(self):
        for value in (10, 20, 30, 40, 100):
            self.metrics.record("message.sent", duration_ms=value)
        _c, durations, _r = self.metrics.summary(0, time.time() + 1)
        delivery = durations["delivery"]
        self.assertEqual(5, delivery["n"])
        self.assertEqual(30, delivery["p50"])
        self.assertEqual(100, delivery["max"])

    def test_the_retry_histogram_counts_enter_presses(self):
        for presses in (1, 2, 2, 3):
            self.metrics.record("submit.attempt", attempts=presses, outcome="ok")
        _c, _d, retries = self.metrics.summary(0, time.time() + 1)
        self.assertEqual({"1": 1, "2": 2, "3": 1}, retries["enter_presses"])
        self.assertEqual(0.25, retries["first_press_success_rate"])

    def test_pruning_drops_old_events_and_keeps_recent_ones(self):
        now = time.time()
        self.metrics.record("message.sent", at=now - 40 * 24 * 3600)
        self.metrics.record("message.sent", at=now - 60)
        self.assertEqual(1, self.metrics.prune()["pruned"])
        counts, _d, _r = self.metrics.summary(0, now + 1)
        self.assertEqual(1, counts["events"])

    def test_recording_never_raises_even_on_a_broken_store(self):
        self.metrics.close()        # the database is gone from under it
        self.metrics.record("message.sent", duration_ms=1)   # must not raise


class MetricsEndpointTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = Metrics(path=os.path.join(self.tmp.name, "m.db"))
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox, metrics=self.metrics)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.metrics.close()
        self.tmp.cleanup()

    def test_the_contract_is_stable_and_self_describing(self):
        report = self.flock.metrics_summary(window_s=3600)
        self.assertEqual(1, report["schema_version"])
        for key in ("generated_at", "interval", "units", "event_definitions",
                    "outcome_definitions", "reason_definitions", "counts",
                    "durations", "retries", "queue", "inbox", "cumulative",
                    "coverage"):
            self.assertIn(key, report, key)
        self.assertEqual("milliseconds", report["units"]["duration"])
        self.assertEqual(3600, report["interval"]["seconds"])
        for key in ("queue_wait", "delivery", "handling", "blocked"):
            self.assertIn(key, report["durations"])
        # It must survive a JSON round trip, since that is how it is consumed.
        self.assertEqual(report, json.loads(json.dumps(report)))

    def test_real_traffic_produces_counts_and_durations(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        sent = self.flock.inbox_send("w2:p1", "a body", subject="a subject",
                                     notify=False, self_pane_id="w1:p2")
        self.flock.inbox_read("w2:p1")
        self.flock.inbox_ack([sent["message_id"]])
        report = self.flock.metrics_summary(window_s=3600)
        counts = report["counts"]
        self.assertEqual(1, counts["by_kind"]["inbox.sent"])
        self.assertEqual(1, counts["by_kind"]["inbox.read"])
        self.assertEqual(1, counts["by_kind"]["inbox.acked"])
        self.assertIn("Claude Proj", counts["by_sender"], "sender at mailbox level")
        self.assertIn("Codex Site", counts["by_recipient"], "recipient at mailbox level")
        self.assertIsNotNone(report["durations"]["queue_wait"])
        self.assertIsNotNone(report["durations"]["handling"])
        self.assertTrue(report["coverage"]["window_has_events"])

    def test_pane_breakdown_only_where_a_pane_id_exists(self):
        self.flock.inbox_send("not-a-running-agent", "body", notify=False)
        report = self.flock.metrics_summary(window_s=3600)
        self.assertEqual({}, report["counts"]["by_pane"],
                         "no pane id on the source event, so no pane row")

    def test_unmeasured_durations_are_null_and_listed_as_missing(self):
        report = self.flock.metrics_summary(window_s=3600)
        self.assertIsNone(report["durations"]["delivery"])
        self.assertIn("delivery", report["coverage"]["missing"])
        self.assertFalse(report["coverage"]["window_has_events"])

    def test_coverage_says_when_recording_began(self):
        self.assertIsNone(self.flock.metrics_summary()["coverage"]["events_since"])
        self.flock.inbox_send("w2:p1", "body", notify=False)
        report = self.flock.metrics_summary()
        self.assertIsNotNone(report["coverage"]["events_since"])
        # A window older than recording says so rather than implying zero.
        old = self.flock.metrics_summary(since=0, until=time.time())
        self.assertIn("only covered from", old["coverage"]["notes"])

    def test_cumulative_dunk_counters_are_separated_from_the_window(self):
        self.flock.add(target="w2:p1", text="x")
        report = self.flock.metrics_summary(window_s=3600)
        self.assertIn("not windowed", report["cumulative"]["note"])
        self.assertEqual(1, len(report["cumulative"]["dunks"]))
        self.assertNotIn("cumulative", report["counts"])

    def test_no_message_text_or_identity_leaks_into_the_report(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        secret_body = "SECRETBODYTEXT that must never appear"
        secret_subject = "SECRETSUBJECTLINE"
        sent = self.flock.inbox_send("w2:p1", secret_body, subject=secret_subject,
                                     notify=True, self_pane_id="w1:p2")
        self.flock.inbox_read("w2:p1")
        self.flock.inbox_ack([sent["message_id"]])
        blob = json.dumps(self.flock.metrics_summary(window_s=3600))
        for forbidden in ("SECRETBODYTEXT", "SECRETSUBJECTLINE", "@",
                          "must never appear"):
            self.assertNotIn(forbidden, blob, f"{forbidden!r} leaked into metrics")

    def test_the_endpoint_is_on_the_registry_and_takes_a_window(self):
        report = dunk_api.dispatch(self.flock, "metrics", {"window_s": 60},
                                   self_pane_id="w1:p2")
        self.assertEqual(60, report["interval"]["seconds"])
        self.assertEqual(1, report["schema_version"])
        command = next(c for c in dunk_api.COMMANDS if c.name == "metrics")
        self.assertTrue(command.mcp, "VP pulls it over MCP as well as the CLI")
        for prop in command.input_schema()["properties"].values():
            self.assertIn(prop["type"], ("string", "number", "integer", "boolean"))


if __name__ == "__main__":
    unittest.main()
