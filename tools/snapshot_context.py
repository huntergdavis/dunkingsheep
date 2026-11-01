#!/usr/bin/env python3
"""Snapshot the context tree + read-side output a Rust port must reproduce (T50 acceptance, t1395).

Builds a small context directory through the PYTHON store with a pinned calendar (supersede and
retire stamp today's date, so `_today` is patched; STALE marks compare `stated` with now(), so the
fixture uses 2020 dates for stale entries and 2999 for fresh ones), runs the read side on it, and
writes:

    rust/fixtures/context/tree/             the entries (no index, no access log)
    rust/fixtures/context/expected.json     {"ledger": {...}, "cases": [{"op", "args", "expected"}]}

    python3 tools/snapshot_context.py --write
    python3 tools/snapshot_context.py --check     # fail if the Python's answer moved

The Rust test (`context::golden_tests`) opens a COPY of `tree/`, rebuilds a ledger from the
`ledger` block (entries to skip = unrecorded, entries recorded with other text = modified) and
fails when a digest, entries listing, search result or check report differs byte for byte.

NOT covered: `stats`/`entry_stats` (they read the access log's wall-clock times) and the global
store; the live tree comparison is separate (tools/context_live_diff.py).
"""
import argparse
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, ROOT)

import dunk_context  # noqa: E402

OUT = os.path.join(ROOT, "rust", "fixtures", "context")
TODAY = "2026-10-31"
FRESH = "2999-01-01"
OLD = "2020-01-01"

DIGEST_ARGS = [
    {"max_bytes": dunk_context.DEFAULT_CAP, "subject": None},
    {"max_bytes": 420, "subject": None},
    {"max_bytes": dunk_context.DEFAULT_CAP, "subject": "deploy"},
]
SEARCHES = [
    {"query": "deploy", "cls": None, "scope": None, "limit": 20},
    {"query": "port", "cls": "fact", "scope": None, "limit": 20},
    {"query": "café OR retired", "cls": None, "scope": None, "limit": 5},
    {"query": "nothingmatchesthis", "cls": None, "scope": None, "limit": 20},
]


def build(tmp):
    root = Path(tmp) / ".dunkingsheep"
    ledger = dunk_context.Ledger(Path(tmp) / "ledger.db")
    store = dunk_context.Store(root, ledger=ledger)
    real_today = dunk_context._today
    dunk_context._today = lambda: TODAY
    try:
        gt = dict(cls="ground-truth", type_="rule", authority="owner", recheck_after="90d")
        store.add("Never edit a shared checkout.\nWork in a worktree.", subject="worktrees",
                  stated=OLD, slug="gt-worktrees", **gt)
        store.add("Production stays installed.", subject="prod", stated=OLD, slug="gt-prod", **gt)
        store.add("An old rule nobody follows.", subject="old-rule", stated=OLD,
                  slug="gt-old-rule", **gt)
        store.retire("gt-old-rule", reason="owner withdrew it")
        fact = dict(cls="fact", type_="measurement", authority="measured")
        store.add("The deploy takes 4 minutes.\nMeasured twice.", subject="deploy",
                  stated=FRESH, recheck_after="7d", slug="fact-deploy", **fact)
        store.add("The port is 8080.", subject="port", stated=OLD, recheck_after="7d",
                  slug="fact-port-old", **fact)
        store.supersede("fact-port-old", "The port is 9090.", by="w1:p1")
        store.add("Café menu rotates weekly; naive text wîth accents to stress the byte cap.",
                  subject="menu", stated=FRESH, recheck_after="30d", slug="fact-cafe", **fact)
        store.add("A fact that forgot to say when to look again.", subject="norecheck",
                  stated=FRESH, slug="fact-norecheck", **fact)
        store.add("retired but searchable", subject="retired-thing", stated=FRESH,
                  recheck_after="7d", slug="fact-retired", **fact)
        store.retire("fact-retired", reason="no longer true")
        store.add("Maybe the cache is the culprit.", cls="provisional", type_="lesson",
                  subject="cache", authority="agent", stated=OLD, slug="prov-cache")
        store.add("Maybe the cache is fine now.", cls="provisional", type_="lesson",
                  subject="cache2", authority="agent", stated=FRESH, slug="prov-cache2")
    finally:
        dunk_context._today = real_today
    # t1395: states `check` must name. `modified`: edited on disk after the daemon wrote it.
    mod = root / "fact" / "fact-cafe.md"
    mod.write_text(mod.read_text() + "\nedited by hand\n")
    # `unrecorded`: a file the daemon never wrote.
    (root / "fact" / "fact-forged.md").write_text(
        "---\nclass: fact\nsubject: forged\ntype: measurement\nscope: project\n"
        f"authority: agent\nstated: {FRESH}\nsource: \nrecheck_after: 7d\n"
        "supersedes: null\nsuperseded_by: null\n---\nA forged fact.\n")
    # A file whose front matter does not parse.
    (root / "fact" / "fact-broken.md").write_text("no front matter here\n")
    return store, root


def snapshot():
    tmp = tempfile.mkdtemp(prefix="ctx-snapshot-")
    try:
        store, root = build(tmp)
        cases = []
        for args in DIGEST_ARGS:
            text = store.digest(max_bytes=args["max_bytes"], subject=args["subject"])
            cases.append({"op": "digest", "args": args, "expected": text})
        entries = [{k: e.get(k) for k in ("slug", "class", "subject", "stated", "body", "error")}
                   for e in store.entries()]
        cases.append({"op": "entries", "args": {}, "expected": entries})
        for args in SEARCHES:
            found = store.search(args["query"], cls=args["cls"], scope=args["scope"],
                                 limit=args["limit"])
            cases.append({"op": "search", "args": args, "expected": found})
        cases.append({"op": "check", "args": {}, "expected": store.check()})
        modified = ["fact-cafe"]
        unrecorded = ["fact-forged", "fact-broken"]
        tree = Path(tmp) / "tree"
        shutil.copytree(root, tree, ignore=shutil.ignore_patterns(".index*", "access.jsonl"))
        return {"ledger": {"modified": modified, "unrecorded": unrecorded},
                "cases": cases}, tree
    except Exception:
        shutil.rmtree(tmp, ignore_errors=True)
        raise


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    opts = parser.parse_args()
    expected, tree = snapshot()
    text = json.dumps(expected, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    target = os.path.join(OUT, "expected.json")
    if opts.write:
        os.makedirs(OUT, exist_ok=True)
        shutil.rmtree(os.path.join(OUT, "tree"), ignore_errors=True)
        shutil.copytree(tree, os.path.join(OUT, "tree"))
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(text)
        print(f"wrote {target} ({len(expected['cases'])} cases)")
        return 0
    if not os.path.exists(target) or open(target, encoding="utf-8").read() != text:
        print("snapshot differs from the committed expected.json", file=sys.stderr)
        return 1
    print("snapshot matches")
    return 0


if __name__ == "__main__":
    sys.exit(main())
