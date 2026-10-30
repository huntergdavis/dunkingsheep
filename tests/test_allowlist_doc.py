"""t903: the allowlist doc must match the allowlist, and a satisfied dependency must say so.

Two drifts, both measured on 2026-10-30:

1. `docs/FLAT_FLEET.md` 4.3 gave the worker row as "task next/heartbeat/complete/note/
   block/release/follow" while a worker pane successfully ran `task unblock`. Lanes read
   that row to decide what they may do, so the row is a claim about the code and has to
   be checked like one.

2. A dependency reaching `done` did not reopen its dependent: t768 (T02) stayed
   `status='blocked'` with `blocked_by` EMPTY, so `task next` never offered it and
   nothing said why - one `task unblock` later another lane claimed it in eight
   seconds. The status is a human decision, so the store must not clear it, but the
   record must name the unblock as the only thing left (both edges: done, cancelled).
"""

import os
import re
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_roles  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def worker_row(path=None):
    """The `worker` row of FLAT_FLEET 4.3, as its 'May call' cell text."""
    text = open(path or os.path.join(ROOT, "docs", "FLAT_FLEET.md"),
                encoding="utf-8").read()
    match = re.search(r"^\| `worker` \| (.+?) \| (.+?) \|$", text, re.M)
    if not match:
        raise AssertionError("the worker row is not in FLAT_FLEET 4.3")
    return match.group(1), match.group(2)


def row_commands(cell):
    """Every `task` verb the cell names, normalised to task_<verb>.

    The doc writes them as `task next/claim/heartbeat/...` (one prefix, many verbs) and
    sometimes as `task_add`; both shapes have to survive, which is why the chunk is
    inspected BEFORE it is split.
    """
    names = set()

    def add(verb):
        verb = verb.strip("*/ .")
        if re.fullmatch(r"[a-z_]+", verb):
            names.add("task_" + verb)

    for chunk in re.findall(r"`([^`]+)`", cell):
        chunk = chunk.strip()
        if chunk.startswith("task "):
            for verb in chunk[len("task "):].split("/"):
                add(verb)
        elif chunk.startswith("task_"):
            names.add(chunk)
        else:
            for part in re.split(r"[/,]", chunk):
                part = part.strip()
                if part.startswith("task "):
                    for verb in part[len("task "):].split("/"):
                        add(verb)
                elif part.startswith("task_"):
                    names.add(part)
    return names


class AllowlistDocTests(unittest.TestCase):
    def test_every_task_verb_the_table_grants_is_named_in_the_worker_row(self):
        # The measured drift: 'task unblock' was granted and unlisted.
        granted = {cmd for cmd in dunk_roles.ALLOW["worker"] if cmd.startswith("task_")}
        named = row_commands(worker_row()[0])
        self.assertEqual(sorted(granted - named), [],
                         "FLAT_FLEET 4.3's worker row omits verbs the table grants")

    def test_the_worker_row_promises_nothing_the_table_denies(self):
        named = row_commands(worker_row()[0])
        denied = sorted(name for name in named
                        if name not in dunk_roles.ALLOW["worker"])
        self.assertEqual(denied, [],
                         "FLAT_FLEET 4.3 promises verbs a worker may not call")

    def test_the_row_still_states_the_two_denials_that_matter(self):
        # Dropping these would be a doc regression in the other direction.
        granted, refused = worker_row()
        self.assertIn("task cancel", refused)
        self.assertIn("task assign", refused)
        self.assertNotIn("task cancel", row_commands(granted))

    def test_a_renamed_verb_in_the_doc_fails_the_test(self):
        # Guard against the test passing because it stopped finding anything.
        self.assertTrue(row_commands(worker_row()[0]))


class SatisfiedDependencyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.tasks = Tasks(path=os.path.join(self.tmp.name, "tasks.db"))

    def tearDown(self):
        self.tasks.close()
        self.tmp.cleanup()

    def _pair(self, block_it=True):
        dep = self.tasks.add("global", "the dependency")
        child = self.tasks.add("global", "the dependent", depends_on=[dep["id"]])
        if block_it:
            self.tasks.update(child["id"], status="blocked", block_reason="waiting")
        return dep, child

    def test_a_done_dependency_names_the_unblock_as_the_only_thing_left(self):
        # t768's shape: blocked, dependency done, blocked_by empty, offered by nothing.
        dep, child = self._pair()
        self.tasks.update(dep["id"], status="done")
        row = self.tasks.get(child["id"])
        self.assertEqual([], row["blocked_by"])
        self.assertIs(True, row["unblock_needed"])
        self.assertIn("every dependency is satisfied", row["dependency_note"])
        self.assertIn("task unblock", row["dependency_note"])

    def test_a_cancelled_dependency_says_the_same_and_not_the_old_warning(self):
        dep, child = self._pair()
        self.tasks.cancel(dep["id"])
        row = self.tasks.get(child["id"])
        self.assertEqual([], row["blocked_by"],
                         "t783 made a cancelled dependency satisfied")
        self.assertIs(True, row["unblock_needed"])
        self.assertIn("CANCELLED", row["dependency_note"])

    def test_an_outstanding_dependency_is_not_reported_as_unblockable(self):
        _dep, child = self._pair()
        row = self.tasks.get(child["id"])
        self.assertEqual(1, len(row["blocked_by"]))
        self.assertIs(False, row["unblock_needed"])
        self.assertIsNone(row["dependency_note"])

    def test_a_blocked_task_with_no_dependencies_is_not_this_case(self):
        task = self.tasks.add("global", "blocked by a decision")
        self.tasks.update(task["id"], status="blocked", block_reason="needs-owner: x")
        row = self.tasks.get(task["id"])
        self.assertIs(False, row["unblock_needed"])

    def test_the_store_does_not_clear_the_block_by_itself(self):
        # Only a person or a lane sets 'blocked', so the store must not undo it - it
        # reports the unblock instead (the decision this task's acceptance allows).
        dep, child = self._pair()
        self.tasks.update(dep["id"], status="done")
        self.assertEqual("blocked", self.tasks.get(child["id"])["status"])


if __name__ == "__main__":
    unittest.main()
