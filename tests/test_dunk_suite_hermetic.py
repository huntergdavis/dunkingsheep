"""t247: the suite owns its state directory, whatever the caller's environment says.

A full run aborted once with dunk_tasks.TaskError "alive_panes must be tagged alive_source='live'"
- the guard doing its job, because the run was reading the REAL tasks database. It was not
reproducible because it depended on the operator's environment: tests/_bootstrap.py used
setdefault(), so an ambient DUNKINGSHEEP_DIR won.

These tests run a child process, because the behaviour happens at import time.
"""

import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def _child(env_extra, code):
    env = dict(os.environ)
    env.pop("DUNKINGSHEEP_TESTS_OWN_ENV", None)
    env.update(env_extra)
    return subprocess.run([sys.executable, "-c", code], cwd=ROOT, env=env,
                          capture_output=True, text=True, timeout=120)


class HermeticStateTests(unittest.TestCase):
    def test_an_ambient_state_dir_is_overridden_and_reported(self):
        code = ("import sys; sys.path.insert(0, 'tests'); import _bootstrap, dunk_tasks; "
                "print('DIR=' + __import__('os').environ['DUNKINGSHEEP_DIR']); "
                "print('DB=' + dunk_tasks.TASKS_DB)")
        result = _child({"DUNKINGSHEEP_DIR": "/tmp/definitely-not-the-live-config"}, code)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("ignoring ambient DUNKINGSHEEP_DIR", result.stderr,
                      "an override must be visible, not silent")
        self.assertNotIn("/tmp/definitely-not-the-live-config", result.stdout,
                         "the suite must not adopt the caller's state dir")
        self.assertIn("dunkingsheep-tests-", result.stdout)

    def test_the_tasks_database_lands_in_the_temp_dir(self):
        code = ("import sys; sys.path.insert(0, 'tests'); import _bootstrap, dunk_tasks; "
                "print(dunk_tasks.TASKS_DB)")
        result = _child({"DUNKINGSHEEP_DIR": "/home/hunter/.config/dunkingsheep"}, code)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("dunkingsheep-tests-", result.stdout)
        self.assertNotIn("/.config/dunkingsheep/tasks.db", result.stdout,
                         "a suite run must never open the live tasks database")

    def test_the_deliberate_opt_out_still_works(self):
        code = ("import sys; sys.path.insert(0, 'tests'); import _bootstrap, os; "
                "print(os.environ['DUNKINGSHEEP_DIR'])")
        result = _child({"DUNKINGSHEEP_DIR": "/tmp/my-own-scratch",
                         "DUNKINGSHEEP_TESTS_OWN_ENV": "1"}, code)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("/tmp/my-own-scratch", result.stdout)

    def test_the_reaper_still_refuses_a_fixture_pane_list(self):
        """The guard that fired is right; the fix is that a suite run no longer reaches it."""
        code = ("import sys; sys.path.insert(0, 'tests'); sys.path.insert(0, '.'); "
                "import _bootstrap; "
                "from dunk_core import Flock, DunkError; from fakes import FakeHerdr; "
                "f = Flock(herdr=FakeHerdr(), persist=False, restore=False); "
                "f.tasks.add('global', 'fixture reap'); "
                "raised = False\n"
                "try:\n"
                "    f.task_reap(self_pane_id='w1:p2')\n"
                "except DunkError as error:\n"
                "    raised = 'alive_source' in str(error) or 'fixture' in str(error)\n"
                "f.close(); print('RAISED' if raised else 'NOT-RAISED')")
        result = _child({"DUNKINGSHEEP_DIR": "/home/hunter/.config/dunkingsheep"}, code)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("RAISED", result.stdout)


if __name__ == "__main__":
    unittest.main()
