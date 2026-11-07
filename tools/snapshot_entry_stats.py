#!/usr/bin/env python3
"""Byte-level golden for the Python store's entry_stats (t1395 gap 1, t3565).

tools/snapshot_context.py pins the fixture tree, the clocks and the access log, then records
`store.entry_stats()` as parsed JSON in expected.json. This tool writes the SAME entry_stats value
as text, in the canonical form that snapshot_context.py uses for expected.json
(`json.dumps(indent=2, sort_keys=True, ensure_ascii=False)`), so the Rust test can compare bytes.

    python3 tools/snapshot_entry_stats.py --write   # regenerate rust/fixtures/context/entry_stats.json
    python3 tools/snapshot_entry_stats.py --check   # fail if the committed bytes differ
"""

import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import snapshot_context  # noqa: E402  (fixture, clocks and access log live there)

OUT = snapshot_context.OUT


def entry_stats_text():
    with snapshot_context.fixed_clock(snapshot_context.CLOCK):
        expected, _tree = snapshot_context.snapshot()
    cases = [case for case in expected["cases"] if case["op"] == "entry_stats"]
    if len(cases) != 1:
        raise SystemExit(f"expected exactly one entry_stats case, found {len(cases)}")
    return json.dumps(cases[0]["expected"], indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    opts = parser.parse_args()
    text = entry_stats_text()
    target = os.path.join(OUT, "entry_stats.json")
    if opts.write:
        os.makedirs(OUT, exist_ok=True)
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(text)
        print(f"wrote {target}")
        return 0
    if not os.path.exists(target) or open(target, encoding="utf-8").read() != text:
        print("entry_stats differs from the committed entry_stats.json", file=sys.stderr)
        return 1
    print("entry_stats matches")
    return 0


if __name__ == "__main__":
    sys.exit(main())
