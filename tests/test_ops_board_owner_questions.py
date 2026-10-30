"""t997 / DESK_DESIGN 4.1: the desk's needs-owner block.

The desk asks the owner to answer a lane's question, so the board must carry the question
VERBATIM, what the answer unblocks, and the owner's own recent asks with status chips -
all from the task rows, never a summary the board invented.
"""

import os
import sys
import unittest
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "bridges"))

import _bootstrap  # noqa: F401
import ops_board  # noqa: E402

NOW = datetime(2026, 10, 30, 12, 0, 0, tzinfo=timezone.utc)


class FakeClient:
    def __init__(self, tasks):
        self.tasks = tasks

    def call(self, name, **args):
        if name != "task_list":
            raise AssertionError(name)
        return {"tasks": self.tasks}


def task(task_id, **kwargs):
    row = {"id": task_id, "title": f"task {task_id}", "status": "open",
           "priority": "medium", "scope": "global", "assignees": [], "depends_on": [],
           "created_at": NOW.timestamp() - 60, "updated_at": NOW.timestamp() - 60,
           "created_by": "review-sync"}
    row.update(kwargs)
    return row


class OwnerQuestionTests(unittest.TestCase):
    def test_the_question_is_verbatim_and_the_prefix_is_stripped(self):
        self.assertEqual("do you want A or B?",
                         ops_board._owner_question("needs-owner: do you want A or B?"))

    def test_a_reason_without_the_prefix_is_not_a_question(self):
        self.assertIsNone(ops_board._owner_question("waiting on a vendor"))
        self.assertIsNone(ops_board._owner_question("needs-owner:"))
        self.assertIsNone(ops_board._owner_question(None))

    def test_the_board_carries_the_question_and_what_it_blocks(self):
        tasks = [task("t1", block_reason="needs-owner: switch the desk over?"),
                 task("t2", depends_on=["t1"]),
                 task("t3", depends_on=["t1"])]
        board = ops_board._task_board(FakeClient(tasks), NOW)
        rows = {row["id"]: row for row in board["items"]}
        self.assertEqual("switch the desk over?", rows["t1"]["needs_owner_question"])
        self.assertEqual(["t2", "t3"], sorted(rows["t1"]["blocks"]),
                         "the desk must say which tasks an answer unblocks")
        self.assertIsNone(rows["t2"]["needs_owner_question"])

    def test_asks_are_the_owners_own_from_the_last_24h_with_status(self):
        old = NOW.timestamp() - 30 * 3600
        tasks = [task("t1", created_by="owner", status="in_progress"),
                 task("t2", created_by="owner@telegram", status="open"),
                 task("t3", created_by="owner", created_at=old),
                 task("t4", created_by="review-sync"),
                 task("t5", created_by="%95")]
        board = ops_board._asks_board(FakeClient(tasks), NOW)
        # the OWNER MAILBOX is `owner` (with or without a transport suffix); a task filed
        # by the owner's PANE is not "from the owner mailbox", which is the provenance the
        # task asks for
        self.assertEqual(["t1", "t2"], [row["id"] for row in board["asks"]])
        self.assertEqual(["in_progress", "open"],
                         [row["status"] for row in board["asks"]],
                         "the chips come from the row's status")
        self.assertEqual(24, board["window_hours"])

    def test_a_missing_source_is_reported_not_invented(self):
        class Broken:
            def call(self, *_args, **_kwargs):
                raise RuntimeError("daemon busy")

        board = ops_board._asks_board(Broken(), NOW)
        self.assertIn("error", board, "an unreadable source must say so, never read as zero")


if __name__ == "__main__":
    unittest.main()
