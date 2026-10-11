"""The herdr guard: install, status, uninstall, and what it intercepts."""

import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))


class GuardInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.bashrc = os.path.join(self.tmp.name, "bashrc")
        open(self.bashrc, "w").write("# existing\nexport PATH=\"$HOME/.local/bin:$PATH\"\n")
        import dunk_guard
        self.guard = dunk_guard
        self.patches = [
            mock.patch.object(dunk_guard, "CONFIG_DIR", self.tmp.name),
            mock.patch.object(dunk_guard, "GUARD_DIR", os.path.join(self.tmp.name, "bin")),
            mock.patch.object(dunk_guard, "GUARD_PATH",
                              os.path.join(self.tmp.name, "bin", "herdr")),
            mock.patch.object(dunk_guard, "BASHRC", self.bashrc),
        ]
        # A sandboxed PATH with a stand-in herdr, so nothing here can ever
        # reach the real binary (it did once, and renamed it mid-incident).
        self.bin_dir = os.path.join(self.tmp.name, "realbin")
        os.makedirs(self.bin_dir)
        self.real_herdr = os.path.join(self.bin_dir, "herdr")
        with open(self.real_herdr, "w") as handle:
            handle.write("#!/bin/sh\necho REAL HERDR\n")
        os.chmod(self.real_herdr, 0o755)
        # First on PATH, so every herdr lookup finds this stub and never the
        # real binary, while the rest of PATH stays usable (python3 etc.).
        self.patches.append(mock.patch.dict(
            os.environ, {"PATH": self.bin_dir + os.pathsep + os.environ.get("PATH", "")},
            clear=False))
        for patch in self.patches:
            patch.start()
        os.environ.pop("DUNKINGSHEEP_HERDR_BIN", None)

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.tmp.cleanup()

    def test_install_is_idempotent_and_uninstall_reverts(self):
        self.assertFalse(self.guard.status()["installed"])
        first = self.guard.install()
        self.assertTrue(first["installed"])
        self.assertTrue(first["bashrc_updated"])
        self.assertTrue(first["shell_configured"])
        self.assertTrue(os.access(self.guard.GUARD_PATH, os.X_OK))
        body = open(self.bashrc).read()
        self.assertEqual(1, body.count(self.guard.MARKER))
        # Installing twice adds nothing further.
        second = self.guard.install()
        self.assertFalse(second["bashrc_updated"])
        self.assertEqual(1, open(self.bashrc).read().count(self.guard.MARKER))
        # Uninstall removes both the wrapper and the PATH line, leaving the
        # user's own bashrc untouched.
        result = self.guard.uninstall()
        self.assertTrue(result["wrapper_removed"])
        self.assertTrue(result["bashrc_cleaned"])
        self.assertFalse(os.path.exists(self.guard.GUARD_PATH))
        after = open(self.bashrc).read()
        self.assertNotIn(self.guard.MARKER, after)
        self.assertIn("# existing", after)
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', after)

    def test_wrapper_routes_typing_and_passes_everything_else_through(self):
        self.guard.install()
        fake_bin = self.bin_dir
        log = os.path.join(self.tmp.name, "calls.log")
        # A stand-in for the real herdr, and for the dunkingsheep CLI.
        with open(os.path.join(fake_bin, "herdr"), "w") as handle:
            handle.write("#!/usr/bin/env python3\nimport sys\n"
                         f"open({log!r},'a').write('HERDR '+' '.join(sys.argv[1:])+'\\n')\n")
        os.chmod(os.path.join(fake_bin, "herdr"), 0o755)
        cli = os.path.join(self.tmp.name, "dunkingsheep")
        with open(cli, "w") as handle:
            handle.write("#!/usr/bin/env python3\nimport sys\n"
                         f"open({log!r},'a').write('DS '+' '.join(sys.argv[1:])+'\\n')\n")
        os.chmod(cli, 0o755)
        wrapper = open(self.guard.GUARD_PATH).read().replace(
            self.guard._cli_path(), cli)
        open(self.guard.GUARD_PATH, "w").write(wrapper)

        env = dict(os.environ, PATH=fake_bin + os.pathsep + os.environ["PATH"])
        env.pop("DUNKINGSHEEP_GUARD", None)
        env.pop("DUNKINGSHEEP_HERDR_BIN", None)

        def run(*argv):
            subprocess.run([sys.executable, self.guard.GUARD_PATH, *argv],
                           env=env, capture_output=True, text=True, timeout=30)

        run("pane", "run", "w1:p1", "hello there")
        run("pane", "send-text", "w1:p1", "no enter")
        run("pane", "send-keys", "w1:p1", "Enter")
        run("agent", "send", "codex", "hi")
        run("pane", "list")                      # untouched
        run("tab", "create", "--label", "x")     # untouched
        calls = open(log).read().splitlines()
        routed = [c for c in calls if c.startswith("DS ")]
        direct = [c for c in calls if c.startswith("HERDR ")]
        self.assertEqual(4, len(routed), calls)
        self.assertIn("DS --json send w1:p1 hello there", calls)
        self.assertIn("DS --json send w1:p1 no enter --no-enter", calls)
        self.assertIn("DS --json send-keys w1:p1 Enter", calls)
        self.assertIn("DS --json send codex hi", calls)
        self.assertEqual(["HERDR pane list", "HERDR tab create --label x"], direct)

    def test_the_daemon_itself_is_never_routed_through_the_guard(self):
        self.guard.install()
        fake_bin = self.bin_dir
        log = os.path.join(self.tmp.name, "calls.log")
        with open(os.path.join(fake_bin, "herdr"), "w") as handle:
            handle.write("#!/usr/bin/env python3\nimport sys\n"
                         f"open({log!r},'a').write('HERDR '+' '.join(sys.argv[1:])+'\\n')\n")
        os.chmod(os.path.join(fake_bin, "herdr"), 0o755)
        env = dict(os.environ, PATH=fake_bin + os.pathsep + os.environ["PATH"],
                   DUNKINGSHEEP_GUARD="1")
        subprocess.run([sys.executable, self.guard.GUARD_PATH, "pane", "run", "w1:p1", "x"],
                       env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(["HERDR pane run w1:p1 x"], open(log).read().splitlines())

    def test_install_alone_never_touches_the_herdr_binary(self):
        self.guard.install()
        self.assertFalse(self.guard._is_guard(self.real_herdr),
                         "install must not rename herdr unless asked")
        self.assertFalse(os.path.exists(os.path.join(self.bin_dir, "herdr-real")))
        self.assertFalse(self.guard.status()["shadowing_herdr"])

    def test_shadow_replaces_herdr_in_place_and_unshadow_restores_it(self):
        bin_dir, real = self.bin_dir, self.real_herdr
        if True:
            self.guard.install(shadow_binary=True)
            # herdr itself is now the wrapper, with the real one beside it.
            self.assertTrue(self.guard._is_guard(real))
            self.assertTrue(os.access(os.path.join(bin_dir, "herdr-real"), os.X_OK))
            state = self.guard.status()
            self.assertTrue(state["shadowing_herdr"])
            self.assertTrue(state["covers_agents_started_by_herdr"])
            # The daemon still resolves the real binary, not the wrapper.
            import herdr_client
            self.assertEqual(os.path.join(bin_dir, "herdr-real"),
                             herdr_client._find_herdr())
            # Installing again is harmless.
            self.guard.install()
            self.assertEqual("#!/bin/sh\necho REAL HERDR\n",
                             open(os.path.join(bin_dir, "herdr-real")).read())
            # And uninstall puts herdr back exactly as it was.
            self.guard.uninstall()
            self.assertFalse(self.guard._is_guard(real))
            self.assertEqual("#!/bin/sh\necho REAL HERDR\n", open(real).read())
            self.assertFalse(os.path.exists(os.path.join(bin_dir, "herdr-real")))

    def test_herdr_client_skips_the_guard_directory(self):
        self.guard.install()
        import herdr_client
        with mock.patch.object(herdr_client, "GUARD_DIR", self.guard.GUARD_DIR), \
                mock.patch.dict(os.environ, {"PATH": self.guard.GUARD_DIR + os.pathsep
                                             + "/usr/bin"}, clear=False):
            os.environ.pop("DUNKINGSHEEP_HERDR_BIN", None)
            resolved = herdr_client._find_herdr()
        self.assertNotEqual(os.path.realpath(self.guard.GUARD_PATH),
                            os.path.realpath(resolved))


if __name__ == "__main__":
    unittest.main()
