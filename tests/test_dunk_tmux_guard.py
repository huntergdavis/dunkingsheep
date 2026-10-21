import os
import shutil
import tempfile
import time
import unittest
from unittest import mock

import dunk_guard
import dunk_tmux_guard
from dunk_terminal import TmuxBackend


class GuardClassificationTests(unittest.TestCase):
    def test_global_options_are_split_from_the_command(self):
        globals_, command, rest = dunk_tmux_guard.split_global(
            ["-L", "sock", "-u", "kill-pane", "-t", "%1"])
        self.assertEqual(["-L", "sock", "-u"], globals_)
        self.assertEqual("kill-pane", command)
        self.assertEqual(["-t", "%1"], rest)

    def test_target_accepts_joined_and_separate_t(self):
        self.assertEqual("%2", dunk_tmux_guard.option_value(["-t", "%2"]))
        self.assertEqual("%2", dunk_tmux_guard.option_value(["-t%2"]))
        self.assertIsNone(dunk_tmux_guard.option_value(["-a"]))

    def test_kill_pane_routes_through_the_daemon(self):
        with mock.patch.object(dunk_tmux_guard, "route_close",
                               return_value=0) as route:
            self.assertEqual(0, dunk_tmux_guard.main(["kill-pane", "-t", "%3"]))
            route.assert_called_once_with("pane", "%3", [])

    def test_kill_window_and_session_map_to_tab_and_workspace(self):
        with mock.patch.object(dunk_tmux_guard, "route_close",
                               return_value=0) as route:
            dunk_tmux_guard.main(["kill-window", "-t", "@4"])
            dunk_tmux_guard.main(["kill-session", "-t", "$5"])
        self.assertEqual([mock.call("tab", "@4", []),
                          mock.call("workspace", "$5", [])],
                         route.call_args_list)

    def test_kill_window_without_target_uses_the_current_window(self):
        with mock.patch.object(dunk_tmux_guard, "current_target",
                               return_value="@9") as current, \
                mock.patch.object(dunk_tmux_guard, "route_close",
                                  return_value=0) as route:
            dunk_tmux_guard.main(["kill-window"])
        current.assert_called_once_with("tab", None, [])
        route.assert_called_once_with("tab", "@9", [])

    def test_kill_all_is_refused(self):
        for argv in (["kill-pane", "-a"], ["kill-window", "-a"],
                     ["kill-session", "-a"]):
            self.assertEqual(2, dunk_tmux_guard.main(argv))

    def test_kill_server_is_refused_outright(self):
        self.assertEqual(2, dunk_tmux_guard.main(["kill-server"]))

    def test_typing_is_refused_with_the_daemon_pointer(self):
        for argv in (["send-keys", "-t", "%1", "hello"],
                     ["send-keys", "-t", "%1", "Enter"],
                     ["paste-buffer", "-t", "%1"],
                     ["run-shell", "echo hi"]):
            self.assertEqual(2, dunk_tmux_guard.main(argv))

    def test_respawn_is_refused(self):
        self.assertEqual(2, dunk_tmux_guard.main(["respawn-pane", "-k", "-t", "%1"]))

    def test_read_only_commands_pass_through(self):
        with mock.patch.object(dunk_tmux_guard, "passthrough",
                               return_value=0) as passthrough:
            self.assertEqual(0, dunk_tmux_guard.main(["list-panes"]))
            self.assertEqual(0, dunk_tmux_guard.main(["capture-pane", "-p", "-t", "%1"]))
        self.assertEqual(2, passthrough.call_count)

    def test_bare_tmux_and_version(self):
        with mock.patch.object(dunk_tmux_guard, "passthrough",
                               return_value=0) as passthrough:
            self.assertEqual(0, dunk_tmux_guard.main(["-V"]))
        passthrough.assert_called_once()
        self.assertEqual(2, dunk_tmux_guard.main([]))


class InstallTests(unittest.TestCase):
    def test_install_writes_a_wrapper_and_a_real_binary(self):
        if not shutil.which("tmux"):
            self.skipTest("tmux is not installed")
        with tempfile.TemporaryDirectory() as tmp:
            guard_dir = os.path.join(tmp, "tmux-guard")
            with mock.patch.object(dunk_guard, "TMUX_GUARD_DIR", guard_dir):
                result = dunk_guard.install_tmux()
            self.assertTrue(result["ok"])
            wrapper = os.path.join(guard_dir, "tmux")
            real = os.path.join(guard_dir, "tmux-real")
            self.assertTrue(os.path.isfile(wrapper))
            self.assertTrue(os.path.exists(real))
            self.assertTrue(os.access(wrapper, os.X_OK))
            with open(wrapper, encoding="utf-8") as handle:
                text = handle.read()
            self.assertIn("dunkingsheep-tmux-guard", text)
            self.assertIn(dunk_guard._cli_path(), text)
            self.assertNotIn("__DUNKINGSHEEP__", text)

    def test_backend_prepends_the_guard_only_for_agent_panes(self):
        if not shutil.which("tmux"):
            self.skipTest("tmux is not installed")
        with tempfile.TemporaryDirectory() as tmp:
            guard_dir = os.path.join(tmp, "tmux-guard")
            with mock.patch.object(dunk_guard, "TMUX_GUARD_DIR", guard_dir):
                dunk_guard.install_tmux()
            backend = TmuxBackend(
                socket_name=f"dsh-guardpath-{os.getpid()}", tmux_bin="tmux",
                guard_dir=guard_dir,
                registry_path=os.path.join(tmp, "reg.json"))
            self.addCleanup(lambda: backend._run(["kill-server"], timeout=5))
            ok, created = backend.create_workspace("path-test", cwd=tmp)
            self.assertTrue(ok, created)
            fake = os.path.join(tmp, "claude")
            with open(fake, "w", encoding="utf-8") as handle:
                handle.write("#!/bin/sh\necho PATH=$PATH\nsleep 20\n")
            os.chmod(fake, 0o755)
            ok, result = backend.start_agent(
                "path-agent", [fake],
                workspace_id=created["workspace"]["workspace_id"], cwd=tmp)
            self.assertTrue(ok, result)
            text = ""
            deadline = time.time() + 8
            while time.time() < deadline:
                text = backend.read_pane(result["pane_id"], lines=80) or ""
                if "PATH=" in text:
                    break
                time.sleep(0.1)
            self.assertIn(guard_dir, text)
            self.assertTrue(backend.close("pane", result["pane_id"])[0])


@unittest.skipUnless(shutil.which("tmux"), "tmux is not installed")
class LiveGuardTests(unittest.TestCase):
    """The guard as the agent actually sees it: PATH-first, real tmux beside.

    The daemon in this session is still the herdr backend, so a routed close of
    a tmux pane must fail closed (it cannot resolve a herdr pane). The point is
    that the pane survives and the agent is told why.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.guard_dir = os.path.join(self.tmp.name, "tmux-guard")
        with mock.patch.object(dunk_guard, "TMUX_GUARD_DIR", self.guard_dir):
            dunk_guard.install_tmux()
        self.socket = f"dsh-live-guard-{os.getpid()}-{id(self)}"
        self.backend = TmuxBackend(
            socket_name=self.socket, guard_dir=self.guard_dir,
            registry_path=os.path.join(self.tmp.name, "reg.json"))
        self.addCleanup(self._cleanup)

    def _cleanup(self):
        try:
            self.backend._run(["kill-server"], timeout=5)
        except Exception:  # noqa: BLE001
            pass
        self.tmp.cleanup()

    def _wait_text(self, pane_id, needle, timeout=8):
        text = ""
        deadline = time.time() + timeout
        while time.time() < deadline:
            text = self.backend.read_pane(pane_id, lines=120) or ""
            if needle in text:
                return text
            time.sleep(0.1)
        return text

    def test_agent_cannot_kill_its_pane_or_server_through_raw_tmux(self):
        ok, created = self.backend.create_workspace("guarded", cwd=self.tmp.name)
        self.assertTrue(ok, created)
        script = os.path.join(self.tmp.name, "claude")
        with open(script, "w", encoding="utf-8") as handle:
            handle.write(
                "#!/bin/sh\n"
                "echo which=$(command -v tmux)\n"
                "tmux kill-server 2>&1; echo server_rc=$?\n"
                "tmux kill-pane -t \"$TMUX_PANE\" 2>&1; echo pane_rc=$?\n"
                "tmux send-keys -t \"$TMUX_PANE\" x 2>&1; echo keys_rc=$?\n"
                "sleep 30\n")
        os.chmod(script, 0o755)
        ok, result = self.backend.start_agent(
            "guarded-agent", [script],
            workspace_id=created["workspace"]["workspace_id"], cwd=self.tmp.name)
        self.assertTrue(ok, result)
        pane = result["pane_id"]
        text = self._wait_text(pane, "keys_rc=")
        self.assertIn(self.guard_dir, text)
        self.assertIn("refusing `tmux kill-server`", text)
        self.assertIn("server_rc=2", text)
        self.assertIn("pane_rc=", text)
        self.assertIn("keys_rc=2", text)
        # The pane and the server survived every attempt.
        self.assertIsNotNone(self.backend.get_pane(pane))
        self.assertTrue(self.backend.close("pane", pane)[0])


if __name__ == "__main__":
    unittest.main()
