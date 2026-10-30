#!/usr/bin/env python3
"""
Operations telemetry: a durable, append-only record of what the daemon did.

Dunking Sheep already keeps two durable stores, but neither answers "how is
delivery going". `inbox.db` knows when a message was written, read and
acknowledged, which is enough for pickup and handling times. `dunks.json`
keeps cumulative counters per schedule. Everything about the typing pipeline -
how long a send waited, how many Enter presses it took, what blocked it - has
until now lived only in memory and in an unstructured log, and was lost on
every restart. This module is where those events become durable.

What is recorded, deliberately: identifiers and numbers. Mailbox names, pane
ids, dunk ids, counts, durations and byte sizes. What is never recorded: a
message body, its subject, an attachment's filename, an account email, or a
secret. A subject is content, so it stays out, and nothing here estimates
token spend, which would be a guess presented as a measurement.

The store is separate from `inbox.db` on purpose: telemetry retention is a
different question from mail retention, and this database can be pruned or
deleted outright without touching anything a person is waiting to read.
"""

import os
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import threading
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
METRICS_DB = os.path.join(CONFIG_DIR, "metrics.db")

# Events older than this are pruned. Thirty days was the agreed window.
RETENTION_S = 30 * 24 * 3600
# Rows are small; this ceiling exists so a runaway loop cannot fill a disk.
MAX_EVENTS = 2_000_000

SCHEMA_VERSION = 1

SCHEMA = """
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          REAL NOT NULL,
    kind        TEXT NOT NULL,
    pane_id     TEXT,
    mailbox     TEXT,
    sender      TEXT,
    source      TEXT,
    outcome     TEXT,
    reason      TEXT,
    duration_ms INTEGER,
    attempts    INTEGER,
    bytes       INTEGER
);
CREATE INDEX IF NOT EXISTS events_by_time ON events (at);
CREATE INDEX IF NOT EXISTS events_by_kind ON events (kind, at);
"""

# What each event means, returned with every summary so a consumer never has
# to guess. Keep in step with the `record` calls in dunk_core.
KINDS = {
    "message.sent": "text was typed into a pane and confirmed submitted",
    "message.failed": "text was typed but could not be confirmed submitted",
    "message.held": "a send was not attempted because the pane was not ready",
    "message.cancelled": "a queued send was withdrawn before delivery",
    "submit.attempt": "one confirm-and-retry sequence, with the Enter presses it took",
    "inbox.sent": "a message was written to a mailbox",
    "inbox.read": "a recipient read a message, with how long it had waited",
    "inbox.acked": "a recipient finished with a message, with how long it held it",
    "dunk.sent": "a scheduled send went out",
    "dunk.skipped": "backpressure skipped a send: pending or unrecognised input",
    "dunk.held": "a scheduled send was held because the pane was not ready",
    "panel.blocking": "a dialog was open with a delivery waiting behind it",
    "turn.end.dispatch": ("the turn-end trigger fired: a lane that ended a turn was "
                          "prompted, or skipped with the reason it named"),
    # t1011: these twelve were recorded and undocumented - a consumer reading KINDS could
    # not interpret most of a summary. Each meaning is taken from the record() call sites
    # (dunk_core, dunk_next_job, dunk_server, web/gameplay.py), and
    # tests/test_dunk_metrics.py now greps those call sites so the map cannot drift again.
    "turn.start": "a lane declared the start of a turn (source names the producer)",
    "turn.end": "a lane declared the end of a turn, with the reason the trigger gave",
    "turn.expired": ("a turn's TTL ran out without an end: the agent declared work and "
                     "never reported finishing"),
    "identity.mismatch": ("a caller's pane/pid claim did not match the live binding; the "
                          "request was refused as unverified"),
    "inbox.archived": "a message was aged out of a mailbox by retention",
    "message.undeliverable": ("a queued send was given up on: the pane was dead or the "
                              "message could not be delivered at all"),
    "next_job.decision": ("the turn-end trigger decided what a finished lane should do "
                          "next: claim, prompt, or skip with a reason"),
    "pane.respawn": "a managed pane's process was replaced in place (respawn-pane)",
    "pane.respawn.rollback": ("a respawn was rolled back to the previous argv after the new "
                              "command failed to come up"),
    "stuck.recover": ("the stuck-message machine pressed Enter on an unsubmitted composer, "
                      "with the attempt number"),
    "autocodeword": ("the desk auto-entered a codeword answer into the running game (the "
                     "gameplay bridge's own event)"),
    "loadstate": "a save slot was loaded through the gameplay bridge, verified or not",
    "savestate": "a save slot was written through the gameplay bridge",
}

OUTCOMES = {
    "ok": "completed as intended",
    "failed": "attempted and did not complete",
    "held": "not attempted; the pane was not ready",
    "cancelled": "withdrawn before it was attempted",
}

REASONS = {
    "draft": "someone was writing in the input box",
    "dialog": "a dialog was open, waiting for a person to answer",
    "stranded": "our own earlier text was still sitting unsubmitted",
    "unconsumed": "the previous scheduled send is still pending in the input box",
    "unrecognised": "backpressure could not read or recognise the input box",
    "unconfirmed": "the submit could not be confirmed from the pane",
    "no_open_work": "the lane had no CLAIMED (leased) task, so it was left alone",
    "unread_mail": "the lane already had unread mail: it was fed",
    "unread_unknown": "the lane's unread count could not be read, so it was not fed",
    "cooldown": "the same pane was prompted moments ago",
    "task_cooldown": "every claimed task was prompted recently",
    "held_delivery_pending": "a prompt for the pane was already queued undelivered",
    "disabled": "the turn-end trigger is switched off",
}


class Metrics:
    """Append-only event store plus the aggregation the endpoint returns."""

    def __init__(self, path=METRICS_DB, retention_s=RETENTION_S, enabled=True):
        self.path = path
        self.retention_s = retention_s
        self.enabled = enabled
        self._lock = threading.RLock()
        self.started_at = time.time()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        self.db = sqlite_connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            self.db.commit()

    def close(self):
        with self._lock:
            self.db.close()

    # -- writing ------------------------------------------------------------

    def record(self, kind, pane_id=None, mailbox=None, sender=None, source=None,
               outcome=None, reason=None, duration_ms=None, attempts=None,
               bytes=None, at=None):
        """Append one event. Never raises: telemetry must not break delivery."""
        if not self.enabled:
            return
        try:
            with self._lock:
                self.db.execute(
                    "INSERT INTO events (at, kind, pane_id, mailbox, sender, source, "
                    "outcome, reason, duration_ms, attempts, bytes) "
                    "VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                    (at or time.time(), kind, pane_id, mailbox, sender, source,
                     outcome, reason,
                     None if duration_ms is None else int(duration_ms),
                     None if attempts is None else int(attempts),
                     None if bytes is None else int(bytes)))
                self.db.commit()
        except Exception:  # noqa: BLE001 - a telemetry failure is not a send failure
            pass

    def prune(self, older_than_s=None, max_events=MAX_EVENTS):
        cutoff = time.time() - (self.retention_s if older_than_s is None else older_than_s)
        with self._lock:
            removed = self.db.execute("DELETE FROM events WHERE at < ?", (cutoff,)).rowcount
            total = self.db.execute("SELECT COUNT(*) AS n FROM events").fetchone()["n"]
            if total > max_events:
                over = total - max_events
                self.db.execute(
                    "DELETE FROM events WHERE id IN "
                    "(SELECT id FROM events ORDER BY at LIMIT ?)", (over,))
                removed += over
            self.db.commit()
        return {"pruned": removed, "retention_s": self.retention_s}

    # -- reading ------------------------------------------------------------

    def earliest(self):
        with self._lock:
            row = self.db.execute("SELECT MIN(at) AS a FROM events").fetchone()
        return row["a"]

    def _rows(self, start, end):
        with self._lock:
            return self.db.execute(
                "SELECT * FROM events WHERE at >= ? AND at < ? ORDER BY at",
                (start, end)).fetchall()

    def summary(self, start, end):
        """Counts and duration distributions for one window.

        Every duration is null rather than zero when the window holds no event
        that measures it, so a consumer can tell "nothing happened" from "not
        measured here"."""
        rows = self._rows(start, end)
        counts = {
            "events": len(rows),
            "by_kind": _tally(rows, "kind"),
            "by_sender": _tally(rows, "sender"),
            "by_recipient": _tally(rows, "mailbox"),
            "by_pane": _tally(rows, "pane_id"),
            "by_source": _tally(rows, "source"),
            "by_outcome": _tally(rows, "outcome"),
            "by_reason": _tally(rows, "reason"),
        }
        durations = {
            "queue_wait": _distribution(rows, "inbox.read"),
            "delivery": _distribution(rows, "message.sent"),
            "handling": _distribution(rows, "inbox.acked"),
            "blocked": _distribution(rows, "panel.blocking"),
        }
        presses = [r["attempts"] for r in rows
                   if r["kind"] == "submit.attempt" and r["attempts"]]
        retries = {
            "enter_presses": _histogram(presses),
            "submits": len(presses) or None,
            "first_press_success_rate": (
                round(sum(1 for p in presses if p == 1) / len(presses), 4)
                if presses else None),
        }
        return counts, durations, retries


def _tally(rows, column):
    out = {}
    for row in rows:
        value = row[column]
        if value:
            out[value] = out.get(value, 0) + 1
    return dict(sorted(out.items(), key=lambda kv: (-kv[1], kv[0])))


def _histogram(values):
    out = {}
    for value in values:
        out[str(value)] = out.get(str(value), 0) + 1
    return dict(sorted(out.items(), key=lambda kv: int(kv[0]))) or None


def _distribution(rows, kind):
    values = sorted(r["duration_ms"] for r in rows
                    if r["kind"] == kind and r["duration_ms"] is not None)
    if not values:
        return None
    return {
        "n": len(values),
        "p50": _percentile(values, 0.50),
        "p90": _percentile(values, 0.90),
        "p99": _percentile(values, 0.99),
        "max": values[-1],
    }


def _percentile(sorted_values, fraction):
    """Nearest-rank percentile. With few samples an interpolated figure would
    imply a precision the data does not have."""
    if not sorted_values:
        return None
    index = max(0, min(len(sorted_values) - 1,
                       int(round(fraction * len(sorted_values) + 0.5)) - 1))
    return sorted_values[index]
