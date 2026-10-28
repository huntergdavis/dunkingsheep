"""Turn authentication reasons must preserve the fail-closed decision."""
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import _bootstrap  # noqa: F401
import dunk_callers as callers
import dunk_core


def proc(pid, ppid, start="a", state="S"):
    return {"pid": pid, "ppid": ppid, "start": start, "state": state}


class TurnAuthenticationReasonTests(unittest.TestCase):
    def test_each_refusal_names_its_failing_condition(self):
        caller = {"pid": 300, "start": "a", "source": "agent"}
        cases = [
            ("caller_missing", None, 200, {}),
            ("caller_start_missing", {"pid": 300}, 200, {}),
            ("pane_pid_missing", caller, None, {}),
            ("caller_pid_invalid", {"pid": "bad", "start": "a"}, 200, {}),
            ("pane_pid_invalid", caller, "bad", {}),
            ("caller_proc_missing", caller, 200, {}),
            ("caller_dead", caller, 200, {300: proc(300, 200, state="Z")}),
            ("caller_start_mismatch", caller, 200, {300: proc(300, 200, start="b")}),
            ("ancestry_proc_missing", caller, 200, {300: proc(300, 250)}),
            ("ancestry_dead", caller, 200,
             {300: proc(300, 250), 250: proc(250, 200, state="Z")}),
            ("pane_not_in_ancestry", caller, 200,
             {300: proc(300, 1), 1: proc(1, 0)}),
            ("ancestry_cycle", caller, 200,
             {300: proc(300, 250), 250: proc(250, 300)}),
            ("ancestry_depth_exceeded", caller, 200,
             {pid: proc(pid, pid + 1) for pid in range(300, 366)}),
        ]
        for reason, identity, root, rows in cases:
            with self.subTest(reason=reason), patch.object(callers, "_stat", side_effect=rows.get):
                self.assertEqual(reason, callers.caller_pane_auth_failure(identity, root))
                self.assertIs(False, callers.caller_belongs_to_pane(identity, root))

    def test_matching_identity_and_ancestry_still_authenticate(self):
        caller = {"pid": 300, "start": "a"}
        rows = {300: proc(300, 250), 250: proc(250, 200), 200: proc(200, 1)}
        for root in (300, 250, 200):
            with self.subTest(root=root), patch.object(callers, "_stat", side_effect=rows.get):
                self.assertIsNone(callers.caller_pane_auth_failure(caller, root))
                self.assertIs(True, callers.caller_belongs_to_pane(caller, root))

    def test_pid_one_is_not_an_authenticated_pane(self):
        with patch.object(callers, "_stat", return_value=proc(1, 0)):
            self.assertEqual("pane_not_in_ancestry",
                             callers.caller_pane_auth_failure({"pid": 1, "start": "a"}, 1))
            self.assertIs(False, callers.caller_belongs_to_pane({"pid": 1, "start": "a"}, 1))


class TurnAuthenticationLogTests(unittest.TestCase):
    def setUp(self):
        # Exercise the real guard without creating stores, workers or terminals.
        self.flock = dunk_core.Flock.__new__(dunk_core.Flock)
        self.flock._caller_state = SimpleNamespace(value={"pid": 300, "start": "a",
                                                        "source": "agent"})
        self.flock.herdr = SimpleNamespace(get_pane=Mock(return_value={"pane_pid": 200}))
        self.flock.turns = {"%35": {"turn_id": "keep-this-turn"}}

    def test_refusal_logs_once_and_leaves_turn_untouched(self):
        cases = [
            ("pane_pid_missing", {}, {}),
            ("caller_start_mismatch", {"pane_pid": 200}, {300: proc(300, 200, start="b")}),
            ("pane_not_in_ancestry", {"pane_pid": 200},
             {300: proc(300, 1), 1: proc(1, 0)}),
        ]
        for reason, pane, rows in cases:
            self.flock.herdr.get_pane.return_value = pane
            with self.subTest(reason=reason), patch.object(callers, "_stat", side_effect=rows.get):
                with self.assertLogs("dunkingsheep", level="WARNING") as logs:
                    with self.assertRaisesRegex(dunk_core.DunkError, "caller is not authenticated"):
                        self.flock.require_turn_caller("%35", "%35")
                self.assertEqual(1, len(logs.output))
                self.assertIn("reason=" + reason, logs.output[0])
                self.assertIn("caller_pid=300", logs.output[0])
                self.assertNotIn("keep-this-turn", logs.output[0])
                self.assertEqual({"%35": {"turn_id": "keep-this-turn"}}, self.flock.turns)

    def test_foreign_label_remains_refused_before_backend_lookup(self):
        with self.assertLogs("dunkingsheep", level="WARNING") as logs:
            with self.assertRaisesRegex(dunk_core.DunkError, "caller's own pane"):
                self.flock.require_turn_caller("%29", "%35")
        self.assertEqual(1, len(logs.output))
        self.assertIn("reason=target_mismatch", logs.output[0])
        self.flock.herdr.get_pane.assert_not_called()

    def test_authenticated_guard_does_not_log_a_refusal(self):
        rows = {300: proc(300, 200), 200: proc(200, 1)}
        with patch.object(callers, "_stat", side_effect=rows.get), patch.object(dunk_core.log, "warning") as warn:
            self.flock.require_turn_caller("%35", "%35")
        warn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
