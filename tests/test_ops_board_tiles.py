"""t996: the operations board's header strip must be MEASURED, not decorative.

The live board still showed the VP-era tiles: `brief.tasks.leader_held` read 56 "held
by the leader" (that was just the unassigned queue - there is no leader in a flat
fleet) and the owner list was empty. These tests pin the replacement: four tiles, each
with a SOURCE and the time it was read, and no number that cannot be traced to a store.
"""

import datetime
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "bridges"))

import _bootstrap  # noqa: F401
import ops_board  # noqa: E402

NOW = datetime.datetime(2026, 10, 30, 15, 0, 0, tzinfo=datetime.timezone.utc)


class FakeClient:
    """A daemon stand-in: canned replies keyed by command name."""

    def __init__(self, replies):
        self.replies = replies
        self.calls = []

    def call(self, name, **args):
        self.calls.append((name, args))
        if name not in self.replies:
            raise RuntimeError(f"no such command {name}")
        reply = self.replies[name]
        if isinstance(reply, Exception):
            raise reply
        return reply


def task(task_id, status, priority, age_s, **extra):
    created = (NOW - datetime.timedelta(seconds=age_s)).timestamp()
    row = {"id": task_id, "title": task_id, "status": status, "priority": priority,
           "scope": "global", "assignees": [], "created_at": created,
           "updated_at": created, "needs_owner": False}
    row.update(extra)
    return row


def board(replies, **kwargs):
    """`conn` is the private OPERATIONS database: an in-memory sqlite is enough here
    because these tests pin the four tiles, not the host samples."""
    import sqlite3
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    try:
        return ops_board.build(FakeClient(replies), conn, now=NOW, **kwargs)
    finally:
        conn.close()


class BriefTileTests(unittest.TestCase):
    def setUp(self):
        self.replies = {
            "task_list": {"tasks": [
                task("t1", "open", "urgent", 3600),
                task("t2", "open", "high", 60),
                task("t3", "blocked", "high", 7200),
                task("t4", "done", "low", 30),
            ]},
            "task_metrics": {"throughput": {"completed": 17, "buckets": [
                # the daemon's real shape: {bucket: unix second, completed: N}
                {"bucket": NOW.timestamp() - 1800, "completed": 4},    # this hour
                {"bucket": NOW.timestamp() - 5400, "completed": 7},    # the hour before
                {"bucket": NOW.timestamp() - 40000, "completed": 6},   # older, same day
            ]}},
            "review_list": {"reviews": [
                {"task": "r1", "repo": "o/r", "pr": 1, "head": "abc", "age_s": 120,
                 "status": "open", "verdict": None},
                {"task": "r2", "repo": "o/r", "pr": 2, "head": "def", "age_s": 900,
                 "status": "open", "verdict": None},
            ]},
            "list_panes": {"panes": [
                {"pane_id": "%1", "agent": "", "agent_status": "working"},
                {"pane_id": "%2", "agent": "", "agent_status": "idle"},
                {"pane_id": "%3", "agent": "", "agent_status": None},
            ], "registry_gap": [
                {"pane": "%9", "role": "reviewer", "alive": False, "label": "dead-lane"},
                {"pane": "%8", "role": "worker", "alive": True, "label": "unreachable"},
            ]},
            "list_dunks": {"dunks": []},
            "health": {"ok": True, "problems": []},
        }

    def test_the_four_tiles_are_measured_and_named(self):
        brief = board(self.replies)["brief"]
        self.assertEqual({"throughput", "backlog", "review", "lanes"}, set(brief))
        for name in ("throughput", "backlog", "review", "lanes"):
            tile = brief[name]
            self.assertIn("source", tile, f"{name} has no source")
            self.assertIn("generated_at", tile, f"{name} has no timestamp")

    def test_the_vp_era_numbers_are_gone(self):
        blob = json.dumps(board(self.replies))
        for dead in ("leader_held", "leader-held", "leader's", "\"leader\""):
            self.assertNotIn(dead, blob, f"{dead} survived into the board")

    def test_backlog_counts_by_priority_and_reports_the_oldest(self):
        brief = board(self.replies)["brief"]["backlog"]
        self.assertEqual(3, brief["open"], "open+blocked only: a done task is not backlog")
        self.assertEqual({"urgent": 1, "high": 1}, brief["by_priority"]["open"])
        self.assertEqual({"high": 1}, brief["by_priority"]["blocked"])
        self.assertEqual(7200, brief["oldest_age_s"])
        self.assertEqual(0, brief["ages_unknown"])

    def test_an_unknown_age_is_not_a_zero_age(self):
        replies = dict(self.replies, task_list={"tasks": [
            {"id": "t9", "title": "no timestamp", "status": "open", "priority": "high",
             "scope": "global", "assignees": [], "created_at": None,
             "updated_at": None, "needs_owner": False}]})
        brief = board(replies)["brief"]["backlog"]
        self.assertEqual(1, brief["open"])
        self.assertIsNone(brief["oldest_age_s"], "an unknown age must not read as zero")
        self.assertEqual(1, brief["ages_unknown"])

    def test_throughput_comes_from_the_daemons_buckets(self):
        # Trailing windows over buckets, NOT the newest bucket alone: a bucket is an
        # hour and the current one is usually partial (the first live read showed 14
        # completed in an incomplete hour, which would have read as "the last hour").
        tile = board(self.replies)["brief"]["throughput"]
        # The trailing 3600 s window holds ONLY the bucket 1800 s old: the 5400 s one
        # is outside it. (My first expectation here was 11 - the bucket arithmetic I
        # did in my head, not the window the code takes. The code was right.)
        self.assertEqual(4, tile["completed_current_bucket"])
        self.assertEqual(17, tile["completed_last_24h"])
        self.assertEqual(17, tile["completed_in_window"])

    def test_hourly_buckets_report_the_current_bucket_not_a_trailing_hour(self):
        hour = NOW.timestamp() - NOW.timestamp() % 3600
        metrics = {"throughput": {"completed": 19, "buckets": [
            {"bucket": hour - 3600, "completed": 7}, {"bucket": hour, "completed": 12}]}}
        from bridges import ops_board
        tile = ops_board._throughput(metrics, datetime.datetime.fromtimestamp(hour + 1200, datetime.timezone.utc))
        self.assertEqual(12, tile["completed_current_bucket"])
        self.assertEqual(hour, tile["bucket_started_at"])
        gone = ops_board._throughput(metrics, datetime.datetime.fromtimestamp(hour + 3700, datetime.timezone.utc))
        self.assertEqual(0, gone["completed_current_bucket"], "a stale bucket is not this hour")

    def test_throughput_without_buckets_says_so_instead_of_zero(self):
        replies = dict(self.replies, task_metrics={"throughput": {"completed": 0}})
        tile = board(replies)["brief"]["throughput"]
        self.assertIsNone(tile["completed_current_bucket"],
                          "no buckets is 'not measured', not 'zero completed'")
        self.assertIn("no buckets", tile["note"],
                      "the tile must SAY why it is null, not just be null")

    def test_a_failing_metric_read_does_not_take_the_backlog_with_it(self):
        replies = dict(self.replies, task_metrics=RuntimeError("daemon busy"))
        brief = board(replies)["brief"]
        self.assertIsNone(brief["throughput"]["completed_current_bucket"])
        self.assertIsNone(brief["throughput"]["completed_last_24h"])
        self.assertEqual(3, brief["backlog"]["open"],
                         "the backlog is still measured when throughput is not")

    def test_the_review_tile_uses_the_daemons_own_age(self):
        tile = board(self.replies)["brief"]["review"]
        self.assertEqual(2, tile["awaiting"])
        self.assertEqual(900, tile["oldest_age_s"])
        self.assertEqual(0, tile["ages_unknown"])

    def test_a_review_without_an_age_is_counted_as_unknown(self):
        replies = dict(self.replies, review_list={"reviews": [
            {"task": "r1", "repo": "o/r", "pr": 1, "head": "abc", "status": "open"}]})
        tile = board(replies)["brief"]["review"]
        self.assertEqual(1, tile["awaiting"])
        self.assertIsNone(tile["oldest_age_s"])
        self.assertEqual(1, tile["ages_unknown"])

    def test_lanes_separate_working_idle_dead_and_unknown(self):
        tile = board(self.replies)["brief"]["lanes"]
        self.assertEqual(1, tile["working"])
        self.assertEqual(1, tile["idle"])
        self.assertEqual(1, tile["dead"],
                         "a registered lane whose pane is gone is DEAD, not unknown")
        # the live-but-unlisted lane only: a bare shell tab (%3) is not an unaccounted lane
        self.assertEqual(1, tile["unknown"])
        self.assertEqual(1, tile["bare_panes"])
        self.assertEqual("%9", tile["dead_items"][0]["pane_id"])

    def test_a_live_but_unlisted_lane_is_not_called_dead(self):
        replies = dict(self.replies, list_panes={
            "panes": [{"pane_id": "%1", "agent_status": "working"}],
            "registry_gap": [{"pane": "%8", "role": "worker", "alive": True}]})
        tile = board(replies)["brief"]["lanes"]
        self.assertEqual(0, tile["dead"])
        self.assertEqual(1, tile["unknown"])

    def test_activity_comes_from_agent_status_not_the_agent_field(self):
        # The live host has `agent` empty for every pi lane; counting on that field
        # reported "0 working" while twenty panes worked. The TILE carries the counts;
        # the full pane list (including `total`) stays in the reference section.
        result = board(self.replies)
        tile = result["brief"]["lanes"]
        self.assertEqual(1, tile["working"])
        reference = result["reference"]["lanes"]
        self.assertEqual(3, reference["total"],
                         "every pane is counted, agent field or not")


if __name__ == "__main__":
    unittest.main()
