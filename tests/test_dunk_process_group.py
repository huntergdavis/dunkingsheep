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
import shutil
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


CONTROL_LEADER = """\
import os, subprocess, sys, time
# PRE-FIX SHAPE: a plain session leader - no PDEATHSIG, no die-with-parent, no group kill.
subprocess.Popen(["/bin/sh", "-c", {child!r}])
time.sleep(120)
"""


class ProcessGroupTests(unittest.TestCase):
    def test_the_fixture_discriminates_old_code_leaves_survivors(self):
        """THE CONTROL, pinned as a test rather than as a message (CoS i29180).

        Measured 2026-10-27 by hand: pre-fix code leaves THREE tagged survivors at +6 s - the leader,
        the sh child and the grandchild. If this control ever passes with zero survivors, the fixture
        has stopped reproducing the leak and every other result in this file is meaningless.
        """
        tmp = tempfile.mkdtemp(prefix="pgctl-")
        self.addCleanup(shutil.rmtree, tmp, True)   # a suite that grows a dir per run is the same
                                                    # untidiness t212 is about (%20's note)
        subprocess.run(["cp", "/bin/sleep", os.path.join(tmp, "tagged-sleep")], check=True)
        leader_code = CONTROL_LEADER.format(child=tmp + "/tagged-sleep 600 & wait")
        spawner_code = SPAWNER.format(leader=leader_code)
        watched = subprocess.Popen([sys.executable, "-c", spawner_code],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(self._kill_tagged, tmp, watched.pid)
        watched.stdout.readline()
        time.sleep(3)
        os.kill(watched.pid, signal.SIGKILL)
        watched.wait(timeout=10)
        time.sleep(6)
        survivors = [(p, _cmdline(p)[:60]) for p in os.listdir("/proc")
                     if p.isdigit() and tmp in _cmdline(p)]
        self.assertGreaterEqual(len(survivors), 2,
                                f"the fixture no longer reproduces the leak - only {survivors} survived "
                                "old code, so it cannot protect the fix")

    def test_tagged_children_die_when_the_watched_parent_is_sigkilled(self):
        tmp = tempfile.mkdtemp(prefix="pg-")
        self.addCleanup(shutil.rmtree, tmp, True)
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

        # GATE ON THE GRANDCHILD (reviewer, PR #48). The previous gate - "two processes whose argv
        # embeds the temp dir" - was satisfied INSTANTLY by the spawner and the leader themselves, both
        # of which carry the whole `python -c` code in argv. The SIGKILL then landed before the sh child
        # was ever forked, so nothing could survive and the assertion passed with the helper
        # neutralised. Wait for the tagged-sleep binary itself.
        sleep_path = os.path.join(tmp, "tagged-sleep")
        deadline = time.time() + 20
        while time.time() < deadline:
            if any(_cmdline(p).startswith(sleep_path)
                   for p in os.listdir("/proc") if p.isdigit()):
                break
            time.sleep(0.2)
        else:
            # DISTINGUISH "broken fixture" FROM "slow box" (CoS i29225): a phantom red sends the next
            # reader hunting a leak that is not there. If the binary is missing the fixture is broken;
            # if it is present but never ran, the box was slow and the deadline is the suspect.
            if not os.path.exists(sleep_path):
                self.fail(f"FIXTURE BROKEN: {sleep_path} does not exist, so nothing could ever run")
            self.fail(
                "SLOW BOX, not a leak: the tagged child never started within 20 s although its binary "
                f"exists ({sleep_path}). Re-run; if it persists, raise the deadline - this is a red "
                "about timing, not about surviving processes.")

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


class ClientTestSpawnGateTests(unittest.TestCase):
    def test_no_preexec_when_the_forker_is_not_the_watched_pid(self):
        """THE POSITIVE HALF (reviewer %20, PR #51): the d456781 bug needed TEST_SPAWN SET and the forker
        being someone other than the watched pid. A probe that only runs with TEST_SPAWN unset cannot see
        it - the arming mutant survives the whole suite without this assertion."""
        tmp = tempfile.mkdtemp(prefix="gate2-")
        self.addCleanup(shutil.rmtree, tmp, True)
        probe = f'''
import json, os, sys
sys.path.insert(0, {ROOT!r})
import dunk_client
seen = {{}}
def fake_popen(*args, **kwargs):
    seen["preexec"] = kwargs.get("preexec_fn")
    class Done:
        pid = 0
    return Done()
dunk_client.subprocess.Popen = fake_popen
client = dunk_client.FlockClient(socket_path=os.path.join({tmp!r}, "ds.sock"), self_pane_id="%99")
try:
    client.start_daemon()
except Exception:
    pass
print(json.dumps({{"preexec": "FUNCTION" if seen.get("preexec") else None}}))
'''
        env = dict(os.environ)
        env["DUNKINGSHEEP_TEST_SPAWN"] = "1"
        # a watched pid that is NOT this process, which is the d456781 shape
        env["DUNKINGSHEEP_DIE_WITH_PARENT"] = "1"
        env["DUNKINGSHEEP_TMUX_SOCKET"] = env.get("DUNKINGSHEEP_TMUX_SOCKET") or "default"
        result = subprocess.run([sys.executable, "-c", probe], capture_output=True, text=True,
                                timeout=60, env=env, cwd=ROOT)
        self.assertEqual(0, result.returncode, result.stderr)
        import json as _json
        seen = _json.loads(result.stdout.strip().splitlines()[-1])
        self.assertIsNone(seen.get("preexec"),
                          "PR_SET_PDEATHSIG must not be armed when the forker is not the watched pid")

    """The CLIENT's gate, not the helper's (reviewer %20, PR #48).

    dunk_client.start_daemon decides whether to pass DUNKINGSHEEP_DIE_WITH_PARENT and whether to arm
    PR_SET_PDEATHSIG. That decision is what went wrong on d456781 - the prctl was armed against a
    short-lived CLI call - and ProductionNoOpTests pins a different gate in a different file.
    """

    def test_the_client_passes_no_parent_binding_without_test_spawn(self):
        """Behavioural: spawn the client's daemon path with TEST_SPAWN unset and read the child env."""
        tmp = tempfile.mkdtemp(prefix="gate-")
        self.addCleanup(shutil.rmtree, tmp, True)
        probe = f'''
import json, os, sys
sys.path.insert(0, {ROOT!r})
import dunk_client
seen = {{}}
real_popen = dunk_client.subprocess.Popen
def fake_popen(*args, **kwargs):
    seen["env"] = {{k: v for k, v in (kwargs.get("env") or {{}}).items()
                   if k.startswith("DUNKINGSHEEP_")}}
    seen["preexec"] = kwargs.get("preexec_fn")
    class Done:
        pid = 0
    return Done()
dunk_client.subprocess.Popen = fake_popen
client = dunk_client.FlockClient(socket_path=os.path.join({tmp!r}, "ds.sock"), self_pane_id="%99")
try:
    client.start_daemon()
except Exception:
    pass                      # the point is the env it WOULD have passed
print(json.dumps(seen))
'''
        env = dict(os.environ)
        env.pop("DUNKINGSHEEP_TEST_SPAWN", None)
        env.pop("DUNKINGSHEEP_DIE_WITH_PARENT", None)
        result = subprocess.run([sys.executable, "-c", probe], capture_output=True, text=True,
                                timeout=60, env=env, cwd=ROOT)
        self.assertEqual(0, result.returncode, result.stderr)
        import json as _json
        seen = _json.loads(result.stdout.strip().splitlines()[-1])
        self.assertNotIn("DUNKINGSHEEP_DIE_WITH_PARENT", seen.get("env") or {},
                         "a normal spawn must not be bound to its caller's life")
        self.assertIsNone(seen.get("preexec"),
                          "a normal spawn must not arm PR_SET_PDEATHSIG against the caller")

if __name__ == "__main__":
    unittest.main()
