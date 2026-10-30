#!/usr/bin/env python3
"""Snapshot the memory corpus a Rust port must reproduce (TASKS.md T51).

T51's acceptance is "recall results identical on the golden corpus" - and until this
file existed there was no corpus, so the acceptance could not be measured at all. This
tool writes it once, and `--check` keeps it honest the way `gen_manifest.py` keeps the
manifest honest:

    python3 tools/snapshot_memory_corpus.py --write    # build rust/fixtures/memory/
    python3 tools/snapshot_memory_corpus.py --check     # fail if the port's target moved

WHAT THE FIXTURE PINS, and why each part is here rather than left to a live store:

* **Ranking, not just membership.** bm25 + `tokenize='porter unicode61'` with `prefix='2 3'`
  is the whole reason recall orders the way it does; a port that returns the right *set*
  in the wrong *order* has not ported recall. The expected block therefore records the
  full ordered result, including `score` and the snippets `recall()` builds.
* **Redaction at the store.** The chunk texts are inserted through `redact()`, so the
  fixture carries the SCRUBBED text - a port that forgets a pattern produces a diff
  here instead of quietly indexing a credential.
* **The AND-then-OR fallback.** `recall()` tries an AND query and falls back to OR when
  that finds nothing; both cases are in `queries` so the fallback is observable.
* **The filters.** one query each for harness, project and `since`, because those are
  SQL WHERE clauses a port can drop without any test noticing.

Deterministic by construction: fixed ids, fixed timestamps, a pinned clock (NOW, recorded
in the fixture because recall() scores a freshness term from `now`), no live
state, and an in-memory store per run. Two runs are byte-identical, which `--check`
relies on.
"""
import argparse
import json
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, ROOT)

import dunk_memory  # noqa: E402

OUT = os.path.join(ROOT, "rust", "fixtures", "memory", "corpus.json")

# recall() scores `0.25 * exp(-age_days / 30)` from time.time(); build() freezes the clock
# here (400 days after the newest chunk, so the term is < 1e-6 and the pinned scores equal
# the bare bm25 term) and the fixture records it, so a replay has no hidden input.
NOW = 1_700_000_200.0 + 400 * 86400.0

# Two sessions of different harnesses and projects, so the harness/project filters have
# something to select between; a third is older, for `since`.
SESSIONS = [
    ("s-claude-1", "claude", "dunkingsheep", "/home/hunter/workspace/dunkingsheep",
     "task queue and leases", 1_700_000_100.0),
    ("s-codex-1", "codex", "poolrad-decomp", "/home/hunter/workspace/poolrad-decomp",
     "overlay dispatcher", 1_700_000_050.0),
    ("s-pi-1", "pi", "dunkingsheep", "/home/hunter/workspace/dunkingsheep",
     "dispatch and panes", 1_700_000_000.0),
]

# Each chunk goes through redact() at insert time, exactly as ingest does. The last one
# carries three credential shapes on purpose: api-key, github token and a
# `password: ...` assignment, so the redaction is pinned by value, not by description.
CHUNKS = [
    ("s-claude-1", 0, "user", "message",
     "The task queue leases work to a pane; a lease expires and the reaper requeues it."),
    ("s-claude-1", 1, "assistant", "message",
     "Claiming walks the parent chain: a process identity is a pid plus its kernel start time."),
    ("s-codex-1", 0, "user", "message",
     "The overlay dispatcher reads slot 15 and composes 001F with exact-call stops."),
    ("s-codex-1", 1, "assistant", "message",
     "Aperture observability: 56 bus-aware models, 538 flat, so the aperture is not "
     "observable in their audits."),
    ("s-pi-1", 0, "user", "message",
     "Panes are listed by the terminal backend; a missing server is an empty world."),
    ("s-pi-1", 1, "assistant", "message",
     "Credentials must never reach the index: api_key=sk-live_abcdefghijklmnopqrstuvwx "
     "and ghp_abcdefghijklmnopqrstuvwxyz0123456789 and password: hunter2CorrectHorse."),
]

# (query, kwargs) - the kwargs are passed to recall() verbatim so the fixture records
# what each filter did.
QUERIES = [
    {"query": "lease requeue", "limit": 5},
    {"query": "kernel start time", "limit": 5},
    {"query": "aperture dispatcher slot", "limit": 5},          # AND finds nothing -> OR
    {"query": "pane", "limit": 3, "harness": "pi"},
    {"query": "queue", "limit": 5, "project": "dunkingsheep"},
    {"query": "lease", "limit": 5, "since": 1_700_000_080.0},
    {"query": "credentials index", "limit": 5},
]


def normalise(fixture):
    """Drop what is ENVIRONMENT and keep what is BEHAVIOUR.

    The first version was not reproducible and `--check` caught it (23 diff lines):
    `context.generated_at` is the wall clock, `stats.path` is a temp directory, and
    `stats.queries.{p50,max,p95,p99}` are the Python's own latency - a measurement of
    one implementation's speed, not a contract a port can or should match. Byte sizes
    go the same way. Everything left (hit ids, ORDER, bm25 scores, snippets, texts,
    counts) is what "recall results identical" means.
    """
    expected = fixture["expected"]
    expected["context"].pop("generated_at", None)
    for key in ("path", "db_bytes", "last_ingest"):
        expected["stats"].pop(key, None)
    expected["stats"].pop("queries", None) if isinstance(
        expected["stats"].get("queries"), dict) else None
    return fixture


def build():
    """The whole fixture, built in a temp store: no live state is read or written."""
    from dunk_memory import MemoryStore, redact

    with tempfile.TemporaryDirectory(prefix="memory-fixture-") as directory:
        path = os.path.join(directory, "memory.db")
        store = MemoryStore(path=path)
        try:
            # A WRITE connection, not the read pool: `_read_connection` is opened with
            # `mode=ro` + `query_only`, which is right for recall and wrong for seeding a
            # fixture ("attempt to write a readonly database", measured).
            connection = store.db
            try:
                for session_id, harness, project, cwd, title, updated in SESSIONS:
                    connection.execute(
                        "INSERT INTO sessions (id, harness, project, cwd, title,"
                        " started_at, updated_at) VALUES (?,?,?,?,?,?,?)",
                        (session_id, harness, project, cwd, title, updated - 60, updated))
                for session_id, ordinal, role, kind, text in CHUNKS:
                    scrubbed = redact(text)
                    connection.execute(
                        "INSERT INTO chunks (session_id, ordinal, role, kind, text,"
                        " token_count, sha256, created_at) VALUES (?,?,?,?,?,?,?,?)",
                        (session_id, ordinal, role, kind, scrubbed, len(scrubbed.split()),
                         __import__("hashlib").sha256(scrubbed.encode()).hexdigest(),
                         1_700_000_200.0 + ordinal))
                connection.commit()
            finally:
                store.db.commit()

            expected = []
            real_time = dunk_memory.time.time
            dunk_memory.time.time = lambda: NOW
            try:
                for spec in QUERIES:
                    params = dict(spec)
                    query = params.pop("query")
                    expected.append({
                        "query": query,
                        "kwargs": params,
                        "recall": store.recall(query, **params),
                    })
                context = store.context("lease queue pane", sessions=3,
                                        snippets_per_session=2)
                stats = store.stats()
            finally:
                dunk_memory.time.time = real_time
        finally:
            store.close()

    return {
        "now": NOW,
        "_note": ("Golden corpus for TASKS.md T51 (memory store port). Built by "
                  "tools/snapshot_memory_corpus.py; regenerate with --write, verify with "
                  "--check. Chunk texts are ALREADY redacted - a port that forgets a "
                  "SECRET_PATTERNS entry produces a diff here."),
        "sessions": [
            {"id": session_id, "harness": harness, "project": project, "cwd": cwd,
             "title": title, "updated_at": updated}
            for session_id, harness, project, cwd, title, updated in SESSIONS
        ],
        "chunks": [
            {"session_id": session_id, "ordinal": ordinal, "role": role, "kind": kind,
             "text": dunk_memory.redact(text)}
            for session_id, ordinal, role, kind, text in CHUNKS
        ],
        "expected": {
            "recall": expected,
            "context": context,
            "stats": stats,
        },
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--write", action="store_true", help="write the fixture")
    parser.add_argument("--check", action="store_true",
                        help="compare the committed fixture with a fresh build")
    args = parser.parse_args(argv)
    fixture = normalise(build())
    text = json.dumps(fixture, indent=2, sort_keys=True) + "\n"
    if args.check:
        try:
            current = open(OUT, encoding="utf-8").read()
        except OSError:
            print(f"missing fixture {OUT}: run --write", file=sys.stderr)
            return 1
        if current == text:
            print("memory corpus matches byte for byte")
            return 0
        import difflib
        diff = list(difflib.unified_diff(current.split("\n"), text.split("\n"),
                                          "committed", "generated", lineterm="", n=1))
        print("\n".join(diff[:60]))
        print(f"\n{len(diff)} diff lines: the port's target has moved", file=sys.stderr)
        return 1
    if not args.write:
        print("pass --write or --check", file=sys.stderr)
        return 2
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "w", encoding="utf-8") as handle:
        handle.write(text)
    print(f"wrote {OUT} ({len(text.splitlines())} lines, "
          f"{len(fixture['expected']['recall'])} queries)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
