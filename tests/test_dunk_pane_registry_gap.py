"""t714: a live lane the pane list drops must be visible, not silently missing.

Measured 2026-10-29 22:15 (steward): roles.json listed %103, %108 and %109 as lanes
with running pids while `dunkingsheep panes` carried none of them, and a send then
read 'dead_lane'. By 23:30 the same command listed %108/%109 again, so the gap was
intermittent and was NOT reproduced by the lane that filed this. The deliverable is
therefore visibility: the pane listing and the health check name the disagreement
(registry says the pane exists, the backend does not list it) with the process
liveness, instead of letting a dropped row read as a lane that died.
"""

import importlib.util
import os
import sys
import tempfile
import unittest
from importlib.machinery import SourceFileLoader
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_api  # noqa: E402
import dunk_core  # noqa: E402
import dunk_health  # noqa: E402
import dunk_roles  # noqa: E402
from dunk_callers import process_start  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load_cli():
    loader = SourceFileLoader("ds_cli_gap", os.path.join(ROOT, "dunkingsheep"))
    spec = importlib.util.spec_from_loader("ds_cli_gap", loader)
    cli = importlib.util.module_from_spec(spec)
    loader.exec_module(cli)
    return cli


class RegistryPaneGapTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.roles = dunk_roles.Roles(path=os.path.join(self.tmp.name, "roles.json"),
                                      default="anonymous")
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                     roles=self.roles, inbox=self.inbox)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def _lose(self, pane_id):
        self.herdr.panes = [p for p in self.herdr.panes if p["pane_id"] != pane_id]

    def _register_alive(self, pane_id, label="ds-worker-x", role="worker"):
        self.roles.register(pane_id, role, pane_pid=os.getpid(),
                            start=process_start(os.getpid()), label=label, by="test")

    def _register_gone(self, pane_id, label="ds-worker-dead"):
        self.roles.register(pane_id, "worker", pane_pid=999999, start="12345",
                            label=label, by="test")

    # -- the comparison itself ------------------------------------------------

    def test_a_live_registered_lane_missing_from_the_list_is_reported_alive(self):
        self._lose("w2:p1")
        self._register_alive("w2:p1")
        gap = self.flock.registry_pane_gap()
        self.assertEqual(["w2:p1"], [row["pane"] for row in gap])
        self.assertIs(True, gap[0]["alive"])
        self.assertEqual("ds-worker-x", gap[0]["label"])
        self.assertEqual(os.getpid(), gap[0]["pid"])

    def test_a_registered_lane_whose_process_is_gone_is_reported_not_alive(self):
        self._lose("w2:p1")
        self._register_gone("w2:p1")
        gap = self.flock.registry_pane_gap()
        self.assertEqual(["w2:p1"], [row["pane"] for row in gap])
        self.assertIs(False, gap[0]["alive"])

    def test_a_registered_lane_the_backend_lists_is_not_a_gap(self):
        self._register_alive("w2:p1")
        self.assertEqual([], self.flock.registry_pane_gap())

    def test_a_reused_pid_is_not_alive(self):
        # Same pid, different starttime: a %N reused after a tmux restart.
        self._lose("w2:p1")
        self.roles.register("w2:p1", "worker", pane_pid=os.getpid(), start="1000",
                            label="reused", by="test")
        self.assertIs(False, self.flock.registry_pane_gap()[0]["alive"])

    def test_no_pane_list_means_no_gap_claim(self):
        # An unreachable backend is a different problem, not a disagreement with the
        # registry: claiming a gap from a failed read would be the same false zero.
        def boom():
            raise RuntimeError("backend down")
        self.herdr.list_panes = boom
        self._register_alive("w2:p1")
        self.assertEqual([], self.flock.registry_pane_gap())

    # -- the two surfaces ----------------------------------------------------

    def test_the_pane_listing_carries_the_gap_beside_the_panes(self):
        self._lose("w2:p1")
        self._register_alive("w2:p1")
        result = dunk_api.h_list_panes(self.flock, {}, None)
        self.assertIn("w2:p1", [row["pane"] for row in result["registry_gap"]])
        self.assertNotIn("w2:p1", [p["pane_id"] for p in result["panes"]],
                         "the pane is still absent from the list; the gap is the note")

    def test_no_gap_means_no_key_at_all(self):
        # Contract: a key is present only when there is something to say.
        result = dunk_api.h_list_panes(self.flock, {}, None)
        self.assertNotIn("registry_gap", result)

    def test_health_names_an_alive_gap_as_a_problem(self):
        self._lose("w2:p1")
        self._register_alive("w2:p1")
        result = dunk_health.run_health(self.flock)
        self.assertIs(False, result["ok"])
        joined = " ".join(result["problems"])
        self.assertIn("missing 1 registered lane", joined)
        self.assertIn("w2:p1", joined)
        self.assertIn("ds-worker-x", joined)
        self.assertEqual(["w2:p1"],
                         [row["pane"] for row in result["checks"]["registry_pane_gap"]])

    def test_health_does_not_alarm_on_a_stale_registration(self):
        self._lose("w2:p1")
        self._register_gone("w2:p1")
        result = dunk_health.run_health(self.flock)
        joined = " ".join(result["problems"])
        self.assertNotIn("missing", joined)
        self.assertEqual(["w2:p1"],
                         [row["pane"] for row in result["checks"]["registry_pane_gap"]],
                         "a dead registration is still visible in the checks")

    def test_the_cli_prints_the_gap_line(self):
        self._lose("w2:p1")
        self._register_alive("w2:p1")
        cli = load_cli()
        result = dunk_api.h_list_panes(self.flock, {}, None)
        printed = []

        def capture(*args, **kwargs):
            printed.append(" ".join(str(a) for a in args))
        with mock.patch("builtins.print", side_effect=capture):
            cli.print_panes(result)
        text = "\n".join(printed)
        self.assertIn("registered lane(s) the backend pane list does not contain", text)
        self.assertIn("w2:p1", text)
        self.assertIn("ALIVE", text)
        self.assertIn("backend_missing_pane", text)


if __name__ == "__main__":
    unittest.main()
