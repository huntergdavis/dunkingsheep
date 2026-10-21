import json
import os
import shutil
import tempfile
import time
import unittest
from unittest import mock

import _bootstrap  # noqa: F401  (isolate live state)
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

    def test_the_real_vp_pane_and_its_containers_are_refused_by_policy(self):
        protected = dunk_tmux_guard.load_protected()
        if "w9:pH" not in protected:
            self.skipTest("the live superuser state does not contain w9:pH")
        panes = [{"pane_id": "w9:pH", "workspace_id": "w9", "tab_id": "w9:t9"},
                 {"pane_id": "w8:p8", "workspace_id": "w8", "tab_id": "w8:t7"}]
        for kind, target in (("pane", "w9:pH"), ("tab", "w9:t9"),
                             ("workspace", "w9")):
            decision = dunk_tmux_guard.close_decision(
                kind, target, panes=panes, protected=protected, managed=set(),
                caller_pane="w8:p8")
            self.assertFalse(decision.allowed, (kind, target))
            self.assertIn("superuser", decision.reason)

    def test_a_same_level_protected_decoy_is_refused_by_the_same_policy(self):
        panes = [{"pane_id": "%42", "workspace_id": "$4", "tab_id": "@4"}]
        decision = dunk_tmux_guard.close_decision(
            "pane", "%42", panes=panes, protected={"%42"}, managed={"%42"},
            caller_pane="%1")
        self.assertFalse(decision.allowed)
        self.assertIn("superuser", decision.reason)

    def test_kill_pane_routes_through_the_daemon_when_policy_allows(self):
        with mock.patch.object(dunk_tmux_guard, "tmux_panes",
                               return_value=[{"pane_id": "%3", "workspace_id": "$1",
                                              "tab_id": "@1"}]), \
                mock.patch.object(dunk_tmux_guard, "load_protected",
                                  return_value=set()), \
                mock.patch.object(dunk_tmux_guard, "load_managed",
                                  return_value={"%3"}), \
                mock.patch.object(dunk_tmux_guard, "route_close",
                                  return_value=0) as route:
            self.assertEqual(0, dunk_tmux_guard.main(["kill-pane", "-t", "%3"]))
            route.assert_called_once_with("pane", "%3", [])

    def test_kill_pane_is_refused_before_the_daemon_when_policy_says_no(self):
        with mock.patch.object(dunk_tmux_guard, "tmux_panes",
                               return_value=[{"pane_id": "%3", "workspace_id": "$1",
                                              "tab_id": "@1"}]), \
                mock.patch.object(dunk_tmux_guard, "load_protected",
                                  return_value={"%3"}), \
                mock.patch.object(dunk_tmux_guard, "load_managed",
                                  return_value={"%3"}), \
                mock.patch.object(dunk_tmux_guard, "route_close") as route:
            self.assertEqual(2, dunk_tmux_guard.main(["kill-pane", "-t", "%3"]))
            route.assert_not_called()

    def test_kill_window_and_session_map_to_tab_and_workspace(self):
        panes = [{"pane_id": "%1", "workspace_id": "$5", "tab_id": "@4"}]
        with mock.patch.object(dunk_tmux_guard, "tmux_panes", return_value=panes), \
                mock.patch.object(dunk_tmux_guard, "load_protected",
                                  return_value=set()), \
                mock.patch.object(dunk_tmux_guard, "load_managed",
                                  return_value={"%1"}), \
                mock.patch.object(dunk_tmux_guard, "route_close",
                                  return_value=0) as route:
            dunk_tmux_guard.main(["kill-window", "-t", "@4"])
            dunk_tmux_guard.main(["kill-session", "-t", "$5"])
        self.assertEqual([mock.call("tab", "@4", []),
                          mock.call("workspace", "$5", [])],
                         route.call_args_list)

    def test_kill_window_without_target_uses_the_current_window(self):
        with mock.patch.object(dunk_tmux_guard, "current_target",
                               return_value="@9") as current, \
                mock.patch.object(dunk_tmux_guard, "tmux_panes",
                                  return_value=[{"pane_id": "%1",
                                                 "workspace_id": "$1",
                                                 "tab_id": "@9"}]), \
                mock.patch.object(dunk_tmux_guard, "load_protected",
                                  return_value=set()), \
                mock.patch.object(dunk_tmux_guard, "load_managed",
                                  return_value={"%1"}), \
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

    def _workspace(self, label):
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

    def test_live_protected_decoy_is_refused_and_survives(self):
        # Same protection level as the VP pane (a superuser entry), but a
        # throwaway: the live guard path is exercised and the pane survives.
        created = self._workspace("guarded-decoy")
        decoy = created["root_pane"]["pane_id"]
        config = os.path.join(self.tmp.name, "protected-config")
        os.makedirs(config, exist_ok=True)
        with open(os.path.join(config, "superusers.json"), "w", encoding="utf-8") as handle:
            json.dump({"panes": [decoy], "names": ["decoy"]}, handle)
        registry = {
            "panes": {decoy: {"at": 1, "agent": "decoy"}},
            "tabs": {created["tab"]["tab_id"]: {"at": 1}},
            "workspaces": {created["workspace"]["workspace_id"]: {"at": 1}},
        }
        with open(os.path.join(config, "tmux-panes.json"), "w", encoding="utf-8") as handle:
            json.dump(registry, handle)
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_DIR": config}):
            code = dunk_tmux_guard.main(
                ["-L", self.socket, "kill-pane", "-t", decoy])
        self.assertEqual(2, code)
        self.assertIsNotNone(self.backend.get_pane(decoy))

    def test_live_managed_decoy_is_actually_killed(self):
        # Positive control: the same live close path does kill a managed,
        # non-protected throwaway, so the refusal above is a policy decision,
        # not a broken close.
        created = self._workspace("close-positive")
        panes = [created["root_pane"]["pane_id"]]
        ok, tabbed = self.backend.create_tab(
            workspace_id=created["workspace"]["workspace_id"], label="victim")
        self.assertTrue(ok, tabbed)
        victim = tabbed["root_pane"]["pane_id"]
        self.assertTrue(self.backend.close("pane", victim)[0])
        self.assertIsNone(self.backend.get_pane(victim))
        self.assertIsNotNone(self.backend.get_pane(panes[0]))

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
