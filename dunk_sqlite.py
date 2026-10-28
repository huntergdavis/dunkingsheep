"""Consistent SQLite connections for daemon stores and read-only consumers."""
from pathlib import Path
import re
import sqlite3

BUSY_TIMEOUT_MS = 5000


def connect(path, *, readonly=False, check_same_thread=True, timeout=BUSY_TIMEOUT_MS / 1000):
    """Use WAL for owned stores; never change journal mode on external reads.

    Accept filesystem paths, not hand-built URI strings. Path.as_uri() escapes
    spaces, '?' and '#' correctly, and readonly=True cannot create a missing DB.
    """
    path = str(path)
    if path.startswith("file:") or re.search(r"\?mode=(?:ro|rw|rwc)(?:&|$)", path):
        raise ValueError("SQLite connection requires a filesystem path; use readonly=True for reads")
    database = Path(path).resolve().as_uri() + "?mode=ro" if readonly else path
    conn = sqlite3.connect(database, uri=readonly, timeout=timeout,
                           check_same_thread=check_same_thread)
    try:
        conn.execute(f"PRAGMA busy_timeout = {int(timeout * 1000)}")
        if readonly:
            conn.execute("PRAGMA query_only = ON")
        elif path != ":memory:":
            conn.execute("PRAGMA journal_mode = WAL")
        return conn
    except Exception:
        conn.close()
        raise
