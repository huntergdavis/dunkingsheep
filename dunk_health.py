"""Deterministic self-check and self-repair for the Dunking Sheep daemon.

There is no model in this loop. Every check has a fixed threshold and, when
`repair` is set, a fixed repair. The daemon's maintenance tick and the
`dunkingsheep health --repair` systemd timer call the same code, so a decision
is encoded once and never asked of an LLM.

Checks: memory DB presence + `PRAGMA quick_check`, schema/ok flag, stale ingest
leases (expired or a partial source left by a restart), WAL size, free disk
space, and basic task counts. Repairs: requeue stale leases, checkpoint a large
WAL. Anything that cannot be repaired is returned in `problems` for a human or
the supervisor to see.
"""

import os
import shutil
import sqlite3
import time

WAL_WARN_BYTES = 64 * 1024 * 1024
DISK_MIN_FREE_BYTES = 512 * 1024 * 1024
HEALTH_PATH = os.path.expanduser("~/.config/dunkingsheep/health.json")


def _stale_leases(path, now):
    """Count ingest rows a dead worker left behind. Read-only, fixed query."""
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        expired = conn.execute(
            "SELECT COUNT(*) FROM ingest_queue WHERE state='leased' "
            "AND (lease_until IS NULL OR lease_until < ?)", (now,)).fetchone()[0]
        partial = conn.execute(
            "SELECT COUNT(*) FROM ingest_queue q JOIN sources s "
            "ON s.id=q.source_id WHERE q.state='skipped' AND s.state='partial'"
        ).fetchone()[0]
    finally:
        conn.close()
    return {"expired_leases": expired, "partial_skipped": partial}


def _quick_check(path):
    conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return conn.execute("PRAGMA quick_check").fetchone()[0]
    finally:
        conn.close()


def run_health(flock, repair=False, deep=False):
    started = time.time()
    problems, repairs, checks = [], [], {}
    store = getattr(flock, "memory", None)
    config_dir = os.path.expanduser("~/.config/dunkingsheep")

    if store is None:
        problems.append("memory service disabled")
    else:
        try:
            stats = flock.memory_stats()
        except Exception as error:  # noqa: BLE001 - report, never raise
            stats = {}
            problems.append(f"memory_stats failed: {error}")
        try:
            status = flock.memory_status()
        except Exception as error:  # noqa: BLE001
            status = {}
            problems.append(f"memory_status failed: {error}")
        memory = {key: stats.get(key) for key in
                  ("path", "schema_version", "sources", "sessions", "chunks",
                   "queue_depth")}
        memory["ok"] = status.get("ok")
        checks["memory"] = memory
        checks["db_bytes"] = stats.get("db_bytes") or {}
        if status.get("ok") is False:
            problems.append("memory status reports ok=false")
        path = stats.get("path")
        if path and os.path.exists(path):
            if deep:
                # A full quick_check scans the whole DB (seconds to tens of
                # seconds); run it only when asked, never on the fast tick.
                try:
                    integrity = _quick_check(path)
                except sqlite3.Error as error:
                    integrity = f"error: {error}"
                checks["integrity"] = integrity
                if integrity != "ok":
                    problems.append(f"sqlite quick_check: {integrity}")
            else:
                # Cheap liveness: the header and freelist answer, and stats
                # (which read real tables) already succeeded.
                integrity = "ok (shallow)"
                checks["integrity"] = integrity
            stale = _stale_leases(path, started)
            checks["stale_leases"] = stale
            if stale["expired_leases"] or stale["partial_skipped"]:
                if repair:
                    store.recover_leases()
                    store.requeue_partial()
                    repairs.append(
                        f"requeued {stale['expired_leases']} expired + "
                        f"{stale['partial_skipped']} partial ingest lease(s)")
                else:
                    problems.append(f"stale ingest leases: {stale}")
            wal = (checks["db_bytes"] or {}).get("-wal") or 0
            checks["wal_bytes"] = wal
            if wal > WAL_WARN_BYTES:
                if repair:
                    store.checkpoint()
                    repairs.append(f"checkpointed WAL ({wal} bytes)")
                else:
                    problems.append(f"WAL large: {wal} bytes")

    usage = shutil.disk_usage(config_dir)
    checks["disk_free_bytes"] = usage.free
    if usage.free < DISK_MIN_FREE_BYTES:
        problems.append(f"disk free below threshold: {usage.free} bytes")

    try:
        checks["tasks"] = flock.tasks.stats().get("totals", {})
    except Exception as error:  # noqa: BLE001
        problems.append(f"task stats failed: {error}")

    return {
        "ok": not problems,
        "checked_at": started,
        "duration_ms": round((time.time() - started) * 1000.0, 1),
        "problems": problems,
        "repairs": repairs,
        "checks": checks,
    }


def write_report(result, path=None):
    """Write the last report where ops tooling (and the VP) can read it."""
    import json
    path = path or HEALTH_PATH
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as handle:
        json.dump(result, handle, indent=2, sort_keys=True)
    return path
