import os
import shutil
import tempfile
import time
import unittest
from unittest import mock

import _bootstrap  # noqa: F401  (isolate live state)
from dunk_terminal import HerdrBackend, TerminalBackend, TmuxBackend, get_backend


class FakeClient:
    """Minimal herdr-shaped client for the delegation tests."""

    def __init__(self):
        self.asked = []

    def agent_status(self, pane_id):
        self.asked.append(pane_id)
        return "working"

    def agent_session(self, pane_id):
        return {"kind": "id", "value": "sess-1"}

    def get_pane(self, pane_id):
        return {"pane_id": pane_id, "agent_status": "idle"}

    def list_panes(self):
        return [{"pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1"}]

    def list_tabs(self):
        return [{"tab_id": "w1:t1", "workspace_id": "w1", "label": "one", "number": 1}]

    def list_workspaces(self):
        return [{"workspace_id": "w1", "label": "alpha", "number": 1}]


class BackendSelectionTests(unittest.TestCase):
    def test_default_is_tmux(self):
        """t50 step 1 (owner decision): the default must not depend on a drop-in.
        herdr remains selectable explicitly for the rollback."""
        with mock.patch.dict(os.environ,
                                      {"DUNKINGSHEEP_TERMINAL_BACKEND": ""}):
            self.assertIsInstance(get_backend(), TmuxBackend)

    def test_tmux_is_selected_by_env(self):
        with mock.patch.dict(
                os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "tmux"}):
            self.assertIsInstance(get_backend(), TmuxBackend)

    def test_unknown_backend_is_a_named_refusal(self):
        with mock.patch.dict(
                os.environ, {"DUNKINGSHEEP_TERMINAL_BACKEND": "vt100"}):
            with self.assertRaisesRegex(ValueError, "unknown terminal backend"):
                get_backend()

    def test_an_existing_client_is_wrapped_not_replaced(self):
        client = FakeClient()
        backend = get_backend(client)
        self.assertIsInstance(backend, HerdrBackend)
        self.assertIs(client, backend.client)

    def test_explicit_backend_passes_through(self):
        backend = TmuxBackend(tmux_bin="/bin/false")
        self.assertIs(backend, get_backend(backend))


class HerdrDelegationTests(unittest.TestCase):
    def test_live_agent_status_comes_from_the_client_not_the_pane_cache(self):
        client = FakeClient()
        backend = HerdrBackend(client)
        self.assertEqual("working", backend.agent_status("w1:p1"))
        self.assertEqual(["w1:p1"], client.asked)

    def test_grouping_is_delegated(self):
        backend = HerdrBackend(FakeClient())
        grouped = backend.list_panes_grouped()
        self.assertEqual("alpha", grouped[0]["workspace_label"])
        self.assertEqual("one", grouped[0]["tab_label"])

    def test_capabilities_are_honest(self):
        self.assertIn("agent_status", HerdrBackend(FakeClient()).capabilities())


@unittest.skipUnless(shutil.which("tmux"), "tmux is not installed")
class TmuxLiveTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.socket = f"dsh-test-{os.getpid()}-{id(self)}"
        self.backend = TmuxBackend(
            socket_name=self.socket,
            registry_path=os.path.join(self.tmp.name, "tmux-panes.json"))
        self.addCleanup(self._cleanup)

    def _cleanup(self):
        try:
            self.backend._run(["kill-server"], timeout=5)
        except Exception:  # noqa: BLE001 - best effort test cleanup
            pass
        self.tmp.cleanup()

    def _workspace(self, label="dsh-live"):
        ok, result = self.backend.create_workspace(label, cwd=self.tmp.name)
        self.assertTrue(ok, result)
        return result

    def _wait_text(self, pane_id, needle, timeout=8):
        text = ""
        deadline = time.time() + timeout
        while time.time() < deadline:
            text = self.backend.read_pane(pane_id, lines=120) or ""
            if needle in text:
                return text
            time.sleep(0.1)
        return text

    def _manual_pane(self, pane_id, direction="right"):
        code, out, err = self.backend._tmux(
            "split-window", "-t", pane_id, "-h" if direction == "right" else "-v",
            "-P", "-F", "#{pane_id}")
        self.assertEqual(0, code, err)
        return out.strip()

    def test_workspace_tab_split_identity_and_screen(self):
        created = self._workspace()
        workspace = created["workspace"]
        root = created["root_pane"]
        self.assertEqual("dsh-live", workspace["label"])
        self.assertTrue(root["pane_id"].startswith("%"))
        self.assertEqual(workspace["workspace_id"], root["workspace_id"])

        ok, tabbed = self.backend.create_tab(
            workspace_id=workspace["workspace_id"], label="second", cwd=self.tmp.name)
        self.assertTrue(ok, tabbed)
        second_pane = tabbed["root_pane"]["pane_id"]
        self.assertNotEqual(root["pane_id"], second_pane)

        ok, split = self.backend.split_pane(second_pane, direction="right",
                                            cwd=self.tmp.name)
        self.assertTrue(ok, split)

        ok, message = self.backend.send_text_and_enter(
            root["pane_id"], "printf 'hello-tmux\\n'")
        self.assertTrue(ok, message)
        self.assertIn("hello-tmux",
                      self._wait_text(root["pane_id"], "hello-tmux"))

        panes = {p["pane_id"]: p for p in self.backend.list_panes()}
        self.assertTrue(panes[root["pane_id"]]["managed"])
        self.assertEqual(self.tmp.name, panes[root["pane_id"]]["cwd"])

        ok, renamed = self.backend.rename("pane", root["pane_id"], "dsh-root")
        self.assertTrue(ok, renamed)
        self.assertEqual("dsh-root",
                         self.backend.get_pane(root["pane_id"])["label"])

        # Closing a managed pane is permitted; closing a window that still has
        # a managed pane (the split) is permitted too.
        self.assertTrue(self.backend.close("pane", split["pane"]["pane_id"])[0])
        self.assertTrue(self.backend.close("tab", tabbed["tab"]["tab_id"])[0])

    def _fake_agent(self):
        bin_dir = os.path.join(self.tmp.name, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        path = os.path.join(bin_dir, "claude")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("#!/bin/sh\nprintf 'fake-agent ready\\n'\nsleep 30\n")
        os.chmod(path, 0o755)
        return path

    def test_start_agent_labels_the_pane_and_is_closeable(self):
        created = self._workspace("dsh-agent-live")
        agent_path = self._fake_agent()
        env = {"PATH": os.path.dirname(agent_path) + os.pathsep
               + os.environ.get("PATH", "")}
        ok, result = self.backend.start_agent(
            "dsh-lane", [agent_path], workspace_id=created["workspace"]["workspace_id"],
            cwd=self.tmp.name, env=env)
        self.assertTrue(ok, result)
        pane_id = result["pane_id"]
        self.assertEqual("claude", result["agent"]["agent"])
        self.assertIn("fake-agent ready", self._wait_text(pane_id, "fake-agent ready"))
        self.assertEqual("dsh-lane", self.backend.get_pane(pane_id)["label"])
        self.assertTrue(self.backend.close("pane", pane_id)[0])

    def test_unmanaged_pane_is_never_closed_like_the_vps_pane(self):
        created = self._workspace("dsh-guard-live")
        workspace = created["workspace"]
        root = created["root_pane"]["pane_id"]
        manual = self._manual_pane(root)
        # The manual pane is real and visible, but Dunking Sheep did not create
        # it: this is the VP's-pane case, and closing it must be refused.
        self.assertIsNotNone(self.backend.get_pane(manual))
        ok, message = self.backend.close("pane", manual)
        self.assertFalse(ok)
        self.assertIn("did not create", message)
        # A window/session containing an unmanaged pane is refused too.
        ok, message = self.backend.close("tab", created["tab"]["tab_id"])
        self.assertFalse(ok)
        self.assertIn("contains panes", message)
        ok, message = self.backend.close("workspace", workspace["workspace_id"])
        self.assertFalse(ok)
        self.assertIn("contains panes", message)
        # The pane is still alive after all refused closes.
        self.assertIsNotNone(self.backend.get_pane(manual))

    def test_managed_tab_and_pane_close(self):
        created = self._workspace("dsh-close-live")
        workspace = created["workspace"]
        root = created["root_pane"]["pane_id"]
        ok, tabbed = self.backend.create_tab(
            workspace_id=workspace["workspace_id"], label="closable")
        self.assertTrue(ok, tabbed)
        self.assertTrue(self.backend.close("tab", tabbed["tab"]["tab_id"])[0])
        self.assertIsNone(self.backend.get_pane(tabbed["root_pane"]["pane_id"]))
        self.assertTrue(self.backend.close("pane", root)[0])
        # The workspace's last window is gone; the session may be gone too.
        self.assertTrue(self.backend.close("workspace", workspace["workspace_id"])[0]
                        or self.backend.get_pane(root) is None)


if __name__ == "__main__":
    unittest.main()
