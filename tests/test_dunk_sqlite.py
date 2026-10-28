"""WAL readers and writers use real temporary databases, never live state."""
import concurrent.futures
import sqlite3
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from tests import _bootstrap  # noqa: F401
from dunk_inbox import Inbox
from dunk_sqlite import BUSY_TIMEOUT_MS, connect


class SqlitePolicyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "inbox.db"
        self.inbox = Inbox(str(self.path), blob_dir=str(Path(self.tmp.name) / "blobs"))
        self.addCleanup(self.inbox.close)

    def test_open_sets_wal_and_timeout_and_readonly_does_not_create(self):
        self.assertEqual("wal", self.inbox.db.execute("PRAGMA journal_mode").fetchone()[0])
        self.assertEqual(BUSY_TIMEOUT_MS, self.inbox.db.execute("PRAGMA busy_timeout").fetchone()[0])
        reader = connect(self.path, readonly=True)
        try:
            self.assertEqual(BUSY_TIMEOUT_MS, reader.execute("PRAGMA busy_timeout").fetchone()[0])
            with self.assertRaises(sqlite3.OperationalError):
                reader.execute("CREATE TABLE forbidden(x)")
        finally:
            reader.close()
        missing = Path(self.tmp.name) / "missing.db"
        with self.assertRaises(sqlite3.OperationalError):
            connect(missing, readonly=True)
        self.assertFalse(missing.exists())
        with self.assertRaises(ValueError):
            connect(str(self.path) + "?mode=ro")
        self.assertFalse(Path(str(self.path) + "?mode=ro").exists())

    def test_readers_complete_while_writer_holds_transaction(self):
        self.inbox.send("lane", "before transaction")
        self.inbox.db.execute("BEGIN EXCLUSIVE")
        self.inbox.db.execute("UPDATE messages SET body='uncommitted'")
        def read():
            conn = connect(self.path, readonly=True)
            try:
                return conn.execute("SELECT body FROM messages").fetchone()[0]
            finally:
                conn.close()
        try:
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                futures = [pool.submit(read) for _ in range(4)]
                self.assertEqual(["before transaction"] * 4,
                                 [f.result(timeout=2) for f in futures])
        finally:
            self.inbox.db.rollback()

    def test_concurrent_reader_writer_hammer_has_no_locked_errors(self):
        barrier = threading.Barrier(6)
        def worker(writer):
            conn = connect(self.path, readonly=not writer)
            try:
                barrier.wait(timeout=5)
                for _ in range(100):
                    if writer:
                        conn.execute("INSERT INTO counters(name,value) VALUES('hammer',1) "
                                     "ON CONFLICT(name) DO UPDATE SET value=value+1")
                        conn.commit()
                    else:
                        conn.execute("SELECT COUNT(*) FROM messages").fetchone()
                return 100
            finally:
                conn.close()
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
            results = list(pool.map(worker, [True, True, False, False, False, False]))
        self.assertEqual(600, sum(results))
        self.assertEqual(200, self.inbox.db.execute(
            "SELECT value FROM counters WHERE name='hammer'").fetchone()[0])

    def test_socket_reports_exhausted_lock_wait_as_retryable(self):
        from dunk_server import FlockServer
        server = object.__new__(FlockServer)
        server.flock = Mock()
        for code in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED):
            error = sqlite3.OperationalError("database is locked")
            error.sqlite_errorcode = code
            with patch("dunk_server.dispatch", side_effect=error):
                reply, stop = server.handle_line('{"id": 7, "cmd": "inbox_count"}')
            self.assertFalse(stop)
            self.assertEqual({"id": 7, "ok": False, "retryable": True,
                              "error": "database busy; retry the request"}, reply)
        self.assertEqual(2, server.flock.clear_caller.call_count)


if __name__ == "__main__":
    unittest.main()
