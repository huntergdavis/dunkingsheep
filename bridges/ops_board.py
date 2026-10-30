"""Deterministic operations board for the VP desk.

The board reads tasks, live lanes, schedules and health from Dunking Sheep,
plus the saved host sample from the private operations database. It exposes
a brief, actionable exceptions and a single reference containing useful rows.
Provider budget windows are read by a command; credit and throughput remain outside this overview.

No model is consulted anywhere in this module. Every number is read from a
record; nothing is inferred, interpolated, or summarised in prose. A source
that cannot be read stays ``unavailable`` and keeps its age instead of
becoming a zero, and a missing observation is never back-filled.
"""

from __future__ import annotations

import os
import sqlite3
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]   # repo root: one repo (FLAT_FLEET.md 9)
DUNKING_SHEEP = Path(os.getenv("DUNKING_SHEEP_DIR", ROOT))
if str(DUNKING_SHEEP) not in sys.path:
    sys.path.insert(0, str(DUNKING_SHEEP))

REFRESH_INTERVAL_S = max(10, int(os.getenv("VP_OPS_BOARD_INTERVAL_S", "30")))
CLIENT_TIMEOUT_S = 5.0
TASK_WINDOW_S = 3600
CREDIT_WINDOW_H = 168
ACTIVE_STATUSES = ("open", "in_progress", "blocked")
PRIORITY_RANK = {"urgent": 0, "high": 1, "medium": 2, "low": 3}



def _now() -> datetime:
    return datetime.now(timezone.utc)


def _iso(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _age_seconds(now: datetime, value: Any) -> int | None:
    """Age of a record in seconds, whatever timestamp shape it uses."""
    if value is None:
        return None
    try:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            moment = datetime.fromtimestamp(float(value), timezone.utc)
        else:
            parsed = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
            moment = parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)
    except (OSError, OverflowError, TypeError, ValueError):
        return None
    return max(0, int((now - moment.astimezone(timezone.utc)).total_seconds()))


def _seconds_until(now: datetime, value: Any) -> int | None:
    """Seconds until a future timestamp, or None when it is not in the future."""
    if value is None:
        return None
    try:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            moment = datetime.fromtimestamp(float(value), timezone.utc)
        else:
            parsed = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
            moment = parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)
    except (OSError, OverflowError, TypeError, ValueError):
        return None
    delta = (moment.astimezone(timezone.utc) - now).total_seconds()
    return max(0, int(delta)) if delta > 0 else None


def _client():
    """A Dunking Sheep client with a short timeout; the daemon is local."""
    from dunk_client import FlockClient

    client = FlockClient.connect_or_start()
    client.timeout = CLIENT_TIMEOUT_S
    return client


def _unavailable(error: str) -> dict:
    return {"state": "unavailable", "error": error}


TASK_LIST_LIMIT = 1000


def _task_board(client, now: datetime) -> dict:
    # The reference list is fetched with an explicit completeness contract:
    # TASK_LIST_LIMIT is well above anything the desk has ever queued, and if
    # the daemon still returns a full page, the projection below is known to
    # be truncated and the leader-held aggregate is reported unavailable
    # rather than a definitive-looking number derived from a truncated list
    # (t407/t408 F3).
    try:
        listing = client.call("task_list", include_done=False, limit=TASK_LIST_LIMIT)
    except Exception as exc:  # noqa: BLE001 - a missing source is reported, not raised
        return _unavailable(type(exc).__name__)
    # t996: throughput comes from the daemon's own buckets (task_metrics), and a
    # failure there must not take the backlog tile with it.
    try:
        throughput = _throughput(client.call("task_metrics", window_s=86400), now)
    except Exception as exc:  # noqa: BLE001
        throughput = {"completed_current_bucket": None, "completed_last_24h": None,
                      "error": type(exc).__name__}
    tasks = listing.get("tasks") if isinstance(listing, dict) else listing
    if not isinstance(tasks, list):
        return _unavailable("invalid task list")
    counts: dict[str, int] = {}
    items = []
    for task in tasks:
        status = str(task.get("status", "unknown"))
        counts[status] = counts.get(status, 0) + 1
        items.append({
            "id": task.get("id"),
            "title": task.get("title"),
            "status": status,
            "priority": task.get("priority"),
            "scope": task.get("scope"),
            "assignees": list(task.get("assignees") or []),
            "created_at": task.get("created_at"),
            "age_s": _age_seconds(now, task.get("created_at")),
            "updated_age_s": _age_seconds(now, task.get("updated_at")),
            "lease_expires_in_s": _seconds_until(now, task.get("lease_expires_at")),
            "blocked_by": list(task.get("blocked_by") or []),
            "eligible": bool(task.get("eligible")),
            "claimable": bool(task.get("claimable")),
            "attempts": task.get("attempts"),
            "needs_owner": bool(task.get("needs_owner")) or "owner" in (task.get("assignees") or []),
        })
    items.sort(key=lambda row: (PRIORITY_RANK.get(row["priority"], 9),
                                -(row["age_s"] or 0), str(row["id"])))
    load: dict[str, int] = {}
    for task in items:
        for assignee in task["assignees"]:
            load[assignee] = load.get(assignee, 0) + 1
    # t996: the VP-era "leader held" tile is gone. It counted the unassigned
    # queue as a person's carried work - there is no leader any more (flat
    # fleet), so the tile reported a role that does not exist and the number it
    # showed was just "open, global" under another name. What replaces it is
    # measured: the backlog by priority with the OLDEST age, because that is the
    # question an operator actually asks ("is anything rotting?").
    #
    # A truncated list must not back a definitive number (t408 F3), so coverage
    # travels with it exactly as it did before.
    complete = len(items) < TASK_LIST_LIMIT
    backlog = {"by_priority": {}, "open": 0, "oldest_age_s": None,
               "ages_unknown": 0, "complete": complete}
    for status in ACTIVE_STATUSES:
        backlog["by_priority"][status] = {}
    for row in items:
        if row["status"] not in ACTIVE_STATUSES:
            continue
        bucket = backlog["by_priority"].setdefault(row["status"], {})
        bucket[row["priority"] or "unknown"] = bucket.get(row["priority"] or "unknown", 0) + 1
        backlog["open"] += 1
    active_ages = [row["age_s"] for row in items
                   if row["status"] in ACTIVE_STATUSES and row["age_s"] is not None]
    backlog["ages_unknown"] = backlog["open"] - len(active_ages)
    # Zero is only a valid age when the age is known (t408 F2); an unknown
    # timestamp makes the max a lower bound, and `ages_unknown` says by how much.
    backlog["oldest_age_s"] = max(active_ages) if active_ages else None
    depth = sum(counts.get(status, 0) for status in ACTIVE_STATUSES)
    return {
        "state": "ok",
        "counts": counts,
        "depth": depth,
        "items": items,
        "by_assignee": [{"assignee": name, "count": count}
                         for name, count in sorted(load.items(), key=lambda row: (-row[1], row[0]))],
        "backlog": backlog,
        "throughput": throughput,
    }


def _throughput(metrics: dict, now: datetime | None = None) -> dict:
    """Completed last hour / last 24 h from `task_metrics` (t996).

    The daemon's payload is `{throughput: {completed: N, buckets: [{bucket, completed}]}}`
    where `bucket` is a unix second. Read from there rather than counted here: the desk
    must not re-derive what the daemon already measures, or the two numbers will
    disagree and nobody will know which is the measurement.

    The buckets are hourly, so there is no trailing hour here: the tile reports the
    CURRENT (partial) bucket under that name, with its start time, never as "the last
    hour" (reviewer finding on #198: it read 0 twenty minutes into an hour with 12).
    """
    throughput = (metrics or {}).get("throughput") or {}
    buckets = throughput.get("buckets") or []
    if not isinstance(buckets, list) or not buckets:
        return {"completed_current_bucket": None, "completed_last_24h": None,
                "note": "no buckets in the metrics payload"}
    now_s = (now or _now()).timestamp()
    rows = [row for row in buckets
            if isinstance(row, dict) and row.get("bucket") is not None]
    day = [row for row in rows if now_s - float(row["bucket"]) <= 86400]
    total = lambda rows: sum(int(row.get("completed") or 0) for row in rows)
    # The daemon's buckets are HOURLY, so no trailing hour can be built from them:
    # the newest bucket is the current, partial hour and is reported as exactly that.
    newest = max(rows, key=lambda row: float(row["bucket"]), default=None)
    current = newest if newest and now_s - float(newest["bucket"]) < 3600 else None
    return {"completed_current_bucket": int(current.get("completed") or 0) if current else 0,
            "bucket_started_at": float(newest["bucket"]) if newest else None,
            "completed_last_24h": total(day),
            "completed_in_window": throughput.get("completed"),
            "note": "current (partial) hourly bucket; last_24h sums buckets started in 86400 s"}


def _review_board(client, now: datetime) -> dict:
    """PRs awaiting review, with the age of the oldest (t996).

    This is the number that proves "all code reviewed" is working: a review queue
    that has stopped moving looks exactly like a clean board otherwise.

    The AGE comes from the daemon (`age_s`, measured against the daemon's clock)
    rather than being re-derived here from a timestamp - my first version looked for
    `created_at`, which review rows do not carry, and would have shipped a tile that
    always said "unknown" while looking measurable.
    """
    try:
        listing = client.call("review_list")
    except Exception as exc:  # noqa: BLE001 - a missing source is reported, not raised
        return _unavailable(type(exc).__name__)
    rows = listing.get("reviews") if isinstance(listing, dict) else listing
    if not isinstance(rows, list):
        return _unavailable("invalid review list")
    ages, unknown = [], 0
    for row in rows:
        age = row.get("age_s")
        if age is None:
            age = _age_seconds(now, row.get("created_at"))
        if age is None:
            unknown += 1
        else:
            ages.append(int(age))
    return {"state": "ok", "review": {
        "awaiting": len(rows),
        "oldest_age_s": max(ages) if ages else None,
        "ages_unknown": unknown,
    }}


def _dunk_board(client, now: datetime) -> dict:
    try:
        dunks = client.call("list_dunks")
    except Exception as exc:  # noqa: BLE001
        return _unavailable(type(exc).__name__)
    rows = (dunks or {}).get("dunks")
    if not isinstance(rows, list):
        return _unavailable("invalid dunk list")
    items = [{
        "id": dunk.get("id"),
        "name": dunk.get("name") or dunk.get("id"),
        "target": dunk.get("target_pane_id") or dunk.get("target_agent")
        or dunk.get("target_tab") or dunk.get("target_workspace") or "unknown",
        "target_label": dunk.get("target_label"),
        "interval_minutes": dunk.get("interval_minutes"),
        "running": bool(dunk.get("running")),
        "status": dunk.get("status"),
        "next_send_in_s": dunk.get("next_send_in_s"),
        "send_count": dunk.get("send_count"),
        "skip_count": dunk.get("skip_count"),
        "hold_count": dunk.get("hold_count"),
        "only_when": dunk.get("only_when"),
        "last_error": dunk.get("last_error"),
        # The daemon clears last_error on a successful send, so an error here
        # belongs to the most recent attempt. The age of the last successful
        # send says whether the schedule still delivers.
        "last_sent_at": dunk.get("last_sent_at"),
        "last_sent_age_s": _age_seconds(now, dunk.get("last_sent_at")),
        "last_skipped_age_s": _age_seconds(now, dunk.get("last_skipped_at")),
    } for dunk in rows]
    return {
        "state": "ok",
        "total": len(items),
        "running": sum(1 for dunk in items if dunk["running"]),
        "failed": sum(1 for dunk in items if dunk["last_error"]),
        "items": items,
    }


def _pane_board(client, now: datetime | None = None) -> dict:
    """Lanes: working / idle / dead, from the daemon's panes AND its lane registry.

    t996: my first pass counted only panes whose `agent` field was set, and on the
    live host that field is empty for every pi lane - the tile read "0 working, 0
    idle, 0 dead" while twenty panes were doing real work. The activity is
    `agent_status` (declared turns), and a lane the terminal backend no longer lists
    is DEAD (`registry_gap`, t714) - not "unknown", which is what a lane with no
    declared turn yet is. Those are three different questions and the tile must not
    collapse them.
    """
    try:
        panes = client.call("list_panes")
    except Exception as exc:  # noqa: BLE001
        return _unavailable(type(exc).__name__)
    rows = (panes or {}).get("panes")
    if not isinstance(rows, list):
        return _unavailable("invalid pane list")
    items = [{
        "pane_id": pane.get("pane_id"),
        "agent": pane.get("agent") or "",
        "agent_status": pane.get("agent_status"),
        "agent_status_source": pane.get("agent_status_source"),
        "agent_status_reason": pane.get("agent_status_reason"),
        "agent_status_inferred": pane.get("agent_status_inferred"),
        "workspace_label": pane.get("workspace_label"),
        "tab_label": pane.get("tab_label"),
        "is_self": bool(pane.get("is_self")),
    } for pane in rows]
    working = sum(1 for pane in items if pane.get("agent_status") == "working")
    idle = sum(1 for pane in items if pane.get("agent_status") == "idle")
    gap = (panes or {}).get("registry_gap") or []
    dead = [dict(row, pane_id=row.get("pane_id") or row.get("pane"))
            for row in gap if row.get("alive") is False]
    # A lane is a pane with a declared turn or an agent; a bare shell tab is not an
    # unaccounted lane and is counted on its own (reviewer finding 2 on #198).
    is_lane = lambda pane: bool(pane.get("agent")) or pane.get("agent_status") in ("working", "idle")
    bare = sum(1 for pane in items if not is_lane(pane))
    return {"state": "ok", "total": len(items), "working": working, "idle": idle,
            "bare_panes": bare,
            "dead": len(dead), "dead_items": dead,
            # A pane the backend lists with no declared turn is unknown; a lane the
            # registry knows and the backend does NOT list, with its process alive, is
            # the t714 case: unreachable, and also not dead.
            "unknown": sum(1 for pane in items if is_lane(pane)
                           and pane.get("agent_status") not in ("working", "idle"))
                       + sum(1 for row in gap if row.get("alive") is True),
            "items": items}


def _health_board(client) -> dict:
    try:
        health = client.call("health")
    except Exception as exc:  # noqa: BLE001
        return _unavailable(type(exc).__name__)
    if not isinstance(health, dict):
        return _unavailable("invalid health report")
    checks = health.get("checks") or {}
    return {
        "state": "ok",
        "ok": bool(health.get("ok")),
        "problems": list(health.get("problems") or []),
        "repairs": list(health.get("repairs") or []),
        "duration_ms": health.get("duration_ms"),
        "memory_ok": (checks.get("memory") or {}).get("ok"),
        "memory_sources": (checks.get("memory") or {}).get("sources"),
        "memory_chunks": (checks.get("memory") or {}).get("chunks"),
        "queue_depth": (checks.get("memory") or {}).get("queue_depth"),
        "stale_leases": (checks.get("stale_leases") or {}).get("expired_leases"),
        "wal_bytes": checks.get("wal_bytes"),
        "disk_free_bytes": checks.get("disk_free_bytes"),
    }


# -- operations database ------------------------------------------------------


def _host_board(conn, now):
    from ops_metrics import summary
    host = summary(conn, now=now).get("host_health")
    if not host:
        return _unavailable("no host sample")
    return {"state": "stale" if host.get("stale") else "ok", **host}


def compact(readings: dict, generated_at: str) -> dict:
    """Three owner-facing sections; no nested copy of the old board."""
    def source(name):
        return {"source": name, "generated_at": generated_at,
                **readings.get(name, _unavailable("missing source"))}
    tasks, panes, dunks, health, host = map(source, ("tasks", "panes", "dunks", "health", "host"))
    reviews = readings.get("reviews") or _unavailable("missing source")
    owner = [row for row in tasks.get("items", []) if row.get("needs_owner")]
    alarms = []
    for reading in (tasks, panes, dunks, health, host):
        if reading["state"] != "ok":
            alarms.append({"source": reading["source"], "state": reading["state"],
                           "generated_at": generated_at,
                           "text": reading.get("error") or (reading["source"] + " is " + reading["state"])})
    for problem in health.get("problems", []):
        alarms.append({"source": "health", "state": "problem",
                       "generated_at": generated_at, "text": problem})
    for row in dunks.get("items", []):
        if row.get("running") and row.get("last_error"):
            alarms.append({"source": "dunks", "state": "problem",
                           "generated_at": generated_at,
                           "text": str(row["id"]) + ": " + row["last_error"]})
    for resource, level in (host.get("levels") or {}).items():
        if level in ("warning", "critical"):
            alarms.append({"source": "host", "state": level, "generated_at": generated_at,
                           "text": resource + ": " + level})
    def count(reading, **values):
        return {"source": reading["source"], "state": reading["state"],
                "generated_at": generated_at,
                **{key: value if reading["state"] == "ok" else None for key, value in values.items()}}
    # t996: the header strip, four tiles, each carrying its SOURCE and the time it
    # was read - the same contract every other number on this board keeps, and the
    # reason the old "56 held by the leader" could not be traced to anything.
    withheld = (tasks.get("backlog") or {})
    throughput = tasks.get("throughput") or {}
    review = reviews.get("review") if isinstance(reviews, dict) else None
    return {
        "generated_at": generated_at, "interval_s": REFRESH_INTERVAL_S, "no_model": True,
        "brief": {
            # The throughput tile keeps its NOTE: a null with no explanation is the
            # thing this board exists to stop showing (t996) - "no buckets" is a
            # measurement failure, and it must be visible as one.
            "throughput": {"source": "tasks", "generated_at": generated_at,
                           "state": tasks["state"],
                           **{key: value for key, value in throughput.items()
                              if key != "error"},
                           "error": throughput.get("error")},
            "backlog": count(tasks, open=withheld.get("open"),
                             by_priority=withheld.get("by_priority") or {},
                             oldest_age_s=withheld.get("oldest_age_s"),
                             ages_unknown=withheld.get("ages_unknown"),
                             complete=withheld.get("complete")),
            "review": {"source": "review_list", "generated_at": generated_at,
                       "state": (reviews or {}).get("state", "unavailable"),
                       "awaiting": (review or {}).get("awaiting"),
                       "oldest_age_s": (review or {}).get("oldest_age_s"),
                       "ages_unknown": (review or {}).get("ages_unknown")},
            "lanes": count(panes, working=panes.get("working"), idle=panes.get("idle"),
                           dead=panes.get("dead"), unknown=panes.get("unknown")),
        },
        "attention": {"generated_at": generated_at, "alarms": alarms,
                      "owner": {"source": "tasks", "state": tasks["state"],
                                "generated_at": generated_at, "items": owner}},
        "reference": {"tasks": tasks, "lanes": panes, "schedules": dunks},
    }


def build(client, conn: sqlite3.Connection, *, now: datetime | None = None) -> dict:
    """Read only actionable sources; no throughput, inbox, credit or config panels.

    `conn` is the private OPERATIONS database (host samples, provider budget),
    not one of the daemon's stores: it is read by `_host_board` and
    `ops_budget.build` and cannot move to the socket without a daemon command for
    host samples. Everything else here is already a client call.
    """
    now = now or _now()
    readers = {"tasks": lambda: _task_board(client, now),
               "panes": lambda: _pane_board(client),
               "dunks": lambda: _dunk_board(client, now),
               "health": lambda: _health_board(client),
               "host": lambda: _host_board(conn, now)}
    readers["reviews"] = lambda: _review_board(client, now)
    readings = {}
    for name, read in readers.items():
        try:
            readings[name] = read()
        except Exception as exc:
            readings[name] = _unavailable(type(exc).__name__)
    board = compact(readings, _iso(now))
    import ops_budget
    board["budget"] = ops_budget.build(conn, now)
    return board


class BoardCache:
    """A cached board plus the timer that keeps it current.

    The refresh thread only reads. Nothing here calls a model, and a failed
    refresh keeps the previous snapshot with a fresh error note.
    """

    def __init__(self, interval_s: int = REFRESH_INTERVAL_S):
        self.interval_s = interval_s
        self._lock = threading.Lock()
        self._snapshot: dict | None = None
        self._error: str | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def _build(self) -> dict:
        from ops_metrics import connect

        conn = connect()
        try:
            return build(_client(), conn)
        finally:
            conn.close()

    def refresh(self) -> dict:
        try:
            snapshot = self._build()
        except Exception as exc:  # noqa: BLE001 - keep the last good board
            with self._lock:
                self._error = type(exc).__name__
                if self._snapshot is not None:
                    stale = dict(self._snapshot)
                    stale["refresh_error"] = self._error
                    return stale
            raise
        with self._lock:
            self._snapshot = snapshot
            self._error = None
        return snapshot

    def snapshot(self, *, max_age_s: int | None = None) -> dict:
        with self._lock:
            current = self._snapshot
            error = self._error
        age_limit = max_age_s if max_age_s is not None else self.interval_s * 3
        if current is None:
            return self.refresh()
        age = _age_seconds(_now(), current.get("generated_at"))
        if age is not None and age > age_limit:
            try:
                return self.refresh()
            except Exception:  # noqa: BLE001 - serve the stale board with its age
                pass
        with self._lock:
            current = self._snapshot
        if error:
            current = {**current, "refresh_error": error}
        return current

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()

        def loop() -> None:
            while not self._stop.wait(self.interval_s):
                try:
                    self.refresh()
                except Exception:  # noqa: BLE001 - the timer keeps running
                    continue

        self._thread = threading.Thread(target=loop, name="vp-ops-board", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)
            self._thread = None


CACHE = BoardCache()


def main() -> None:
    import argparse
    import json

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="print the board as JSON")
    parser.add_argument("--refresh", action="store_true", help="force a fresh read")
    args = parser.parse_args()
    board = CACHE.refresh() if args.refresh else CACHE.snapshot()
    print(json.dumps(board, indent=2, sort_keys=True, default=str))


if __name__ == "__main__":
    main()
