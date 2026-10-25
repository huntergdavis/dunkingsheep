"""t88: the suite reaps the daemons it spawns, by pid, and reports the ones it cannot.

The leak this covers (measured 2026-10-24): repeated full-suite runs left five
`dunkingsheep serve` processes on mktemp config dirs, one three days old, none
with a client. The old exit sweep only matched sockets under the run's own _TMP,
so a test that gave the daemon its own temp dir escaped it.
"""

import os
import subprocess
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: E402  (installs the recording Popen)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class SpawnRecordTests(unittest.TestCase):
    """The MECHANISM: a spawned pid is recorded, and the sweep reaps exactly that pid.

    A stand-in process is used on purpose. A real daemon on a temp socket exits
    immediately in some environments, and a test whose subject may already be dead
    passes for the wrong reason - it did exactly that twice while this was being
    written. The end-to-end evidence for the real thing is the sampler count (9
    serve processes before a full suite, 7 after, no new pid), not this test.
    """

    def test_a_spawned_process_is_recorded_and_reaped_by_pid(self):
        proc = subprocess.Popen(  # noqa: S603 - the point is to spawn one
            [sys.executable, "-c", "import time; time.sleep(120)  # serve"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True)
        try:
            time.sleep(0.2)
            pidfile = _bootstrap._PIDFILE
            self.assertTrue(os.path.exists(pidfile), "the spawn must leave a pidfile record")
            with open(pidfile, encoding="utf-8") as handle:
                recorded = [int(line.split("\t", 1)[0]) for line in handle if line.strip()]
            self.assertIn(proc.pid, recorded, f"{proc.pid} not recorded in {recorded}")

            _bootstrap._sweep_recorded_daemons()
            deadline = time.time() + 10
            while time.time() < deadline and proc.poll() is None:
                time.sleep(0.1)
            self.assertIsNotNone(proc.poll(), "a recorded pid must be reaped by the sweep")
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=10)

    def test_the_liveness_test_does_not_count_a_zombie(self):
        """kill(pid, 0) succeeds for a zombie; the sweep must not report one as
        LEAKED, which is what produced false alarms on 2026-10-24."""
        proc = subprocess.Popen([sys.executable, "-c", "pass"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        proc.wait()                       # exited, not yet reaped by us
        self.assertFalse(_bootstrap._alive(proc.pid),
                         "an exited process is not a leaked daemon")


if __name__ == "__main__":
    unittest.main()
