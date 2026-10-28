"""t212: a test-spawned daemon must not outlive the process that spawned it.

Measured 2026-10-27: 121 `dunkingsheep serve` processes at ppid=1, cwds under test worktrees, load1 65.7
on 8 CPUs. The suite's exit sweep cannot run when a run is killed, times out or crashes, so the sweep is
not the guarantee - dying with the parent is.
"""

import os
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI = os.path.join(ROOT, "dunkingsheep")


def _alive(pid):
    try:
        with open(f"/proc/{pid}/stat", "rb") as handle:
            state = handle.read().rsplit(b")", 1)[1].split()[0]
        return state not in (b"Z", b"X", b"x")
    except OSError:
        return False


class DieWithParentTests(unittest.TestCase):
    def test_a_test_spawned_daemon_exits_when_its_parent_dies(self):
        """The spawning process is a short-lived python that starts the daemon and then exits."""
        tmp = tempfile.mkdtemp(prefix="dieparent-")
        spawner = f'''
import os, subprocess, sys, time
env = dict(os.environ, DUNKINGSHEEP_TEST_SPAWN="1",
           DUNKINGSHEEP_DIE_WITH_PARENT=str(os.getpid()),   # watch the SPAWNER, not the suite
           DUNKINGSHEEP_DIR={tmp!r}, DUNKINGSHEEP_SOCKET={tmp + "/ds.sock"!r},
           DUNKINGSHEEP_TMUX_SOCKET="default")
proc = subprocess.Popen([sys.executable, {CLI!r}, "serve"], env=env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                        start_new_session=True)
print(proc.pid, flush=True)
time.sleep(1.5)
'''
        result = subprocess.run([sys.executable, "-c", spawner], capture_output=True, text=True, timeout=60)
        self.assertEqual(0, result.returncode, result.stderr)
        daemon_pid = int(result.stdout.split()[0])
        # NEVER LEAVE A STRAY BEHIND, even when the assertion fails: a failing assertion raises before
        # any later cleanup line, which is how three of these leaked while I was testing this very fix.
        self.addCleanup(self._kill_if_alive, daemon_pid)
        # The spawner has exited; the daemon must notice and go away on its own.
        deadline = time.time() + 15
        while time.time() < deadline and _alive(daemon_pid):
            time.sleep(0.5)
        self.assertFalse(_alive(daemon_pid),
                         f"daemon {daemon_pid} outlived its parent - that is the t212 leak")

    @staticmethod
    def _kill_if_alive(pid):
        if _alive(pid):
            try:
                os.kill(pid, 9)
            except OSError:
                pass

    def test_no_grandchildren_survive_the_parent(self):
        """The CoS measured the amplifier: 218 guard processes forking `tmux list-panes -a`, load 78 ->
        13 when killed. A suite that exits cleanly while its daemons' grandchildren keep polling is
        still leaking, so the proof has to look one level down."""
        tmp = tempfile.mkdtemp(prefix="dieparent-gc-")
        spawner = f'''
import os, subprocess, sys, time
env = dict(os.environ, DUNKINGSHEEP_TEST_SPAWN="1",
           DUNKINGSHEEP_DIE_WITH_PARENT=str(os.getpid()),
           DUNKINGSHEEP_DIR={tmp!r}, DUNKINGSHEEP_SOCKET={tmp + "/ds.sock"!r},
           DUNKINGSHEEP_TMUX_SOCKET="default")
proc = subprocess.Popen([sys.executable, {CLI!r}, "serve"], env=env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                        start_new_session=True)
print(proc.pid, flush=True)
time.sleep(2.0)
'''
        result = subprocess.run([sys.executable, "-c", spawner], capture_output=True, text=True, timeout=60)
        self.assertEqual(0, result.returncode, result.stderr)
        daemon_pid = int(result.stdout.split()[0])
        self.addCleanup(self._kill_if_alive, daemon_pid)
        deadline = time.time() + 15
        while time.time() < deadline and _alive(daemon_pid):
            time.sleep(0.5)
        # One level down: anything still running whose ancestry leads to the dead daemon.
        leftovers = []
        for pid in os.listdir("/proc"):
            if not pid.isdigit():
                continue
            try:
                cmd = open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\0", b" ").decode()
                ppid = int(open(f"/proc/{pid}/stat", "rb").read().rsplit(b")", 1)[1].split()[1])
            except (OSError, ValueError, IndexError):
                continue
            if ppid == daemon_pid or "list-panes" in cmd and tmp in cmd:
                leftovers.append((pid, cmd[:70]))
        self.assertEqual([], leftovers,
                         f"the daemon died but its children/grandchildren did not: {leftovers}")

    def test_a_normal_spawn_is_not_tied_to_its_caller(self):
        """The guard must be opt-in: a lane's auto-started daemon outlives the CLI call."""
        source = open(os.path.join(ROOT, "dunk_client.py")).read()
        self.assertIn('DUNKINGSHEEP_TEST_SPAWN") == "1"', source,
                      "the parent-death env must be conditional, not always set")


if __name__ == "__main__":
    unittest.main()
