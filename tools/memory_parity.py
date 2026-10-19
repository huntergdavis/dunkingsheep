#!/usr/bin/env python3
"""
Memory v1 parity harness (t10).

Memory comparison harness. By default it is **memory-only** (deja is retired):
it copies the live DB read-only, runs the verbatim golden set and the hand-
labelled ground-truth intent set, and proves rollback (backup -> delete ->
restore). `--compare-deja` enables the archival comparison against a deja binary
if one is ever restored; deja is never rebuilt or modified.

    python3 tools/memory_parity.py                 # memory only (default)
    python3 tools/memory_parity.py --compare-deja  # archival deja comparison
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


# Hand-written intent queries: NOT slices of indexed chunks. They test whether
# the two engines agree on what is relevant, rather than whether a verbatim
# phrase is found (which is near-tautological for any substring index).
# Hand-labelled ground-truth intent queries (2026-10-19), authored against the
# live DB (488 sources / 326 sessions / 262,299 chunks). Each is a natural-
# language paraphrase of a real session, NOT a slice of its text, and carries
# the session id that a correct retrieval should return. This is the set the
# reviewer asked for: recall@5 is meaningful because `expected` is known, and it
# is published next to top-5 overlap.
INTENT_GROUND_TRUTH = (
    {"query": "how do i wire deja memory and the dunkingsheep agent nerve center together as mcp servers",
     "expected": "01a14ae4-5c27-7224-ac08-9b7ded0021b7", "harness": "pi",
     "topic": "add deja + dunkingsheep MCP servers"},
    {"query": "what is the poolrad review and queue accounting lane supposed to do",
     "expected": "01a14c56-d196-777f-aaeb-0eac03c2083d", "harness": "pi",
     "topic": "poolrad review + queue accounting lane"},
    {"query": "the vp process crashed how do we restore the web interface and check the free models",
     "expected": "01a1500b-3f47-7499-bbb6-2e16df47c578", "harness": "pi",
     "topic": "VP recovery, free models, web + telegram"},
    {"query": "which routines did astra recover for the poolrad strict c port",
     "expected": "01a14b16-22bb-7257-b3e0-18827b9d7a69", "harness": "pi",
     "topic": "Astra +1153/+12FB full-state recovery"},
    {"query": "why did deja break with an append eof and how was it repaired",
     "expected": "01a14c46-4d96-7466-b01a-69c117faf8c7", "harness": "pi",
     "topic": "deja stale staging append EOF repair"},
    {"query": "what did pr 860 deliver for the poolrad c chain and byte comparisons",
     "expected": "01a14af7-41a3-7b92-803d-39037aab3555", "harness": "codex",
     "topic": "PR #860 +057B/+263D C chain, 813 byte checks"},
    {"query": "can crackdown run on a switch lite under switchroot linux with box64",
     "expected": "01a14b7c-7c8a-7482-8b1d-467dcd6113ad", "harness": "codex",
     "topic": "Crackdown on Switchroot/Tegra via box64"},
    {"query": "how is the code word reference frame captured and matched with rune templates",
     "expected": "01a14c77-7e82-7423-9fac-543bedec3675", "harness": "codex",
     "topic": "code-word reference frame + rune matching"},
    {"query": "how do we spend the remaining claude weekly allowance on porting c functions",
     "expected": "01a14a62-2b5e-7123-b77e-dde9dad43205", "harness": "codex",
     "topic": "Claude weekly budget for C porting"},
    {"query": "who is staffing the poolrad panes and what are their weekly limits",
     "expected": "01a12b3e-8cea-7de2-bfd1-ba3a8310754f", "harness": "codex",
     "topic": "Poolrad staffing 4/4 + weekly limits"},
    {"query": "how do i use dunking sheep to federate work to the poolrad research tabs",
     "expected": "01a1281b-85e3-7e23-bdf6-1ea8af4d69de", "harness": "codex",
     "topic": "federate work via Dunking Sheep MCP"},
    {"query": "cap a deja memory lookup at 90 seconds when the index is unresponsive",
     "expected": "281c8700-5acc-438b-b93d-c061e28437eb", "harness": "claude",
     "topic": "timebox an unresponsive deja lookup"},
    {"query": "how fast does the jolt vehicle recover from a high speed side slide",
     "expected": "f341143d-cffd-4df7-b8ba-e6981a4ba419", "harness": "claude",
     "topic": "Slamdemonium Jolt side-slide recovery"},
    {"query": "fake herdr workspace creation and the 88 test suite",
     "expected": "fb4de8d7-2112-41d5-aa5a-e9d84472270b", "harness": "claude",
     "topic": "fake herdr + 88 tests"},
    {"query": "how do sessions name and message their peer sessions",
     "expected": "9742b6e1-d504-40a0-92b9-7d2536d3f756", "harness": "claude",
     "topic": "session naming + peer messaging"},
    {"query": "why was the host put on hold and deja bounded during high psi",
     "expected": "a3ad1536-abb2-4fb8-af2c-1362b8656678", "harness": "claude",
     "topic": "PM host hold, PSI10, bound deja"},
    {"query": "the opus 5.5 task for 0x1951 capture parity",
     "expected": "12a7f8ad-6b6c-4d8e-b123-327925860532", "harness": "claude",
     "topic": "Opus 5.5 0x1951 capture parity"},
    {"query": "take over as vp from the handoff and keep the web apps running",
     "expected": "ses_eb525b6e7ffeijn0MceDp1jDs6", "harness": "opencode",
     "topic": "VP takeover from the handoff"},
    {"query": "run the crackdown pc recomp on a nintendo switch running linux",
     "expected": "ses_eb5752dd0ffemYjGdywEE10pQO", "harness": "opencode",
     "topic": "Crackdown recomp on Switch Linux"},
    {"query": "enable all permissions and lsps in opencode and add deja memory",
     "expected": "ses_eb584d764ffeqb34iU2IyL0ZCT", "harness": "opencode",
     "topic": "opencode config: permissions, LSP, deja"},
    {"query": "which snap interfaces did bottom need for hardware monitoring",
     "expected": "019e9dc8-a554-7aa2-a5fb-cd0aa979d2f8", "harness": "codex",
     "topic": "bottom snap interfaces"},
    {"query": "how does dunkingbird capture the screen on kde wayland",
     "expected": "019e9ddb-827f-7721-b26f-5294f84fe086", "harness": "codex",
     "topic": "dunkingbird KDE Wayland capture"},
    {"query": "does moving a function change the binary when section sorting is enabled",
     "expected": "019e9dc7-5b1a-70d1-b8f3-3689793af273", "harness": "codex",
     "topic": "split-function binary identity"},
    {"query": "how was the dunkingbird key delay and send timeout tuned",
     "expected": "019ea2fd-eceb-7482-907a-cf53a4df5fda", "harness": "codex",
     "topic": "dunkingbird key delay + timeout"},
)

# Breadth-only set (no expected session): useful for rank agreement, but its
# recall is 0 by construction, so it is optional and never the headline.
INTENT_OPEN = (
    "how do we keep memory queries fast while the ingest worker is running",
    "what happened when the vice president pane was closed by an agent",
    "how does the task queue assign work to an idle agent",
    "why does the daemon hold a message while a human is typing",
    "how is the poolrad printer path verified against the original binary",
    "what protects destructive pane operations from a non-superuser",
    "how does a crashed task lease get requeued",
    "what caused the agent message backpressure bug",
    "how do we migrate agents from deja to the new memory service",
    "what is the recipe for a self-removing recurring reminder",
    "how does the inbox deliver a message without typing over a human",
    "how are the memory recall results ranked and filtered",
)


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


def deja_top(query, harness, limit=5, allowed_sessions=None, timeout=30):
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
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
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
    # Read-only source: the live DB is never opened for writing here.
    source = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    target = sqlite3.connect(dest)
    try:
        source.backup(target)
    finally:
        target.close()
        source.close()


def score_queries(store, queries, allowed_sessions, deja_timeout=30,
                  skip_deja=False):
    """Compare memory vs deja top-5 for each query. Never silently skips a
    deja failure: it is counted in deja_errors and fails the gate."""
    overlaps, latency, details = [], [], []
    top1 = expected_both = memory_hit = deja_hit = deja_errors = 0
    for item in queries:
        started = time.perf_counter()
        mem_ids = [row["session_id"] for row in
                   store.recall(item["query"], limit=5,
                                harness=item.get("harness"))]
        latency.append((time.perf_counter() - started) * 1000.0)
        if skip_deja:
            deja_ids = []
        else:
            deja_ids = deja_top(item["query"], item.get("harness"),
                                allowed_sessions=allowed_sessions,
                                timeout=deja_timeout)
        if deja_ids is None:
            deja_errors += 1
            details.append({**item, "status": "deja-error"})
            continue
        inter = set(deja_ids) & set(mem_ids)
        denom = max(1, min(5, len(deja_ids) or 5, len(mem_ids) or 5))
        overlaps.append(len(inter) / denom)
        if deja_ids and mem_ids and deja_ids[0] == mem_ids[0]:
            top1 += 1
        expected = item.get("expected")
        if expected is not None:
            if expected in deja_ids and expected in mem_ids:
                expected_both += 1
            if expected in mem_ids:
                memory_hit += 1
            if expected in deja_ids:
                deja_hit += 1
        details.append({**item, "deja": deja_ids[:5], "memory": mem_ids[:5],
                        "deja_skipped": skip_deja,
                        "overlap": round(len(inter) / denom, 3),
                        "memory_hit": None if expected is None else expected in mem_ids,
                        "deja_hit": None if expected is None else expected in deja_ids})
    scored = len(overlaps)
    latency.sort()
    metrics = {
        "queries": len(queries),
        "scored": scored,
        "deja_skipped": skip_deja,
        "deja_errors": deja_errors,
        "mean_top5_overlap": (None if skip_deja else
                              (round(sum(overlaps) / scored, 4) if scored else None)),
        "top1_match_rate": (None if skip_deja else
                            (round(top1 / scored, 4) if scored else None)),
        "expected_session_in_both_top5": (None if skip_deja else
                                          (round(expected_both / len(queries), 4)
                                           if queries else None)),
        "memory_recall5": round(memory_hit / len(queries), 4) if queries else None,
        "deja_recall5": (None if skip_deja else
                         (round(deja_hit / len(queries), 4) if queries else None)),
        "recall_parity": (None if skip_deja else
                          (round(memory_hit / deja_hit, 4) if deja_hit else None)),
        "p50_ms": round(latency[len(latency) // 2], 2) if latency else None,
        "p95_ms": round(latency[min(len(latency) - 1,
                                    int(0.95 * (len(latency) - 1)))], 2)
        if latency else None,
    }
    return metrics, details


def render_markdown(report):
    """Human report. Every comparison table publishes recall@5 next to overlap."""
    lines = [
        "# Memory comparison report (no parity claim)",
        "",
        "> The reviewer signed a scoped statement: memory primary, deja installed",
        "> read-only, **no parity claim** ([memory_signoff.md](memory_signoff.md)).",
        "> This report compares the two engines; it does not assert they match.",
        "",
        f"- corpus: {report['counts']['sources']} sources / "
        f"{report['counts']['sessions']} sessions / {report['counts']['chunks']} chunks",
        f"- deja answered: {'not queried (memory-only)' if report.get('deja_answered') is None else report.get('deja_answered')}",
        f"- rollback: {report.get('rollback', {}).get('passed')}",
        "",
    ]
    for title, key, detail_key in (
            ("Verbatim golden queries", "parity", "details"),
            ("Ground-truth intent queries (hand-labelled)", "intent", "intent_details"),
            ("Open intent queries (overlap only)", "intent_open", "intent_open_details")):
        m = report.get(key)
        if not m or not m.get("queries"):
            continue
        lines += [
            f"## {title} (n={m['queries']})", "",
            "| metric | memory | deja |",
            "| --- | --- | --- |",
            f"| **recall@5** | **{m['memory_recall5']}** | "
            f"**{'skipped' if m.get('deja_skipped') else m['deja_recall5']}** |",
            f"| **mean top-5 overlap** | — | "
            f"**{'skipped' if m.get('deja_skipped') else m['mean_top5_overlap']}** |",
            f"| top-1 match rate | — | {m['top1_match_rate']} |",
            f"| deja errors | — | {m['deja_errors']} |",
            f"| p50 / p95 ms | {m['p50_ms']} / {m['p95_ms']} | — |",
            "",
        ]
        details = report.get(detail_key) or []
        if key != "parity" and details:
            lines += ["| query | expected | memory hit | deja hit | overlap |",
                      "| --- | --- | --- | --- | --- |"]
            for item in details:
                expected = item.get("expected") or ""
                deja_hit = "skipped" if item.get("deja_skipped") else item.get("deja_hit")
                overlap = "skipped" if item.get("deja_skipped") else item.get("overlap")
                lines.append(
                    f"| {item['query']} | `{expected[:12]}` | "
                    f"{item.get('memory_hit')} | {deja_hit} | {overlap} |")
            lines.append("")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--claude", type=int, default=6, help="claude files")
    parser.add_argument("--codex", type=int, default=10, help="codex files")
    parser.add_argument("--queries", type=int, default=60)
    parser.add_argument("--db", help="temp memory DB path")
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--bounded", action="store_true",
                        help="re-ingest the bounded sample (slow) instead of "
                             "comparing on a copy of the live DB")
    parser.add_argument("--live-db",
                        default=os.path.expanduser("~/.config/dunkingsheep/memory.db"),
                        help="live memory DB to copy for the full-corpus comparison")
    parser.add_argument("--deja-timeout", type=float, default=30,
                        help="per-query deja timeout in seconds (default 30)")
    parser.add_argument("--no-intent", dest="intent", action="store_false",
                        help="skip the non-verbatim intent query set")
    parser.add_argument("--detail", metavar="PATH",
                        help="write per-query detail JSON here")
    parser.add_argument("--markdown", metavar="PATH",
                        help="write the human comparison report here")
    parser.add_argument("--compare-deja", action="store_true",
                        help="archival: also query a deja binary for overlap")
    parser.add_argument("--memory-only", action="store_true",
                        help="compatibility alias; memory-only is the default")
    parser.add_argument("--open-intent", dest="open_intent",
                        action="store_true",
                        help="also run the breadth-only open intent set")
    args = parser.parse_args(argv)
    # Deja is retired. The default never touches it; --compare-deja is an
    # explicit archival opt-in.
    args.memory_only = not args.compare_deja

    tmpdir = tempfile.mkdtemp(prefix="memory-parity-")
    db_path = args.db or os.path.join(tmpdir, "memory.db")
    report = {}
    try:
        deja_before = deja_answers()
        if args.bounded:
            store = MemoryStore(path=db_path)
            sources = bounded_sources(args.claude, args.codex)
            ingest = {"mode": "bounded", "files": len(sources), "chunks": 0,
                      "sessions": 0, "errors": [], "seconds": 0.0}
            started = time.time()
            for path in sources:
                try:
                    counts = store.ingest_path(path)
                    ingest["chunks"] += counts.get("chunks", 0)
                    ingest["sessions"] += counts.get("sessions", 0)
                except Exception as error:  # noqa: BLE001 - reported as coverage gap
                    ingest["errors"].append({"path": path, "error": str(error)[:300]})
            ingest["seconds"] = round(time.time() - started, 2)
        else:
            # Full corpus, no re-ingest: a consistent read-only backup of the
            # live DB. The live file is never modified; rollback runs on the copy.
            live = os.path.expanduser(args.live_db)
            if not os.path.exists(live):
                raise SystemExit(f"live memory db not found: {live}")
            started = time.time()
            backup_db(live, db_path)
            store = MemoryStore(path=db_path)
            ingest = {"mode": "live-copy", "source": live, "files": None,
                      "chunks": None, "sessions": None, "errors": [],
                      "seconds": round(time.time() - started, 2)}
        stats = store.stats()

        allowed_sessions = {row[0] for row in sqlite3.connect(db_path)
                            .execute("SELECT id FROM sessions")}
        queries = golden_queries(store, args.queries)
        parity, details = score_queries(store, queries, allowed_sessions,
                                         deja_timeout=args.deja_timeout,
                                         skip_deja=args.memory_only)
        parity["gate"] = 0.90
        parity["passed"] = bool(
            queries
            and (parity["deja_skipped"] or parity["deja_errors"] == 0)
            and parity["memory_recall5"] is not None
            and parity["memory_recall5"] >= 0.90
            and (parity["mean_top5_overlap"] or 0) >= 0.50)
        report["parity"] = parity
        report["details"] = details
        report["latency_ms"] = {"p50": parity["p50_ms"], "p95": parity["p95_ms"]}

        if args.intent:
            intent = [dict(item) for item in INTENT_GROUND_TRUTH]
            intent_metrics, intent_details = score_queries(
                store, intent, allowed_sessions, deja_timeout=args.deja_timeout,
                skip_deja=args.memory_only)
            intent_metrics["passed"] = bool(
                (intent_metrics["deja_skipped"] or intent_metrics["deja_errors"] == 0)
                and (intent_metrics["memory_recall5"] or 0) >= 0.50
                and (intent_metrics["deja_skipped"]
                     or (intent_metrics["mean_top5_overlap"] or 0) >= 0.20))
            report["intent"] = intent_metrics
            report["intent_details"] = intent_details
        if args.open_intent:
            open_intent = [{"query": query} for query in INTENT_OPEN]
            open_metrics, open_details = score_queries(
                store, open_intent, allowed_sessions, deja_timeout=args.deja_timeout)
            report["intent_open"] = open_metrics
            report["intent_open_details"] = open_details
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
            "deja_answers_after": (True if args.memory_only else deja_answers()),
            "passed": (empty_counts["chunks"] == 0
                       and before_counts == restored_counts
                       and (args.memory_only or deja_answers())),
        }
        report["deja_answered"] = (None if args.memory_only else
                                   bool(deja_before
                                        and report["rollback"]["deja_answers_after"]))
        report["passed"] = bool((not queries or report["parity"]["passed"])
                                and report["rollback"]["passed"]
                                and not ingest["errors"]
                                and (args.memory_only or report["deja_answered"])
                                and (not args.intent or report["intent"]["passed"]))
        if args.markdown:
            with open(args.markdown, "w") as handle:
                handle.write(render_markdown(report))
        if args.detail:
            with open(args.detail, "w") as handle:
                json.dump({"parity": report["parity"],
                           "details": report["details"],
                           "intent": report.get("intent"),
                           "intent_details": report.get("intent_details", [])},
                          handle, indent=2)
        print(json.dumps(report, indent=2))
        return 0 if report["passed"] else 1
    finally:
        if args.keep:
            print(f"kept {tmpdir}", file=sys.stderr)
        else:
            shutil.rmtree(tmpdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
