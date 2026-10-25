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


class SpawnRecordTests(unittest.TestCase):
  def test_a_spawned_daemon_is_recorded_and_reaped(self):
      tmp = os.path.join(_bootstrap._TMP, "sweep-case")
      os.makedirs(tmp, exist_ok=True)
      sock = os.path.join(tmp, "ds.sock")
      env = dict(os.environ, DUNKINGSHEEP_DIR=tmp, DUNKINGSHEEP_SOCKET=sock)
      proc = subprocess.Popen(  # noqa: S603 - the point is to spawn one
          [sys.executable, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                        "dunkingsheep"), "serve"],
          env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
          start_new_session=True)
      pidfile = _bootstrap._PIDFILE
      assert os.path.exists(pidfile), "the spawn must leave a pidfile record"
      with open(pidfile, encoding="utf-8") as handle:
          recorded = [int(line.split("\t", 1)[0]) for line in handle if line.strip()]
      assert proc.pid in recorded, f"{proc.pid} not in {recorded}"

      _bootstrap._sweep_recorded_daemons()
      deadline = time.time() + 5
      while time.time() < deadline:
          if proc.poll() is not None:
              break
          time.sleep(0.1)
      assert proc.poll() is not None, "the recorded daemon must be gone after the sweep"
      proc.wait(timeout=5)