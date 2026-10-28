"""POST /api/terminals/<name>/resize: the one write, and what it may claim.

The two failures that would make this route worse than absent:

  * echoing the REQUEST as if it were the size - tmux has no absolute per-pane size,
    so the answer must be the geometry read back after `resize-window`, or the caller
    believes a size that never happened;
  * refusing (or silently succeeding) when a client is attached. Measured on
    2026-10-28: `resize-window` succeeds with an 80x24 client attached and the window
    became 120x40, but with window-size=latest that client's NEXT resize event wins -
    so the honest report is `clients_attached`, not a refusal and not a silent success.
"""
import json
import os
import shutil
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: F401,E402 - suite isolation must precede every dunk_* import
import dunk_http  # noqa: E402
import dunk_terminal  # noqa: E402


def pane(pane_id, label, cols=80, rows=24, session="$1"):
    return {"pane_id": pane_id, "label": label, "agent": "pi", "cols": cols,
            "rows": rows, "workspace_id": session, "workspace_label": "bootstrap"}


class FakeBackend:
    name = "fake"

    def __init__(self, panes, capabilities=("resize",), result=(True, {})):
        self.panes = panes
        self._capabilities = frozenset(capabilities)
        self.result = result
        self.calls = []

    def capabilities(self):
        return self._capabilities

    def list_panes_grouped(self):
        return self.panes

    def resize(self, pane_id, cols, rows):
        self.calls.append((pane_id, cols, rows))
        return self.result


class ResizeRoute(unittest.TestCase):
    def setUp(self):
        self._real = dunk_terminal.get_backend

    def tearDown(self):
        dunk_terminal.get_backend = self._real

    def _install(self, backend):
        dunk_terminal.get_backend = lambda herdr=None: backend
        return backend

    def test_success_reports_the_measured_size_not_the_request(self):
        backend = self._install(FakeBackend(
            [pane("%28", "cos")],
            result=(True, {"pane_id": "%28", "requested": {"cols": 100, "rows": 30},
                           "cols": 99, "rows": 29, "clients_attached": 0,
                           "overridable_by_client": False,
                           "window_pane_count": 2,
                           "resized_panes": [{"pane_id": "%28", "cols": 99, "rows": 29,
                                              "cols_before": 80, "rows_before": 24}]})))
        out = dunk_http.route_post("/api/terminals/cos/resize", {"cols": 100, "rows": 30})
        self.assertEqual("%28", out["pane_id"])
        self.assertEqual(99, out["cols"], "must report tmux's size, not the request")
        self.assertEqual(29, out["rows"])
        self.assertIn("as_of", out)
        self.assertEqual(2, out["window_pane_count"])
        self.assertEqual("%28", out["resized_panes"][0]["pane_id"])
        self.assertEqual([("%28", 100, 30)], backend.calls)

    def test_a_collision_name_addresses_the_pane_the_get_list_named(self):
        """'x-2' is the suffix GET gave the second pane called 'x'; POST must hit it."""
        backend = self._install(FakeBackend(
            [pane("%1", "x"), pane("%2", "x"), pane("%3", "x-2")],
            result=(True, {"pane_id": "%2", "cols": 90, "rows": 30,
                           "clients_attached": 0, "overridable_by_client": False})))
        # Names are: x, x-2 (collision suffix), x-2-2 (the literal 'x-2' pane).
        self.assertEqual(["x", "x-2", "x-2-2"],
                         [n for n, _ in dunk_http._unique_names(backend.panes)])
        dunk_http.route_post("/api/terminals/x-2/resize", {"cols": 90, "rows": 30})
        self.assertEqual("%2", backend.calls[0][0])

    def test_unknown_name_is_404_and_nothing_is_resized(self):
        backend = self._install(FakeBackend([pane("%1", "cos")]))
        with self.assertRaises(dunk_http.ApiError) as caught:
            dunk_http.route_post("/api/terminals/nope/resize", {"cols": 90, "rows": 30})
        self.assertEqual(404, caught.exception.status)
        self.assertEqual([], backend.calls)

    def test_backend_refusal_is_400_carrying_tmux_wording(self):
        self._install(FakeBackend([pane("%1", "cos")],
                                  result=(False, "width too small")))
        with self.assertRaises(dunk_http.ApiError) as caught:
            dunk_http.route_post("/api/terminals/cos/resize", {"cols": 0, "rows": 30})
        self.assertEqual(400, caught.exception.status)
        self.assertIn("width too small", str(caught.exception))

    def test_backend_without_the_capability_is_501(self):
        self._install(FakeBackend([pane("%1", "cos")], capabilities=()))
        with self.assertRaises(dunk_http.ApiError) as caught:
            dunk_http.route_post("/api/terminals/cos/resize", {"cols": 90, "rows": 30})
        self.assertEqual(501, caught.exception.status)

    def test_missing_fields_and_a_non_object_body_are_400(self):
        self._install(FakeBackend([pane("%1", "cos")]))
        for body in ({"cols": 90}, {"rows": 24}, [], "90x24"):
            with self.assertRaises(dunk_http.ApiError) as caught:
                dunk_http.route_post("/api/terminals/cos/resize", body)
            self.assertEqual(400, caught.exception.status, body)

    def test_an_unknown_write_path_is_404_not_a_silent_get(self):
        self._install(FakeBackend([pane("%1", "cos")]))
        for path in ("/api/terminals/cos/kill", "/api/terminals", "/api/roster"):
            with self.assertRaises(dunk_http.ApiError) as caught:
                dunk_http.route_post(path, {})
            self.assertEqual(404, caught.exception.status, path)

    def test_percent_encoded_name_resolves(self):
        backend = self._install(FakeBackend(
            [pane("%7", "π - vp")],
            result=(True, {"pane_id": "%7", "cols": 70, "rows": 20,
                           "clients_attached": 0, "overridable_by_client": False})))
        dunk_http.route_post("/api/terminals/%CF%80%20-%20vp/resize",
                             {"cols": 70, "rows": 20})
        self.assertEqual("%7", backend.calls[0][0])


class TmuxResize(unittest.TestCase):
    """The real thing, on a tmux server this test owns and kills."""

    def setUp(self):
        if not shutil.which("tmux"):
            self.skipTest("tmux is not installed")
        self.socket = f"dsh-resize-test-{os.getpid()}"
        self.backend = dunk_terminal.TmuxBackend(socket_name=self.socket,
                                                registry_path="/nonexistent.json")
        self.backend._tmux("new", "-d", "-s", "probe", "-x", "80", "-y", "24")

    def tearDown(self):
        try:
            self.backend._tmux("kill-server")
        except Exception:                               # noqa: BLE001 - best effort cleanup
            pass

    def _pane(self):
        return self.backend.list_panes()[0]["pane_id"]

    def test_resize_reports_the_size_tmux_actually_set(self):
        pane_id = self._pane()
        ok, result = self.backend.resize(pane_id, 100, 40)
        self.assertTrue(ok, result)
        self.assertEqual(100, result["cols"])
        self.assertEqual(40, result["rows"])
        self.assertEqual({"cols": 100, "rows": 40}, result["requested"])
        # Independently: tmux itself now reports that geometry.
        code, out, _ = self.backend._tmux("list-panes", "-F",
                                          "#{pane_width}x#{pane_height}")
        self.assertEqual("100x40", out.strip())
        self.assertEqual(0, result["clients_attached"])
        self.assertFalse(result["overridable_by_client"])

    def test_our_bounds_speak_first_for_absurd_sizes(self):
        """0 and 99999 never reach tmux: the guard answers in words the caller can act on."""
        pane_id = self._pane()
        for cols, rows in ((0, 24), (99999, 24), (80, 0), (80, 99999)):
            ok, message = self.backend.resize(pane_id, cols, rows)
            self.assertFalse(ok, (cols, rows))
            self.assertIn("between", message, (cols, rows))
        # Nothing moved.
        code, out, _ = self.backend._tmux("list-panes", "-F",
                                          "#{pane_width}x#{pane_height}")
        self.assertEqual("80x24", out.strip())

    def test_a_size_tmux_itself_refuses_is_reported_in_tmux_words(self):
        """With the guard opened, tmux's refusal ('width too large') is passed through.

        Measured directly on 2026-10-28: `resize-window -x 99999` -> rc 1 'width too
        large'. Through this API that input is caught by RESIZE_MAX first, so this test
        widens the bound to prove the pass-through branch is real and not decorative.
        """
        pane_id = self._pane()
        self.backend.RESIZE_MAX = 100000
        ok, message = self.backend.resize(pane_id, 99999, 24)
        self.assertFalse(ok)
        self.assertIn("width too large", message)

    def test_a_failed_resize_leaves_the_window_exactly_as_it_was(self):
        """A refusal must not report a half-done re-layout - or do one.

        This is the reviewer's first question: if the resize fails, is the window
        untouched? tmux refuses before touching the layout; this test proves it through
        the API AND reads the geometry independently, twice (our own refusals and
        tmux's).
        """
        pane_id = self._pane()
        ok, _ = self.backend.resize(pane_id, 90, 30)
        self.assertTrue(ok)
        before = self.backend._tmux("list-panes", "-F", "#{pane_width}x#{pane_height}")[1]
        self.assertEqual("90x30", before.strip())

        self.backend.RESIZE_MAX = 100000                    # let tmux refuse, not us
        ok, message = self.backend.resize(pane_id, 99999, 30)
        self.assertFalse(ok)
        self.assertIn("width too large", message)
        after = self.backend._tmux("list-panes", "-F", "#{pane_width}x#{pane_height}")[1]
        self.assertEqual(before, after, "a refused resize moved the window")

        ok, message = self.backend.resize(pane_id, 0, 30)   # our guard
        self.assertFalse(ok)
        self.assertEqual(before, self.backend._tmux(
            "list-panes", "-F", "#{pane_width}x#{pane_height}")[1])

    def test_a_multi_pane_window_says_which_panes_it_moved(self):
        """The reviewer's second question: a caller must not need tmux's vocabulary.

        Resizing one pane sizes the WINDOW, so the neighbour re-lays-out too. The answer
        carries window_pane_count and the panes whose geometry changed (with their
        before values), so a caller that asked about one pane learns what else moved.
        """
        panes = self.backend.list_panes()
        first = panes[0]["pane_id"]
        self.backend._tmux("split-window", "-t", first, "-d")
        panes = self.backend.list_panes()
        self.assertEqual(2, len(panes), "the split did not create a second pane")
        before = {p["pane_id"]: (p["cols"], p["rows"]) for p in panes}

        ok, result = self.backend.resize(first, 120, 40)
        self.assertTrue(ok, result)
        self.assertEqual(2, result["window_pane_count"])
        moved = {item["pane_id"]: item for item in result["resized_panes"]}
        self.assertIn(first, moved, "the target itself must appear among the moved panes")
        for pane_id, item in moved.items():
            self.assertEqual(before[pane_id], (item["cols_before"], item["rows_before"]))
        # Every pane tmux now reports as changed must be in the report - nothing silent.
        now = {p["pane_id"]: (p["cols"], p["rows"])
               for p in self.backend.list_panes()}
        changed = {pid for pid, size in now.items() if size != before[pid]}
        self.assertEqual(changed, set(moved), "a pane changed shape without being reported")

    def test_a_non_integer_size_is_refused_before_tmux(self):
        ok, message = self.backend.resize(self._pane(), "abc", 24)
        self.assertFalse(ok)
        self.assertIn("integers", message)

    def test_a_missing_pane_is_refused(self):
        ok, message = self.backend.resize("%9999", 90, 30)
        self.assertFalse(ok)
        self.assertIn("no such pane", message)

    def test_capability_is_advertised_only_by_the_tmux_backend(self):
        self.assertIn("resize", self.backend.capabilities())
        self.assertNotIn("resize", dunk_terminal.TerminalBackend().capabilities())
        ok, message = dunk_terminal.TerminalBackend().resize("%1", 80, 24)
        self.assertFalse(ok)
        self.assertIn("cannot resize", message)


if __name__ == "__main__":
    unittest.main()
