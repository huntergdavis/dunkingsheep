"""t212: killing the WATCHED parent must take the daemon's children too.

Reviewer's reproduction, kept in shape: a synthetic blocking `list-panes`-shaped child that spawns one
grandchild; SIGKILL the watched parent; then look for tagged descendants. The parent watch alone failed
it - the daemon was gone by 6 s while BOTH the tagged child and grandchild were alive at +6 s and +60 s,
the child reparented to init.

Three levels, because that is where the mechanisms live:

    spawner (the watched parent, SIGKILLed)
      -> session leader with PR_SET_PDEATHSIG (what the client installs for a test spawn)
           -> blocking child -> grandchild

SIGKILL cannot be handled, so the kernel's PDEATHSIG is what reaches the session leader, and the
leader's process-group kill (dunk_server.kill_own_process_group, the shipped helper) is what reaches
the children. Killing the leader itself would prove nothing.
"""

import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

LEADER = """\
import os, signal, subprocess, sys, time
sys.path.insert(0, {root!r})
import dunk_server
os.environ["DUNKINGSHEEP_TEST_SPAWN"] = "1"
signal.signal(signal.SIGTERM, lambda *_: (dunk_server.kill_own_process_group("test"), os._exit(0)))
subprocess.Popen(["/bin/sh", "-c", {child!r}])
time.sleep(120)
"""

SPAWNER = """\
import ctypes, os, subprocess, sys, time
def pdeathsig():
    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    libc.prctl(1, 15)          # PR_SET_PDEATHSIG, SIGTERM
proc = subprocess.Popen([sys.executable, "-c", {leader!r}], start_new_session=True,
                        preexec_fn=pdeathsig, env=dict(os.environ, DUNKINGSHEEP_TEST_SPAWN="1"),
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print(proc.pid, flush=True)
time.sleep(120)
"""


def _cmdline(pid):
    try:
        return open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\0", b" ").decode()
    except OSError:
        return ""


class ProcessGroupTests(unittest.TestCase):
    def test_tagged_children_die_when_the_watched_parent_is_sigkilled(self):
        tmp = tempfile.mkdtemp(prefix="pg-")
        # A copy of sleep whose argv[0] carries the temp dir: a survivor must be identifiable, or
        # "sleep 600" says nothing about where it came from.
        subprocess.run(["cp", "/bin/sleep", os.path.join(tmp, "tagged-sleep")], check=True)
        leader_code = LEADER.format(root=ROOT,
                                    child=tmp + "/tagged-sleep 600 & wait")
        spawner_code = SPAWNER.format(leader=leader_code)
        watched = subprocess.Popen([sys.executable, "-c", spawner_code],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self._kill_tagged, tmp, watched.pid)
        leader_pid = int(watched.stdout.readline().split()[0])

        deadline = time.time() + 15
        tagged = []
        while time.time() < deadline:
            tagged = [p for p in os.listdir("/proc") if p.isdigit() and tmp in _cmdline(p)]
            if len(tagged) >= 2:
                break
            time.sleep(0.3)
        self.assertGreaterEqual(len(tagged), 2,
                                f"fixture never reached child+grandchild: {tagged}")

        os.kill(watched.pid, signal.SIGKILL)      # the watched parent: unhandleable
        watched.wait(timeout=10)

        for wait in (6, 60):
            time.sleep(wait if wait == 6 else wait - 6)
            survivors = [(p, _cmdline(p)[:70]) for p in os.listdir("/proc")
                         if p.isdigit() and tmp in _cmdline(p)]
            self.assertEqual([], survivors,
                             f"tagged descendants still alive at +{wait}s: {survivors}")

    @staticmethod
    def _kill_tagged(tmp, spawner_pid):
        for pid in os.listdir("/proc"):
            if pid.isdigit() and tmp in _cmdline(pid):
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except OSError:
                    pass
        try:
            os.killpg(os.getpgid(spawner_pid), signal.SIGKILL)
        except OSError:
            pass


if __name__ == "__main__":
    unittest.main()
