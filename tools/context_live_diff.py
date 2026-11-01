#!/usr/bin/env python3
"""Dump the Python store's read side for a READ-ONLY COPY of a live context tree (t1395 part 2).

    python3 tools/context_live_diff.py SRC_TREE OUT_DIR

Copies SRC_TREE (never written, never opened for writing) to OUT_DIR/tree, adopts every live
entry into a fresh throwaway ledger (the live ledger is keyed by the live path, which a copy
cannot reuse), and writes OUT_DIR/expected.json in the shape of the golden fixture. Then:

    DS_CONTEXT_LIVE_DIR=OUT_DIR cargo test -p ds-core --lib -- --ignored context::golden_tests::live

compares the Rust read side with it. Queries are the fixed ones plus the first word of every
live subject, so the FTS path is exercised on the real vocabulary.
"""
import json
import os
import shutil
import sys
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))

import dunk_context  # noqa: E402


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    src, out = Path(argv[1]), Path(argv[2])
    if not src.is_dir():
        print(f"no such tree: {src}", file=sys.stderr)
        return 2
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)
    tree = out / "tree"
    shutil.copytree(src, tree, ignore=shutil.ignore_patterns(".index*", "access.jsonl"))
    ledger = dunk_context.Ledger(out / "ledger.db")
    store = dunk_context.Store(tree, ledger=ledger)
    store.adopt(ledger)
    cases = []
    for max_bytes, subject in ((dunk_context.DEFAULT_CAP, None), (600, None)):
        cases.append({"op": "digest", "args": {"max_bytes": max_bytes, "subject": subject},
                      "expected": store.digest(max_bytes=max_bytes, subject=subject)})
    entries = store.entries()
    cases.append({"op": "entries", "args": {}, "expected": [
        {k: e.get(k) for k in ("slug", "class", "subject", "stated", "body", "error")}
        for e in entries]})
    words = []
    for entry in entries:
        for word in str(entry.get("subject") or "").replace("_", "-").split("-"):
            if word.isalnum() and word.lower() not in ("and", "or", "not", "near") \
                    and word not in words:
                words.append(word)
    for query in ["deploy", "rust"] + words[:25]:
        args = {"query": query, "cls": None, "scope": None, "limit": 20}
        try:
            found = store.search(query, limit=20)
        except dunk_context.ContextError as exc:
            found = {"error": str(exc)}
        cases.append({"op": "search", "args": args, "expected": found})
    cases.append({"op": "check", "args": {}, "expected": store.check()})
    (out / "expected.json").write_text(
        json.dumps({"ledger": {"modified": [], "unrecorded": [], "adopt": True},
                    "cases": cases}, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8")
    print(f"{len(entries)} entries, {len(cases)} cases -> {out}/expected.json")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
