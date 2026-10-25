"""t142: readers must wait, not see raw 'database is locked'.

Live, right after the 2.70.35 restart: a plain inbox read raised
"database is locked (5)". Cause: inbox.db was journal_mode=DELETE, so writers block
readers, and the store now has several threads with one connection each. Fix: WAL plus
an explicit busy_timeout on every connection, so a reader arriving during a write waits
instead of raising.
"""

import os
import sqlite3
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_inbox  # noqa: E402


class LockingTests(unittest.TestCase):
    def test_the_inbox_opens_in_wal_with_a_busy_timeout(self):
        with tempfile.TemporaryDirectory() as tmp:
            inbox = dunk_inbox.Inbox(path=os.path.join(tmp, "inbox.db"),
                                     blob_dir=os.path.join(tmp, "blobs"))
            try:
                mode = inbox.db.execute("PRAGMA journal_mode").fetchone()[0]
                timeout = inbox.db.execute("PRAGMA busy_timeout").fetchone()[0]
                self.assertEqual("wal", str(mode).lower(),
                                 "writers block readers under DELETE")
                self.assertGreaterEqual(int(timeout), 1000,
                                        "a reader would raise instead of waiting")
            finally:
                inbox.close()

    def test_concurrent_readers_and_a_writer_never_see_locked(self):
        with tempfile.TemporaryDirectory() as tmp:
            inbox = dunk_inbox.Inbox(path=os.path.join(tmp, "inbox.db"),
                                     blob_dir=os.path.join(tmp, "blobs"))
            stop = threading.Event()
            errors = []

            def reader():
                while not stop.is_set():
                    try:
                        inbox.count("lane")
                        inbox.read("lane", limit=5, peek=True)
                    except Exception as error:      # noqa: BLE001 - that is the point
                        errors.append(f"{type(error).__name__}: {error}")
                        stop.set()

            def writer():
                n = 0
                while not stop.is_set():
                    n += 1
                    try:
                        inbox.send("lane", f"body {n}", sender="pm")
                    except Exception as error:      # noqa: BLE001
                        errors.append(f"{type(error).__name__}: {error}")
                        stop.set()

            threads = ([threading.Thread(target=reader, daemon=True) for _ in range(4)]
                       + [threading.Thread(target=writer, daemon=True) for _ in range(2)])
            for thread in threads:
                thread.start()
            deadline = time.time() + 5
            while time.time() < deadline and not stop.is_set():
                time.sleep(0.05)
            stop.set()
            for thread in threads:
                thread.join(timeout=5)
            try:
                self.assertEqual([], errors, "a lane saw a raw sqlite lock error")
            finally:
                inbox.close()


if __name__ == "__main__":
    unittest.main()
