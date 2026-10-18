#!/usr/bin/env python3
"""
Memory v1 parity harness (t10).

Bounded backfill of real sessions into a TEMP memory database (never the live
one, never deja), a golden query set, and a top-5 session overlap comparison
against `deja --json`. Also proves rollback: a consistent DB backup, an empty
re-open, a restore, and deja still answering.

    python3 tools/memory_parity.py --queries 60

Deja is read-only here; it is never rebuilt or modified.
"""

import argparse
import glob
import json
import os
import random
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from dunk_memory import MemoryIngestor, MemoryStore  # noqa: E402

DEJA_BIN = os.environ.get("DEJA_BIN", os.path.expanduser("~/.local/bin/deja"))
HOME = os.path.expanduser("~")
STOPWORDS = {
    "the", "and", "for", "that", "this", "with", "from", "you", "your", "not",
    "are", "was", "were", "have", "has", "had", "but", "can", "will", "would",
    "there", "then", "than", "they", "them", "into", "out", "all", "any",
    "one", "two", "use", "using", "used", "get", "got", "make", "made",
    "when", "what", "which", "while", "where", "how", "why", "also", "just",
    "should", "could", "about", "over", "more", "most", "some", "such",
    "only", "other", "each", "because", "before", "after", "been", "being",
}


def bounded_sources(claude, codex):
    sources = []
    sources += sorted(glob.glob(os.path.join(HOME, ".pi/agent/sessions/**/*.jsonl"),
                                recursive=True))
    claude_files = sorted(glob.glob(os.path.join(HOME, ".claude/projects/**/*.jsonl"),
                                    recursive=True), key=os.path.getmtime, reverse=True)
    sources += claude_files[:claude]
    codex_files = sorted(glob.glob(os.path.join(HOME, ".codex/sessions/**/*.jsonl"),
                                    recursive=True), key=os.path.getmtime, reverse=True)
    sources += codex_files[:codex]
    opencode = os.path.join(HOME, ".local/share/opencode/opencode.db")
    if os.path.exists(opencode):
        sources.append(opencode)
    return sources


def golden_queries(store, count, seed=7):
    rng = random.Random(seed)
    conn = store._read_connection()
    try:
        rows = conn.execute(
            "SELECT session_id, text FROM chunks WHERE length(text) > 300 "
            "ORDER BY RANDOM() LIMIT ?", (count * 6,)).fetchall()
        harnesses = {row["id"]: row["harness"] for row in conn.execute(
            "SELECT id, harness FROM sessions").fetchall()}
    finally:
        store._release_connection(conn)
    queries = []
    seen = set()
    for row in rows:
        words = [word for word in re.findall(r"[A-Za-z0-9_]{4,}", row["text"])
                 if word.lower() not in STOPWORDS and not word.startswith("http")]
        if len(words) < 10:
            continue
        start = rng.randint(0, len(words) - 5)
        phrase = " ".join(words[start:start + 5])
        if phrase.lower() in seen:
            continue
        seen.add(phrase.lower())
        harness = harnesses.get(row["session_id"])
        if harness not in ("claude", "codex", "opencode"):
            continue
        queries.append({"query": phrase, "expected": row["session_id"],
                        "harness": harness})
        if len(queries) >= count:
            break
    return queries


def deja_top(query, harness, limit=5, allowed_sessions=None):
    """Deja's ranked session ids, optionally restricted to the ingested files.

    The bounded backfill has a subset of deja's corpus, so the fair parity test
    compares rankings on the same files: deja results whose path was not
    ingested are skipped.
    """
    cmd = [DEJA_BIN, "--json"]
    if harness:
        cmd += ["--harness", harness]
    cmd.append(query)
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    text = proc.stdout.strip()
    if text == "null" or not text:
        return []
    start = text.find("[")
    if start < 0:
        return None
    try:
        data = json.loads(text[start:])
    except ValueError:
        return None
    ids = []
    for item in data if isinstance(data, list) else []:
        session = (item or {}).get("session") or {}
        session_id = session.get("id")
        if not session_id:
            continue
        if allowed_sessions is not None and session_id not in allowed_sessions:
            continue
        ids.append(session_id)
        if len(ids) >= limit:
            break
    return ids


def deja_answers():
    return deja_top("dunkingsheep task system", "claude") is not None


def backup_db(path, dest):
    source = sqlite3.connect(path)
    target = sqlite3.connect(dest)
    try:
        source.backup(target)
    finally:
        target.close()
        source.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claude", type=int, default=6, help="claude files")
    parser.add_argument("--codex", type=int, default=10, help="codex files")
    parser.add_argument("--queries", type=int, default=60)
    parser.add_argument("--db", help="temp memory DB path")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args(argv)

    tmpdir = tempfile.mkdtemp(prefix="memory-parity-")
    db_path = args.db or os.path.join(tmpdir, "memory.db")
    report = {}
    try:
        deja_before = deja_answers()
        store = MemoryStore(path=db_path)
        sources = bounded_sources(args.claude, args.codex)
        ingest = {"files": len(sources), "chunks": 0, "sessions": 0,
                  "errors": [], "seconds": 0.0}
        started = time.time()
        for path in sources:
            try:
                counts = store.ingest_path(path)
                ingest["chunks"] += counts.get("chunks", 0)
                ingest["sessions"] += counts.get("sessions", 0)
            except Exception as error:  # noqa: BLE001 - reported as coverage gap
                ingest["errors"].append({"path": path, "error": str(error)[:300]})
        ingest["seconds"] = round(time.time() - started, 2)
        stats = store.stats()

        allowed_sessions = {row[0] for row in sqlite3.connect(db_path)
                            .execute("SELECT id FROM sessions")}
        queries = golden_queries(store, args.queries)
        overlaps = []
        top1 = 0
        expected_both = 0
        memory_hit = 0
        deja_hit = 0
        latency = []
        details = []
        for item in queries:
            t = time.perf_counter()
            mem_ids = [row["session_id"] for row in
                       store.recall(item["query"], limit=5,
                                    harness=item["harness"])]
            latency.append((time.perf_counter() - t) * 1000.0)
            deja_ids = deja_top(item["query"], item["harness"],
                                 allowed_sessions=allowed_sessions)
            if deja_ids is None:
                details.append({**item, "status": "deja-error"})
                continue
            inter = set(deja_ids) & set(mem_ids)
            denom = max(1, min(5, len(deja_ids) or 5, len(mem_ids) or 5))
            overlaps.append(len(inter) / denom)
            if deja_ids and mem_ids and deja_ids[0] == mem_ids[0]:
                top1 += 1
            if item["expected"] in deja_ids and item["expected"] in mem_ids:
                expected_both += 1
            if item["expected"] in mem_ids:
                memory_hit += 1
            if item["expected"] in deja_ids:
                deja_hit += 1
            details.append({**item, "deja": deja_ids[:5], "memory": mem_ids[:5],
                            "overlap": round(len(inter) / denom, 3)})
        scored = len(overlaps)
        report["parity"] = {
            "queries": len(queries),
            "scored": scored,
            "mean_top5_overlap": round(sum(overlaps) / scored, 4) if scored else None,
            "top1_match_rate": round(top1 / scored, 4) if scored else None,
            "expected_session_in_both_top5": round(expected_both / len(queries), 4)
            if queries else None,
            "memory_recall5": round(memory_hit / len(queries), 4) if queries else None,
            "deja_recall5": round(deja_hit / len(queries), 4) if queries else None,
            "recall_parity": round(memory_hit / deja_hit, 4) if deja_hit else None,
            "gate": 0.90,
            "passed": bool(queries and memory_hit / len(queries) >= 0.90),
        }
        latency.sort()
        report["latency_ms"] = {
            "p50": round(latency[len(latency) // 2], 2) if latency else None,
            "p95": round(latency[min(len(latency) - 1, int(0.95 * (len(latency) - 1)))], 2)
            if latency else None,
        }
        report["counts"] = {"sources": stats["sources"], "sessions": stats["sessions"],
                            "chunks": stats["chunks"],
                            "db_bytes": sum(stats["db_bytes"].values()),
                            "queue": stats["queue"]}
        report["ingest"] = ingest
        report["deja_answered_before"] = deja_before

        # Rollback proof.
        backup = db_path + ".bak"
        before_counts = {k: stats[k] for k in ("sources", "sessions", "chunks")}
        backup_db(db_path, backup)
        store.close()
        for suffix in ("", "-wal", "-shm"):
            if os.path.exists(db_path + suffix):
                os.remove(db_path + suffix)
        empty = MemoryStore(path=db_path)
        empty_counts = {k: empty.stats()[k] for k in ("sources", "sessions", "chunks")}
        empty.close()
        shutil.copyfile(backup, db_path)
        for suffix in ("-wal", "-shm"):
            if os.path.exists(db_path + suffix):
                os.remove(db_path + suffix)
        restored = MemoryStore(path=db_path)
        restored_counts = {k: restored.stats()[k] for k in
                           ("sources", "sessions", "chunks")}
        restored.close()
        report["rollback"] = {
            "before": before_counts,
            "after_delete": empty_counts,
            "after_restore": restored_counts,
            "restored_equal": before_counts == restored_counts,
            "deja_answers_after": deja_answers(),
            "passed": (empty_counts["chunks"] == 0
                       and before_counts == restored_counts
                       and deja_answers()),
        }
        report["passed"] = bool(report["parity"]["passed"]
                                and report["rollback"]["passed"]
                                and not ingest["errors"])
        print(json.dumps(report, indent=2))
        return 0 if report["passed"] else 1
    finally:
        if args.keep:
            print(f"kept {tmpdir}", file=sys.stderr)
        else:
            shutil.rmtree(tmpdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
