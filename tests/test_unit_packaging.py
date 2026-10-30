"""t735: a fresh install must start the units with no hand edits.

Found during the FF-10 cutover (steward %107, 2026-10-29 23:10-23:20): all four
defects below had to be worked around by hand, and every future install hits them.

1. `bridges/run.sh` was mode 100644 in git, so `dunkingsheep-web.service` died with
   status=203/EXEC and restarted in a loop (counter reached 17). The cutover fix was
   `chmod +x` on the STAGED release tree, which does not survive the next release.
2. `dunkingsheep-telegram.service` ran `.../current/.venv/bin/python
   bridges/telegram_bridge.py` with no PYTHONPATH and no subcommand: `web` was not
   importable and the script requires one of {setup,run,status}.
3. `tools/telegram_acceptance.sh` refused a daemon that HAD the verbs (fixed with the
   precondition probing the verb - see t594/PR151).
4. `web/app.py` raised a bare FileNotFoundError for a missing VP_CHANNELS_FILE, which
   the documented install does not create.

These tests catch 1 and 2 without a host, as the task asks.
"""

import os
import re
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
UNITS = os.path.join(ROOT, "packaging", "systemd")


def unit(name):
    with open(os.path.join(UNITS, name), encoding="utf-8") as handle:
        return handle.read()


def exec_start(name):
    for line in unit(name).splitlines():
        if line.startswith("ExecStart="):
            return line[len("ExecStart="):]
    raise AssertionError(f"{name} has no ExecStart")


def executable_scripts(name):
    """Scripts a unit runs directly (not through an interpreter or the CLI)."""
    argv = exec_start(name).split()
    out = []
    for index, word in enumerate(argv):
        if word.startswith("-") or index == 0 and "/" not in word:
            continue
        if re.search(r"\.(sh|py)$", word):
            out.append(word)
    return out


class UnitPackagingTests(unittest.TestCase):
    def test_every_script_a_unit_runs_is_executable_in_the_index(self):
        """t735 item 1: a 100644 script is 203/EXEC, and a chmod on a staged release
        does not survive the next release - the INDEX is the durable place."""
        checked = 0
        for name in sorted(os.listdir(UNITS)):
            if not name.endswith(".service"):
                continue
            for script in executable_scripts(name):
                rel = script.split("/current/")[-1].lstrip("/")
                proc = subprocess.run(["git", "-C", ROOT, "ls-files", "-s", "--", rel],
                                      capture_output=True, text=True)
                if proc.returncode != 0 or not proc.stdout.strip():
                    continue                      # not tracked here (e.g. an installed path)
                mode = proc.stdout.split()[0]
                self.assertEqual("100755", mode,
                                 f"{rel} is run by {name} but is mode {mode} in git")
                checked += 1
        self.assertGreaterEqual(checked, 1, "no unit script was checked - the test is asleep")

    def test_the_telegram_unit_runs_the_installed_wrapper_with_a_subcommand(self):
        """t735 item 2: no invented .venv path, no relative script, and the subcommand
        the script requires."""
        line = exec_start("dunkingsheep-telegram.service")
        self.assertNotIn(".venv", line)
        self.assertIn("/.local/bin/dunkingsheep", line)
        self.assertIn("--no-start-daemon", line)
        self.assertIn("telegram-bridge run", line)

    def test_no_unit_points_dunkingsheep_dir_at_the_install(self):
        """t735 item 4, same class: DUNKINGSHEEP_DIR is the CONFIG dir; a release path
        sends the daemon socket, the role registry and inbox.db into the release tree."""
        for name in sorted(os.listdir(UNITS)):
            if not name.endswith(".service"):
                continue
            for line in unit(name).splitlines():
                if line.startswith("DUNKINGSHEEP_DIR="):
                    self.assertIn(".config/dunkingsheep", line,
                                  f"{name}: DUNKINGSHEEP_DIR must be the config dir")

    def test_a_missing_channels_file_says_what_to_do(self):
        """t735 item 4: the bare FileNotFoundError sent the operator hunting."""
        with open(os.path.join(ROOT, "web", "app.py"), encoding="utf-8") as handle:
            source = handle.read()
        self.assertIn("VP_CHANNELS_FILE does not exist", source)
        self.assertIn("cp <checkout>/channels.json", source,
                      "the error must name the file to copy and where it goes")


if __name__ == "__main__":
    unittest.main()
