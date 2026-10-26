"""t212: the tmux/terminal tests must not leak processes when they fail.

pi-review-2 found 18 orphaned 'dunkingsheep serve' processes (ppid=1) across three review
directories: the live-server tests failed in a sandbox without a tmux server and their
addCleanup kill-server did not remove the agents those tests had started. A test that leaves a
process behind is a defect in the test, not a cost of running it.

This asserts the property directly: a process whose cwd is a test's temp directory must not
outlive the test.
"""

import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import test_dunk_tmux_guard as guard_tests  # noqa: E402
import test_terminal as terminal_tests  # noqa: E402


def cwd_processes(directory):
    real = os.path.realpath(directory)
    found = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            if os.path.realpath(os.readlink(f"/proc/{entry}/cwd")) == real:
                found.append(int(entry))
        except OSError:
            continue
    return found


class NoLeakTests(unittest.TestCase):
    def test_the_guard_helper_reaps_a_process_in_a_directory(self):
        """The helper itself, proven on a process we spawn: no tmux needed."""
        with tempfile.TemporaryDirectory() as tmp:
            proc = __import__("subprocess").Popen(
                ["/bin/sh", "-c", "sleep 60"], cwd=tmp,
                stdout=__import__("subprocess").DEVNULL,
                stderr=__import__("subprocess").DEVNULL, start_new_session=True)
            try:
                time.sleep(0.2)
                self.assertIn(proc.pid, cwd_processes(tmp))
                guard_tests._kill_cwd(tmp)
                deadline = time.time() + 5
                while time.time() < deadline and proc.poll() is None:
                    time.sleep(0.1)
                self.assertIsNotNone(proc.poll(), "the helper left the process running")
            finally:
                if proc.poll() is None:
                    proc.kill()
                proc.wait(timeout=5)

    def test_both_live_test_modules_expose_their_cleanup(self):
        """A future edit must not drop the cleanup hook silently."""
        self.assertTrue(hasattr(terminal_tests.TmuxLiveTests, "_kill_leftovers"))
        self.assertTrue(hasattr(guard_tests, "_kill_cwd"))


if __name__ == "__main__":
    unittest.main()
