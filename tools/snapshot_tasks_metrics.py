#!/usr/bin/env python3
"""Snapshot the tasks.db + metrics() output a Rust port must reproduce (TASKS.md T14).

T14's acceptance is "JSON identical for the golden fixture". This builds a small tasks.db
through the PYTHON store with a pinned clock, records `Tasks.metrics(...)` for it, and writes:

    rust/fixtures/tasks/metrics.db             the store the Rust test opens a COPY of
    rust/fixtures/tasks/metrics_expected.json  {"cases": [{"args": ..., "expected": ...}]}

    python3 tools/snapshot_tasks_metrics.py --write
    python3 tools/snapshot_tasks_metrics.py --check    # fail if the Python's answer moved

Deterministic: a counter clock (one tick per call), fixed actors. The cases cover the shapes the
unit tests miss: a needs-owner question, assignees, priorities, claim and completion latency,
two actors, a requeue, a cancelled task, day buckets, a window that excludes events, and a scope
filter.
"""
import argparse
import json
import os
import shutil
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, ROOT)

import dunk_tasks  # noqa: E402

OUT = os.path.join(ROOT, "rust", "fixtures", "tasks")
START = 1_700_000_000.0
GLOBAL = "global"


def build(path):
    ticks = {"n": 0}

    def fake_now():
        ticks["n"] += 1
        return START + ticks["n"] * 37.5

    real_now = dunk_tasks._now
    dunk_tasks._now = fake_now
    try:
        store = dunk_tasks.Tasks(path)
        store.add(GLOBAL, "first", priority="high", created_by="w1:p1")
        store.add(GLOBAL, "second", priority="urgent", assignees=["w1:p1", "w2:p2"])
        store.add(GLOBAL, "third", assignees=["w1:p1"])
        asked = store.add(GLOBAL, "which port?")
        store.block(asked["id"], reason="needs-owner: which port do we target?")
        store.block(store.add(GLOBAL, "waits", priority="low")["id"], reason="waiting on PR 12")
        pane = store.add("w9:p9", "pane work", assignees=["w9:p9"])
        gone = store.add(GLOBAL, "dropped")
        store.cancel(gone["id"])
        claimed = store.claim_next(GLOBAL, "w1:p1", lease_seconds=60)["task"]
        store.complete(claimed["id"], by="w1:p1")
        second = store.claim_next(GLOBAL, "w2:p2", lease_seconds=60)["task"]
        store.complete(second["id"], by="w2:p2")
        third = store.claim_next(GLOBAL, "w1:p1", lease_seconds=60)["task"]
        store.complete(third["id"], by="w1:p1")
        spare = store.add(GLOBAL, "spare", priority="low")
        store.block(spare["id"], reason="waiting")
        store.unblock(spare["id"])
        store.claim_next("w9:p9", "w9:p9", lease_seconds=60)
        store.close()
    finally:
        dunk_tasks._now = real_now
    return START


def cases(store, start):
    until = start + 100 * 37.5
    plan = [
        {"window_s": 3600.0, "until": until, "scopes": [GLOBAL]},
        {"window_s": 3600.0, "until": until},
        {"window_s": 86400.0, "until": until, "bucket": "day", "scope_label": "visible"},
        {"window_s": 300.0, "until": until - 600.0},
        {"since": start + 100.0, "until": start + 900.0, "scopes": ["w9:p9"]},
        {"window_s": 3600.0, "until": start + 10.0},
    ]
    out = []
    for args in plan:
        out.append({"args": args, "expected": store.metrics(**args)})
    return out


def produce(workdir):
    path = os.path.join(workdir, "metrics.db")
    start = build(path)
    store = dunk_tasks.Tasks(path)
    try:
        result = {"cases": cases(store, start)}
    finally:
        store.close()
    return path, result


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", action="store_true")
    group.add_argument("--check", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as workdir:
        path, result = produce(workdir)
        text = json.dumps(result, indent=1, sort_keys=True) + "\n"
        expected_path = os.path.join(OUT, "metrics_expected.json")
        if args.write:
            os.makedirs(OUT, exist_ok=True)
            shutil.copyfile(path, os.path.join(OUT, "metrics.db"))
            with open(expected_path, "w", encoding="utf-8") as handle:
                handle.write(text)
            print(f"wrote {OUT} ({len(result['cases'])} cases)")
            return 0
        with open(expected_path, encoding="utf-8") as handle:
            current = handle.read()
        if current != text:
            print("tasks metrics fixture is stale: rerun with --write", file=sys.stderr)
            return 1
        print("tasks metrics fixture is current")
        return 0


if __name__ == "__main__":
    sys.exit(main())
