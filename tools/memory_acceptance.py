#!/usr/bin/env python3
"""
Memory service v1 acceptance test.

Two phases:

1. FLEET PROFILE (the bar): 20 parallel recall clients for 60 s while a large
   ingest runs, over a realistic query mix (narrow/medium/broad). Pass: p95 < 1 s,
   p99 < 2 s, zero failures, zero queries over 2 s.
2. BROAD STRESS (reported, not a v1 bar): a few clients hammering single
   common-term queries. Deja managed 2.49 s for 5 parallel recalls; this reports
   what 20 concurrent broad scans cost here.

Also checks redaction and the read-during-write stall regression. It never
touches deja or the real memory database.

    python3 tools/memory_acceptance.py --duration 60
"""

import argparse
import json
import os
import random
import shutil
import sqlite3
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from dunk_memory import MemoryIngestor, MemoryStore  # noqa: E402

SHARED = "acceptance memory recall"


def write_sample(path, sessions, messages, prefix, start=0):
    with open(path, "w", encoding="utf-8") as handle:
        for session in range(start, start + sessions):
            session_token = f"{prefix}{session:04d}"
            session_id = f"{prefix}-session-{session}"
            for index in range(messages):
                role = "user" if index % 2 == 0 else "assistant"
                text = (f"{SHARED} alpha decision queue note {index} "
                        f"{session_token}{index:04d} about the flux capacitor.")
                handle.write(json.dumps({
                    "role": role, "text": text, "session_id": session_id,
                    "cwd": "/home/hunter/workspace/acceptance",
                    "timestamp": 1780000000 + session * 100 + index,
                }) + "\n")
    return path


def percentile(values, fraction):
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(round(fraction * (len(ordered) - 1))))
    return round(ordered[index], 2)


def run_clients(store, queries, clients, duration, lock):
    latencies = []
    errors = [0]
    deadline = time.time() + duration
    rng = random.Random(1234)

    def worker():
        local = []
        while time.time() < deadline:
            query = rng.choice(queries)
            started = time.perf_counter()
            try:
                store.recall(query, limit=10)
            except Exception:  # noqa: BLE001 - counted, not hidden
                with lock:
                    errors[0] += 1
            local.append((time.perf_counter() - started) * 1000.0)
        with lock:
            latencies.extend(local)

    threads = [threading.Thread(target=worker, daemon=True) for _ in range(clients)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=duration + 30)
    return latencies, errors[0]


def redaction_check(store, tmpdir):
    secret = "sk-live-abcdef0123456789abcdef0123456789"
    path = os.path.join(tmpdir, "secrets.jsonl")
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(json.dumps({
            "role": "user", "session_id": "secret-session",
            "text": f"here is my key {secret} and password=hunter2secret",
            "cwd": "/home/hunter/workspace/acceptance",
        }) + "\n")
    store.ingest_path(path, harness="generic", project="acceptance")
    conn = store._read_connection()
    try:
        leaked = conn.execute(
            "SELECT COUNT(*) AS n FROM chunks WHERE text LIKE ?",
            (f"%{secret}%",)).fetchone()["n"]
        leaked += conn.execute(
            "SELECT COUNT(*) AS n FROM chunks WHERE text LIKE ?",
            ("%hunter2secret%",)).fetchone()["n"]
        redacted = conn.execute(
            "SELECT COUNT(*) AS n FROM chunks WHERE text LIKE '%[REDACTED%'"
        ).fetchone()["n"]
    finally:
        store._release_connection(conn)
    return {"leaked": leaked, "redacted_chunks": redacted,
            "passed": leaked == 0 and redacted > 0}


def stall_regression(tmpdir):
    """Hold a write transaction while a read query runs; it must not wait."""
    path = os.path.join(tmpdir, "stall.db")
    store = MemoryStore(path=path)
    sample = os.path.join(tmpdir, "stall.jsonl")
    write_sample(sample, 5, 10, "stall")
    store.ingest_path(sample, harness="generic", project="acceptance")
    writer = sqlite3.connect(path, timeout=10)
    writer.execute("PRAGMA busy_timeout = 10000")
    writer.execute("BEGIN IMMEDIATE")
    result = {}

    def read_query():
        started = time.perf_counter()
        store.recall("stall0000 flux capacitor", limit=5)
        result["ms"] = (time.perf_counter() - started) * 1000.0

    thread = threading.Thread(target=read_query)
    thread.start()
    time.sleep(1.5)
    writer.rollback()
    writer.close()
    thread.join(timeout=10)
    timed_out = "ms" not in result
    store.close()
    return {"read_during_write_ms": round(result.get("ms", 0), 2),
            "timed_out": timed_out,
            "passed": not timed_out and result.get("ms", 9999) < 1000}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--clients", type=int, default=20)
    parser.add_argument("--sessions", type=int, default=400)
    parser.add_argument("--messages", type=int, default=30)
    parser.add_argument("--ingest-sessions", type=int, default=6000)
    parser.add_argument("--ingest-messages", type=int, default=40)
    parser.add_argument("--db", help="memory database path (default: a temp file)")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args(argv)

    tmpdir = tempfile.mkdtemp(prefix="memory-acceptance-")
    try:
        db_path = args.db or os.path.join(tmpdir, "memory.db")
        store = MemoryStore(path=db_path)
        base_path = os.path.join(tmpdir, "base.jsonl")
        write_sample(base_path, args.sessions, args.messages, "zq")
        store.ingest_path(base_path, harness="generic", project="acceptance")
        base_stats = store.stats()
        redaction = redaction_check(store, tmpdir)

        big_path = os.path.join(tmpdir, "big.jsonl")
        write_sample(big_path, args.ingest_sessions, args.ingest_messages, "bq",
                     start=args.sessions)
        ingestor = MemoryIngestor(store, poll_s=0.2)
        ingestor.start()
        ingestor.enqueue(big_path, harness="generic", project="acceptance-bg")
        wait_until = time.time() + 10
        while time.time() < wait_until and store.status()["queue_depth"] == 0:
            time.sleep(0.1)

        # Realistic query mix: mostly specific, some medium, a few broad.
        narrow = [f"zq{session:04d}{index:04d} decision"
                  for session in range(args.sessions)
                  for index in (0, 1, 2, 7, 15, 23)]
        short = [f"zq{session:04d}{index:04d}"
                 for session in range(args.sessions)
                 for index in (0, 7, 15, 23)]
        broad = ["alpha", "acceptance memory", "flux capacitor", "decision queue"]
        fleet_queries = narrow * 3 + short * 3 + broad
        result = {}

        def fleet_phase():
            lock = threading.Lock()
            latencies, errors = run_clients(store, fleet_queries, args.clients,
                                            args.duration, lock)
            result["latencies"] = latencies
            result["errors"] = errors

        started = time.time()
        fleet_phase()
        elapsed = time.time() - started

        # Broad stress, reported not gated.
        stress = {}
        if args.duration >= 10:
            lock = threading.Lock()
            lat, err = run_clients(store, broad, max(5, args.clients // 4),
                                   min(15.0, args.duration / 4), lock)
            stress = {"clients": max(5, args.clients // 4),
                      "queries": len(lat), "failures": err,
                      "p50_ms": percentile(lat, 0.50),
                      "p95_ms": percentile(lat, 0.95),
                      "max_ms": round(max(lat), 2) if lat else None}

        ingestor.stop()
        stats = store.stats()
        latencies = result["latencies"]
        report = {
            "duration_s": round(elapsed, 2),
            "clients": args.clients,
            "queries": len(latencies),
            "failures": result["errors"],
            "p50_ms": percentile(latencies, 0.50),
            "p95_ms": percentile(latencies, 0.95),
            "p99_ms": percentile(latencies, 0.99),
            "max_ms": round(max(latencies), 2) if latencies else None,
            "p95_bar": 1000.0, "p99_bar": 2000.0, "max_bar": 2000.0,
            "sessions": stats["sessions"],
            "chunks": stats["chunks"],
            "db_bytes": sum(stats["db_bytes"].values()),
            "queue_depth": stats["queue_depth"],
            "queue": stats["queue"],
            "cache_hits": stats["queries"].get("cache_hits"),
            "broad_stress": stress,
            "redaction": redaction,
            "stall_regression": stall_regression(tmpdir),
            "base_stats": {key: base_stats[key]
                           for key in ("sessions", "chunks", "queue_depth")},
        }
        report["passed"] = (
            result["errors"] == 0
            and latencies
            and report["p95_ms"] < 1000.0
            and report["p99_ms"] < 2000.0
            and report["max_ms"] <= 2000.0
            and redaction["passed"]
            and report["stall_regression"]["passed"]
        )
        print(json.dumps(report, indent=2))
        return 0 if report["passed"] else 1
    finally:
        if args.keep:
            print(f"kept {tmpdir}", file=sys.stderr)
        else:
            shutil.rmtree(tmpdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
