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

import json
import os
import shutil
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import time

WAL_WARN_BYTES = 64 * 1024 * 1024
# The dispatcher runs every 5 minutes. Missing two ticks means the feeder is dead,
# which is invisible while a human is feeding lanes by hand - the failure mode that
# lasted hours on 2026-10-23.
DISPATCH_STALE_S = 16 * 60
DISK_MIN_FREE_BYTES = 512 * 1024 * 1024
HEALTH_PATH = os.path.expanduser("~/.config/dunkingsheep/health.json")


def _stale_leases(path, now):
    """Count ingest rows a dead worker left behind. Read-only, fixed query."""
    conn = sqlite_connect(path, readonly=True)
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
    conn = sqlite_connect(path, readonly=True)
    try:
        return conn.execute("PRAGMA quick_check").fetchone()[0]
    finally:
        conn.close()


def _dispatch_freshness(config_dir, now):
    """How long since a dispatcher tick completed: 'the feeder is dead' as a number.

    Absence is not evidence: a tick that has never run and a tick that stopped are
    different, so the check reports both, with the age.
    """
    path = os.path.join(config_dir, "dispatch-state.json")
    try:
        with open(path, encoding="utf-8") as handle:
            state = json.load(handle)
        tick_at = float(state.get("tick_at") or 0)
    except (OSError, ValueError):
        return {"status": "absent", "age_s": None, "reason": "no dispatcher tick has ever run"}
    if not tick_at:
        return {"status": "absent", "age_s": None, "reason": "the dispatch state has no tick_at"}
    age = max(0.0, now - tick_at)
    return {"status": "ok" if age <= DISPATCH_STALE_S else "stale", "age_s": int(age),
            "dry_run": bool(state.get("dry_run")), "reason": None}


def run_health(flock, repair=False, deep=False):
    started = time.time()
    problems, repairs, checks = [], [], {}
    dispatch = _dispatch_freshness(os.environ.get("DUNKINGSHEEP_DIR")
                                   or os.path.expanduser("~/.config/dunkingsheep"), started)
    checks["dispatch"] = dispatch
    # ABSENT IS NOT A PROBLEM: a fresh host has no tick yet, and a health check that
    # calls that DEGRADED is an alarm nobody can satisfy until the timer runs. It is
    # reported in the checks (so it is visible) and becomes a problem only when a tick
    # EXISTS and has stopped.
    if dispatch["status"] == "stale":
        problems.append(f"dispatcher: last successful tick {dispatch['age_s']}s ago "
                        f"(stale after {DISPATCH_STALE_S}s) - the feeder is not running")
    store = getattr(flock, "memory", None)
    # Honour DUNKINGSHEEP_DIR: a health check that reads the LIVE dispatch state while
    # a test points it at a temp dir is not isolated, and it reports the live fleet's
    # staleness as if it were the test's.
    config_dir = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkingsheep")

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

    # REGISTRY vs PANE LIST (t714). A registered lane whose process is alive but which
    # the backend pane list does not contain is unreachable: mail cannot be typed into
    # it, and a guard that trusts the list calls it dead. Measured 22:15 on 2026-10-29
    # for %103/%108/%109 and never reproduced, so the check reports the disagreement
    # instead of asserting a cause. Registrations whose process is GONE are listed in
    # the checks but are not a problem: that is a stale row, not a lost lane.
    gap_reader = getattr(flock, "registry_pane_gap", None)
    if callable(gap_reader):
        try:
            gap = gap_reader()
        except Exception as error:  # noqa: BLE001
            gap = []
            problems.append(f"registry/pane-list comparison failed: {error}")
        checks["registry_pane_gap"] = gap
        alive_gap = [row for row in gap if row.get("alive")]
        if alive_gap:
            names = ", ".join(f"{row['pane']} ({row.get('label') or row.get('role')}, "
                              f"pid {row.get('pid')})" for row in alive_gap)
            problems.append(
                f"backend pane list is missing {len(alive_gap)} registered lane(s) whose "
                f"process is alive: {names} - they cannot be typed into and read as dead "
                "to the close/dead guards (t699/t714)")

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
