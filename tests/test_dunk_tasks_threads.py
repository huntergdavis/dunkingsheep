"""t93 root cause: a sqlite3 connection is not safe to share between threads.

The daemon logged "InterfaceError: bad parameter or other API misuse" and
`dunk_tasks.leases_for` once saw rows as plain tuples (IndexError: tuple index out of
range) although row_factory was set. Both are the same defect: one connection used by
several threads at once, with several paths (list, stats, metrics, leases_for_pane,
_record, _next_id) that never took the store's lock.

The store now keeps ONE CONNECTION PER THREAD. This test hammers the same operations the
daemon does from twelve threads and fails if any of them raises.
"""

import os
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_tasks import Tasks  # noqa: E402


class ThreadedConnectionTests(unittest.TestCase):
    def test_twelve_threads_do_not_corrupt_each_others_reads(self):
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                task = tasks.add("global", "probe", priority="high")
                now = time.time()
                tasks.db.execute(
                    "INSERT INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at,"
                    " expires_at) VALUES (?,?,?,?,?)",
                    (task["id"], "%4", now, now, now + 600))
                tasks.db.commit()

                stop = threading.Event()
                errors = []

                def hammer(kind):
                    while not stop.is_set():
                        try:
                            if kind == 0:
                                tasks.leases_for(task["id"])
                            elif kind == 1:
                                tasks.list(scope="global")
                            elif kind == 2:
                                tasks.for_pane("%4")
                            elif kind == 3:
                                tasks.stats()
                            else:
                                tasks.heartbeat(task["id"], "%4")
                        except Exception as error:      # noqa: BLE001 - that is the point
                            errors.append(f"{type(error).__name__}: {error}")
                            stop.set()

                threads = [threading.Thread(target=hammer, args=(i % 5,), daemon=True)
                           for i in range(12)]
                for thread in threads:
                    thread.start()
                deadline = time.time() + 5
                while time.time() < deadline and not stop.is_set():
                    time.sleep(0.05)
                stop.set()
                for thread in threads:
                    thread.join(timeout=2)
                self.assertEqual([], errors,
                                 "a shared connection corrupted a read under threads")
            finally:
                tasks.close()

    def test_each_thread_gets_its_own_connection(self):
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                seen = {}
                held = []          # keep the connections ALIVE: ids of freed objects are
                                   # recycled, so recording id() alone compares addresses,
                                   # not identity (the PR's own docstring warns about it)
                def record():
                    connection = tasks.db
                    held.append(connection)
                    seen[threading.get_ident()] = id(connection)
                threads = [threading.Thread(target=record) for _ in range(3)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join()
                seen[threading.get_ident()] = id(tasks.db)
                self.assertEqual(len(seen), len(set(seen.values())),
                                 "two threads shared one connection")
            finally:
                tasks.close()


if __name__ == "__main__":
    unittest.main()
