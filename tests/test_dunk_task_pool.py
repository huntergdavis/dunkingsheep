"""The task store's connections must be bounded by LIVE threads, not by history.

Measured 2026-10-25/26: 5,478 open handles on tasks.db with nine live threads, ~11 per
minute, and the Errno 24 that broke 'send-keys'. The cause was a per-thread connection that
was never closed when its (short-lived) thread exited: the daemon runs an outbox worker, a
push worker and a reaper per pane.

The pool is pruned on every access, so the bound is the number of live threads.
"""

import os
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_tasks import Tasks  # noqa: E402


class BoundedConnectionTests(unittest.TestCase):
    def test_short_lived_threads_do_not_accumulate_connections(self):
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                tasks.add("global", "probe", priority="high")

                def work():
                    tasks.list(scope="global")

                for _ in range(50):
                    thread = threading.Thread(target=work)
                    thread.start()
                    thread.join()
                # Assert on the POOL, not on connection_count(): a counter that prunes
                # as a side effect would make a leaking pool look clean exactly when it
                # is measured (reviewer i27680).
                tasks.list(scope="global")
                self.assertLessEqual(len(tasks._connections), 2,
                                     f"{len(tasks._connections)} connections after 50 "
                                     "short-lived threads")
            finally:
                tasks.close()

    def test_a_burst_then_quiet_workload_does_not_hold_the_tail(self):
        """The reviewer's failing case: three threads run and join, then three ordinary
        reads. On the first head the pool kept all four connections because pruning only
        ran when a NEW thread opened its first connection."""
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                tasks.add("global", "probe")

                def work():
                    tasks.list(scope="global")

                threads = [threading.Thread(target=work) for _ in range(3)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join()
                for _ in range(3):
                    tasks.list(scope="global")
                self.assertLessEqual(len(tasks._connections), 1,
                                     f"burst-then-quiet kept {len(tasks._connections)} "
                                     "connections for dead threads")
            finally:
                tasks.close()

    def test_each_live_thread_still_gets_its_own_connection(self):
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                seen = {}
                barrier = threading.Barrier(3, timeout=10)

                def work():
                    barrier.wait()
                    seen[threading.get_ident()] = id(tasks.db)
                    tasks.list(scope="global")
                    barrier.wait()

                threads = [threading.Thread(target=work) for _ in range(3)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join()
                self.assertEqual(3, len(set(seen.values())),
                                 "live threads must not share one connection")
            finally:
                tasks.close()

    def test_the_counter_does_not_prune(self):
        """Measuring must not repair the defect being measured."""
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                def work():
                    tasks.list(scope="global")
                for _ in range(5):
                    thread = threading.Thread(target=work)
                    thread.start()
                    thread.join()
                before = tasks.connection_count()
                self.assertGreaterEqual(before, 2,
                                        "connection_count() pruned as a side effect")
            finally:
                tasks.close()

    def test_closing_the_store_closes_every_connection(self):
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            tasks.add("global", "probe")
            tasks.list(scope="global")
            tasks.close()
            self.assertEqual(0, tasks.connection_count())


if __name__ == "__main__":
    unittest.main()
