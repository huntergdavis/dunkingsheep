"""Binary selection only: every process/exec/signal boundary is forbidden."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import _bootstrap  # noqa: F401
import dunk_guard
import dunk_tmux_guard


class ControllerReached(Exception):
    pass


class ResolutionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.agent = self.root / "agent"
        self.binary = self.root / "bin" / "tmux"
        self.destination = self.root / "installed"
        self.write(self.agent / "tmux", "# dunkingsheep-tmux-guard\n")
        self.write(self.binary, "# synthetic non-guard executable; never run\n")
        patches = [
            mock.patch.dict(os.environ, {"PATH": str(self.agent) + os.pathsep + str(self.binary.parent),
                                         "DUNKINGSHEEP_TMUX_BIN": ""}),
            mock.patch.object(dunk_guard, "TMUX_GUARD_DIR", str(self.destination)),
            mock.patch.object(subprocess, "run", side_effect=AssertionError("real subprocess forbidden")),
            mock.patch.object(subprocess, "Popen", side_effect=AssertionError("real Popen forbidden")),
            mock.patch.object(os, "execv", side_effect=AssertionError("real exec forbidden")),
            mock.patch.object(os, "execve", side_effect=AssertionError("real exec forbidden")),
            mock.patch.object(os, "kill", side_effect=AssertionError("real signal forbidden")),
        ]
        for patcher in patches:
            patcher.start()
            self.addCleanup(patcher.stop)

    def write(self, path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\n" + content)
        path.chmod(0o755)

    def test_path_guard_is_skipped_and_result_is_absolute(self):
        self.assertEqual(str(self.binary.resolve()), dunk_guard.resolve_real_tmux())
        self.assertFalse(dunk_tmux_guard.is_guard(dunk_guard.resolve_real_tmux()))

    def test_guard_to_guard_sibling_and_override_are_never_selected(self):
        self.destination.mkdir()
        (self.destination / "tmux-real").symlink_to(self.agent / "tmux")
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TMUX_BIN": str(self.agent / "tmux")}):
            self.assertEqual(str(self.binary), dunk_guard.resolve_real_tmux())

    def test_guard_only_chain_is_refused_before_install_changes(self):
        self.destination.mkdir()
        link = self.destination / "tmux-real"
        link.symlink_to(self.agent / "tmux")
        self.write(self.destination / "tmux", "# dunkingsheep-tmux-guard\n# preserved\n")
        before = (self.destination / "tmux").read_bytes()
        target = os.readlink(link)
        with mock.patch.dict(os.environ, {"PATH": str(self.agent)}):
            with self.assertRaisesRegex(FileNotFoundError, "non-guard tmux"):
                dunk_guard.install_tmux()
        self.assertEqual(before, (self.destination / "tmux").read_bytes())
        self.assertEqual(target, os.readlink(link))

    def test_install_under_agent_path_links_non_guard_binary(self):
        result = dunk_guard.install_tmux()
        self.assertEqual(str(self.binary), result["real_tmux"])
        self.assertEqual(self.binary, (self.destination / "tmux-real").resolve())
        self.assertTrue(dunk_tmux_guard.is_guard(self.destination / "tmux"))
        self.assertFalse(dunk_tmux_guard.is_guard(self.destination / "tmux-real"))

    def test_reinstall_preserves_a_copied_real_binary(self):
        copied = self.destination / "tmux-real"
        self.write(copied, "# copied real binary sentinel\n")
        before = copied.read_bytes()
        with mock.patch.dict(os.environ, {"PATH": str(self.agent)}):
            dunk_guard.install_tmux()
            dunk_guard.install_tmux()
        self.assertFalse(copied.is_symlink())
        self.assertEqual(before, copied.read_bytes())

    def test_reinstall_through_symlinked_directory_preserves_copied_binary(self):
        copied = self.destination / "tmux-real"
        self.write(copied, "# copied real binary sentinel\n")
        alias = self.root / "alias"
        alias.symlink_to(self.destination, target_is_directory=True)
        before = copied.read_bytes()
        with mock.patch.object(dunk_guard, "TMUX_GUARD_DIR", str(alias)):
            dunk_guard.install_tmux()
        self.assertFalse(copied.is_symlink())
        self.assertEqual(before, copied.read_bytes())

    def test_install_refuses_to_overwrite_hardlinked_selected_binary(self):
        self.destination.mkdir()
        target = self.destination / "tmux"
        os.link(self.binary, target)
        with self.assertRaisesRegex(RuntimeError, "selected real tmux"):
            dunk_guard.install_tmux()
        self.assertFalse((self.destination / "tmux-real").exists())

    def test_install_refuses_to_overwrite_selected_binary(self):
        target = self.destination / "tmux"
        self.write(target, "# selected non-guard binary\n")
        before = target.read_bytes()
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_TMUX_BIN": str(target)}):
            with self.assertRaisesRegex(RuntimeError, "selected real tmux"):
                dunk_guard.install_tmux()
        self.assertEqual(before, target.read_bytes())
        self.assertFalse((self.destination / "tmux-real").exists())

    def check_original_fixture_controller(self, name):
        # Load the actual fixture under guard-first PATH. Intercept construction
        # before any lifecycle method: this is not evidence of a live session.
        path = Path(__file__).with_name("test_dunk_tmux_guard.py")
        spec = importlib.util.spec_from_file_location("tmux_fixture_resolution_probe", path)
        fixtures = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixtures)
        cls, method = name.split(".")
        case = getattr(fixtures, cls)(method)
        def controller(*args, **kwargs):
            self.assertEqual(str(self.binary.resolve()), kwargs.get("tmux_bin"))
            self.assertTrue(dunk_tmux_guard.is_guard(Path(kwargs["guard_dir"]) / "tmux"))
            raise ControllerReached()
        try:
            with mock.patch.object(fixtures, "TmuxBackend", side_effect=controller) as construct:
                with self.assertRaises(ControllerReached):
                    case.setUp()
                    getattr(case, method)()
                self.assertEqual(1, construct.call_count)
        finally:
            if hasattr(case, "tmp"):
                case.tmp.cleanup()
            case.doCleanups()

    def test_install_fixture_controller(self):
        self.check_original_fixture_controller("InstallTests.test_backend_prepends_the_guard_only_for_agent_panes")

    def test_protected_fixture_controller(self):
        self.check_original_fixture_controller("LiveGuardTests.test_live_protected_decoy_is_refused_and_survives")

    def test_managed_fixture_controller(self):
        self.check_original_fixture_controller("LiveGuardTests.test_live_managed_decoy_is_actually_killed")

    def test_agent_fixture_controller(self):
        self.check_original_fixture_controller("LiveGuardTests.test_agent_cannot_kill_its_pane_or_server_through_raw_tmux")
