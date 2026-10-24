"""Direct owner delivery: the message is in front of the agent, nothing is typed.

The owner waited seven minutes for a message because it was written to an inbox and
then typed into a pane behind a typing guard. He said the obvious thing: there was never
a need to wait for agent input to clear - a message can be pushed and read.

So this is a PUSH channel with no inbox row and no keystrokes:

    owner_push(target, body)   -> stores the message for a pane, returns its id
    owner_take(pane)           -> the harness extension drains what is waiting

The harness side (pi's extension) polls `owner_take` about once a second and injects
what it finds into the conversation. Nothing is typed, so no guard can hold it, and the
send path cannot fail silently: an undrained message is visible as a row with
`taken_at IS NULL`.

Scope is the owner -> CoS path only. Other agents keep the inbox, because the inbox is
what gives them history, attachments and acks.
"""
import os
import sqlite3
import threading
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))
DB_PATH = os.path.join(CONFIG_DIR, "owner-push.db")

SCHEMA = """
CREATE TABLE IF NOT EXISTS owner_push (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pane_id TEXT NOT NULL,
    target TEXT,
    subject TEXT,
    body TEXT NOT NULL,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT
);
CREATE INDEX IF NOT EXISTS owner_push_pane ON owner_push(pane_id, taken_at);

-- A DRAIN HEARTBEAT, separate from the messages. A take proves a message was read
-- once; it does not prove a poller is running now (a manual `owner take` would
-- confer "healthy" for ever) and an idle poller draining EMPTY queues writes no
-- row at all (so a healthy lane would look dead). The extension records every
-- successful poll here, empty or not, and that is what capability reads.
CREATE TABLE IF NOT EXISTS owner_poll (
    pane_id   TEXT NOT NULL,
    at        REAL NOT NULL,
    by        TEXT,
    PRIMARY KEY (pane_id)
);
"""


class OwnerPush:
    def __init__(self, path=DB_PATH):
        self.path = path
        self._lock = threading.RLock()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        self.db = sqlite3.connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            self.db.commit()

    def close(self):
        """Close the connection. A caller that opens a store per query must be able
        to hand the handle back; without this a capability probe leaked one per
        fleet row per refresh (independent review, 2026-10-24)."""
        with self._lock:
            try:
                self.db.close()
            except Exception:                    # noqa: BLE001 - closing twice is fine
                pass

    def push(self, pane_id, body, *, target=None, subject=None, now=None):
        body = (body or "").strip()
        if not body:
            raise ValueError("an owner message needs a body")
        now = now or time.time()
        with self._lock:
            cursor = self.db.execute(
                "INSERT INTO owner_push (pane_id, target, subject, body, created_at) "
                "VALUES (?,?,?,?,?)", (pane_id, target, subject, body, now))
            self.db.commit()
            return {"id": cursor.lastrowid, "pane_id": pane_id, "at": now}

    def take(self, pane_id, *, by=None, limit=20, now=None):
        """Hand over what is waiting for this pane, oldest first."""
        now = now or time.time()
        with self._lock:
            rows = [dict(row) for row in self.db.execute(
                "SELECT * FROM owner_push WHERE pane_id = ? AND taken_at IS NULL "
                "ORDER BY id LIMIT ?", (pane_id, int(limit)))]
            for row in rows:
                self.db.execute("UPDATE owner_push SET taken_at = ?, taken_by = ? WHERE id = ?",
                                (now, by or pane_id, row["id"]))
            self.db.commit()
        return rows

    def poll(self, pane_id, by=None, now=None):
        """Record a successful poll by the lane's own harness (empty polls count)."""
        now = now or time.time()
        with self._lock:
            self.db.execute(
                "INSERT INTO owner_poll (pane_id, at, by) VALUES (?,?,?) "
                "ON CONFLICT(pane_id) DO UPDATE SET at=excluded.at, by=excluded.by",
                (pane_id, now, by or "harness"))
            self.db.commit()
        return {"pane_id": pane_id, "at": now, "by": by or "harness"}

    def last_poll(self, pane_id):
        """When this pane's harness last polled, or None. THE drain fact."""
        with self._lock:
            row = self.db.execute(
                "SELECT at FROM owner_poll WHERE pane_id = ?", (pane_id,)).fetchone()
        return row["at"] if row is not None and row["at"] else None

    def last_take(self, pane_id):
        """When this pane last drained a push, or None. The OBSERVED drain fact.

        A harness name is not evidence: %2 is pi and still went blind for a day
        because its session started before the extension existed and never loaded
        it (measured 2026-10-24). Only a take proves a drainer is running.
        """
        with self._lock:
            row = self.db.execute(
                "SELECT MAX(taken_at) AS at FROM owner_push WHERE pane_id = ?",
                (pane_id,)).fetchone()
        return (row["at"] if row is not None and row["at"] else None)

    def pending(self, pane_id=None):
        with self._lock:
            if pane_id:
                return [dict(row) for row in self.db.execute(
                    "SELECT * FROM owner_push WHERE pane_id = ? AND taken_at IS NULL ORDER BY id",
                    (pane_id,))]
            return [dict(row) for row in self.db.execute(
                "SELECT * FROM owner_push WHERE taken_at IS NULL ORDER BY id")]

    def stats(self, window_s=86400, now=None):
        now = now or time.time()
        with self._lock:
            rows = [dict(row) for row in self.db.execute(
                "SELECT created_at, taken_at FROM owner_push WHERE created_at >= ?",
                (now - window_s,))]
        latencies = sorted(max(0.0, row["taken_at"] - row["created_at"])
                           for row in rows if row["taken_at"])
        return {"pushed": len(rows), "taken": len(latencies),
                "pending": sum(1 for row in rows if not row["taken_at"]),
                "latency_s": {"p50": _pct(latencies, 0.5), "p90": _pct(latencies, 0.9),
                              "max": round(latencies[-1], 2) if latencies else None}}


def _pct(values, q):
    if not values:
        return None
    index = min(len(values) - 1, int(round(q * (len(values) - 1))))
    return round(values[index], 2)
