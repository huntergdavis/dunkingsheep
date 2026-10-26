"""t227: the dispatcher feeds NOBODY unless a lane is named.

Before t227 the dispatch tick's candidate list was empty only because every pane read
"unknown". Once a turn signal makes lanes eligible, an empty-by-default allowlist is what keeps a
merge or an upgrade from silently auto-nudging reviewer, CoS and VP lanes with board work
(reviewer pi-review-1 + VP on #38).
"""

import json
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402
import dunk_dispatch  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class AllowlistTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:          # tmux-shaped: the backend cannot answer status
            pane["agent_status"] = "unknown"
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        self.tmp.close()
        os.unlink(self.tmp.name)
        self._saved = (dunk_api.DISPATCH_LANES_PATH,
                       os.environ.pop("DUNKINGSHEEP_DISPATCH_LANES", None))
        dunk_api.DISPATCH_LANES_PATH = self.tmp.name

    def tearDown(self):
        dunk_api.DISPATCH_LANES_PATH, env = self._saved
        if env is not None:
            os.environ["DUNKINGSHEEP_DISPATCH_LANES"] = env
        try:
            os.unlink(self.tmp.name)
        except FileNotFoundError:
            pass
        self.flock.close()

    def _write(self, lanes):
        with open(self.tmp.name, "w") as handle:
            json.dump({"lanes": lanes}, handle)

    def _tick(self, **args):
        panes = [pane for pane in self.flock.resolve_lanes()
                 if pane.get("agent_status") not in (None, "unknown")
                 and pane["pane_id"] in dunk_api._dispatch_allowlist(self.flock, args)]
        queue = dunk_dispatch.Queue()
        return dunk_dispatch.tick(
            queue, panes, dry_run=True,
            delivery=lambda pane: {"state": "ready", "reason": None},
            unread=lambda pane: 0,
            send=lambda pane_id, text: None,
            lanes=dunk_api._dispatch_allowlist(self.flock, args),
            derived=[{"id": "board:t1", "task_id": "t1", "brief": "board work",
                      "derived": True, "lane": None}])

    def _working_lane(self):
        pane_id = self.herdr.panes[0]["pane_id"]
        self.flock.turn_start(pane_id, source="test")
        return pane_id

    def test_the_default_allowlist_is_empty_and_feeds_nothing(self):
        self.assertEqual(set(), dunk_api._dispatch_allowlist(self.flock, {}))
        state = self._tick()
        self.assertEqual([], state["dispatched"],
                         "a ready lane must NOT be fed without being named")
        self.assertEqual(0, state["lanes"])

    def test_an_allowlisted_ready_lane_is_fed(self):
        pane_id = self._working_lane()
        self._write([pane_id])
        state = self._tick()
        self.assertEqual([pane_id], [record["pane_id"] for record in state["dispatched"]])

    def test_an_allowlisted_lane_that_is_working_is_not_fed(self):
        pane_id = self._working_lane()
        self._write([pane_id])
        panes = [pane for pane in self.flock.resolve_lanes() if pane["pane_id"] == pane_id]
        queue = dunk_dispatch.Queue()
        state = dunk_dispatch.tick(
            queue, panes, dry_run=True,
            delivery=lambda pane: {"state": "working", "reason": "declared turn"},
            unread=lambda pane: 0, send=lambda pane_id, text: None,
            lanes={pane_id}, derived=[{"id": "board:t1", "task_id": "t1", "brief": "board work",
                                       "derived": True, "lane": None}])
        self.assertEqual([], state["dispatched"])
        self.assertEqual(1, state["working"])

    def test_a_name_resolves_to_a_pane_id(self):
        pane_id = self._working_lane()
        lane = next(p for p in self.flock.resolve_lanes() if p["pane_id"] == pane_id)
        self._write([lane["agent_name"]])
        self.assertIn(pane_id, dunk_api._dispatch_allowlist(self.flock, {}))

    def test_an_id_entry_resolves_to_itself(self):
        pane_id = self._working_lane()
        self._write([pane_id])
        self.assertEqual({pane_id}, dunk_api._dispatch_allowlist(self.flock, {}))

    def test_the_allowlist_does_not_shadow_the_pane_listing(self):
        """Regression: a local named `lanes` inside the resolver shadowed the parameter that
        carries the pane listing, so name resolution iterated config strings, raised, and was
        swallowed - every name silently passed through unresolved."""
        pane_id = self._working_lane()
        lane = next(p for p in self.flock.resolve_lanes() if p["pane_id"] == pane_id)
        self._write([lane["agent_name"]])
        self.assertIn(pane_id, dunk_api._dispatch_allowlist(self.flock, {}))

    def test_a_failed_pane_listing_says_the_names_could_not_be_resolved(self):
        """Reviewer: a names-only allowlist must not degrade to literals SILENTLY when the pane
        listing fails - ids keep working, names match nothing, and the operator is told why."""
        import contextlib
        import io

        class BrokenLanes:
            def resolve_lanes(self):
                raise RuntimeError("cannot list panes: 'tab_id'")

        self._write(["codex-worker,%4"])
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            resolved = dunk_api._dispatch_allowlist(BrokenLanes(), {})
        self.assertIn("%4", resolved, "an id must still work")
        self.assertIn("codex-worker", resolved)
        message = err.getvalue()
        self.assertIn("cannot resolve lane names", message)
        self.assertIn("codex-worker", message)

    def test_a_working_listing_prints_nothing_about_names(self):
        import contextlib
        import io
        pane_id = self._working_lane()
        self._write([pane_id])
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            dunk_api._dispatch_allowlist(self.flock, {})
        self.assertEqual("", err.getvalue())

    def test_a_wrong_shaped_config_says_it_is_feeding_nothing(self):
        """Reviewer note 1: {"lane": [...]} is a typo for "lanes" - a silent disable is the
        failure this allowlist exists to prevent."""
        import contextlib
        import io
        with open(self.tmp.name, "w") as handle:
            json.dump({"lane": ["%4"]}, handle)
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(set(), dunk_api._dispatch_allowlist(self.flock, {}))
        self.assertIn("no lanes list", err.getvalue())

    def test_an_absent_config_is_silent(self):
        """Absent is normal, not an error: nothing is fed and nothing is printed."""
        import contextlib
        import io
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(set(), dunk_api._dispatch_allowlist(self.flock, {}))
        self.assertEqual("", err.getvalue())

    def test_an_unreadable_config_feeds_nothing(self):
        with open(self.tmp.name, "w") as handle:
            handle.write("{not json")
        self.assertEqual(set(), dunk_api._dispatch_allowlist(self.flock, {}))


if __name__ == "__main__":
    unittest.main()
