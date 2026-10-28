"""GET /api/terminals must be true about what it can and cannot see.

t62 asks for [{name, agent, state, cols, rows}] over the tmux backend. The two things
that make that list WRONG rather than merely empty are (1) a label that two panes
answer to - 'opencode-ds41-b' is recorded on two panes on this host, so a name that is
not made unique is not an address - and (2) a state the backend cannot know, which must
come back null WITH a reason instead of "unknown" dressed up as a value.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: F401,E402 - suite isolation must precede every dunk_* import
import dunk_http  # noqa: E402
import dunk_terminal  # noqa: E402


def pane(pane_id, label, agent="pi", cols=80, rows=24, workspace="bootstrap"):
    return {"pane_id": pane_id, "label": label, "agent": agent,
            "cols": cols, "rows": rows, "workspace_label": workspace,
            "terminal_title_stripped": label}


class FakeBackend:
    def __init__(self, panes):
        self.panes = panes

    def list_panes_grouped(self):
        return self.panes


class TerminalsRoute(unittest.TestCase):
    def setUp(self):
        self._real_backend = dunk_terminal.get_backend
        self._real_call = dunk_http._call

    def tearDown(self):
        dunk_terminal.get_backend = self._real_backend
        dunk_http._call = self._real_call

    def _install(self, panes, daemon_panes=None, daemon_error=None):
        dunk_terminal.get_backend = lambda herdr=None: FakeBackend(panes)

        def fake_call(command, **args):
            self.assertEqual("list_panes", command)
            if daemon_error is not None:
                raise daemon_error
            return {"panes": daemon_panes or []}

        dunk_http._call = fake_call

    def test_contract_fields_come_from_the_pane_and_the_daemon_status(self):
        self._install(
            [pane("%1", "cos", agent="pi", cols=100, rows=30)],
            daemon_panes=[{"pane_id": "%1", "agent_status": "working"}],
        )
        result = dunk_http.route("/api/terminals", {})
        self.assertIsInstance(result, list)
        self.assertEqual(1, len(result))
        item = result[0]
        for key in ("name", "agent", "state", "cols", "rows"):
            self.assertIn(key, item)
        self.assertEqual("cos", item["name"])
        self.assertEqual("pi", item["agent"])
        self.assertEqual("working", item["state"])
        self.assertEqual(100, item["cols"])
        self.assertEqual(30, item["rows"])
        self.assertNotIn("state_reason", item)

    def test_duplicate_label_gets_a_stable_unique_suffix(self):
        """'opencode-ds41-b' really is on two panes; both must stay addressable."""
        self._install([pane("%28", "opencode-ds41-b"), pane("%41", "opencode-ds41-b")])
        names = [item["name"] for item in dunk_http.route("/api/terminals", {})]
        self.assertEqual(["opencode-ds41-b", "opencode-ds41-b-2"], names)
        self.assertEqual(len(names), len(set(names)))
        # Same input, same names: the suffix is not random.
        again = [item["name"] for item in dunk_http.route("/api/terminals", {})]
        self.assertEqual(names, again)

    def test_suffix_cannot_collide_with_a_real_label(self):
        """A pane literally named 'x-2' must not be overwritten by the second 'x'."""
        self._install([pane("%1", "x-2"), pane("%2", "x"), pane("%3", "x")])
        names = [item["name"] for item in dunk_http.route("/api/terminals", {})]
        self.assertEqual(len(names), len(set(names)))
        self.assertIn("x-2", names)
        self.assertEqual(3, len(names))

    def test_unknown_state_is_null_with_a_reason_and_a_dead_daemon_does_not_empty_the_list(self):
        self._install([pane("%1", "cos")], daemon_error=RuntimeError("socket closed"))
        item = dunk_http.route("/api/terminals", {})[0]
        self.assertEqual("cos", item["name"])
        self.assertIsNone(item["state"])
        self.assertIn("socket closed", item["state_reason"])

    def test_unnamed_shell_pane_reports_no_agent_rather_than_an_empty_string(self):
        self._install([pane("%7", "bash", agent="")],
                      daemon_panes=[{"pane_id": "%7", "agent_status": None,
                                     "agent_status_reason": "no turn recorded"}])
        item = dunk_http.route("/api/terminals", {})[0]
        self.assertIsNone(item["agent"])
        self.assertIsNone(item["state"])
        self.assertEqual("no turn recorded", item["state_reason"])


class BackendGeometry(unittest.TestCase):
    def _backend(self):
        backend = dunk_terminal.TmuxBackend.__new__(dunk_terminal.TmuxBackend)
        backend.socket_name = None
        backend._load_registry = lambda: {"panes": {}}
        return backend

    def _line(self, *extra):
        return self._backend().PANE_SEP.join(
            ["%2", "$2", "@2", "/tmp", "pi", "lane", "1", "cos", "pi", "1", "", "4242"]
            + list(extra))

    def test_format_asks_tmux_for_the_cell_grid(self):
        self.assertIn("#{pane_width}", self._backend().PANE_FORMAT)
        self.assertIn("#{pane_height}", self._backend().PANE_FORMAT)

    def test_geometry_is_parsed_from_the_appended_fields(self):
        parsed = self._backend()._parse_panes(self._line("132", "43"))[0]
        self.assertEqual(132, parsed["cols"])
        self.assertEqual(43, parsed["rows"])

    def test_a_twelve_field_line_still_parses_with_geometry_absent(self):
        """Old captures and fixtures predate the geometry fields; they must not lie."""
        parsed = self._backend()._parse_panes(self._line())[0]
        self.assertEqual(4242, parsed["pane_pid"])
        self.assertIsNone(parsed["cols"])
        self.assertIsNone(parsed["rows"])


if __name__ == "__main__":
    unittest.main()
