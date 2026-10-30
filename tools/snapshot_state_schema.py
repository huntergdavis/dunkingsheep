#!/usr/bin/env python3
"""Snapshot the seven SQLite schemas the manifest copies (T00 decision (a)).

The manifest section "Schemas (live databases, verbatim)" has to reproduce exactly,
and it cannot read the live databases at generation time: a host whose tasks.db has
grown a table would generate a different document, which is what made
`--check` fail on 2026-10-30 (allocation_workers/offers/reviews had been added
since the manifest was written).

So the schemas are a FIXTURE: this tool writes them once, the generator reads the
fixture, and `--check-drift` compares the fixture against the live databases so it
cannot go stale silently. Snapshotting the present rather than reproducing history
is deliberate (t766 decision): the manifest describes the tree as it is.

    python3 tools/snapshot_state_schema.py --write   # live -> rust/fixtures/state
    python3 tools/snapshot_state_schema.py --check-drift
"""
import argparse
import json
import os
import pathlib
import sqlite3
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "rust/fixtures/state/schema.json"
CONFIG = pathlib.Path(os.environ.get("DUNKINGSHEEP_DIR",
                                     str(pathlib.Path.home() / ".config" / "dunkingsheep")))
DATABASES = ("tasks", "inbox", "metrics", "dispatch", "agent-state", "owner-push", "memory")


def live_schema(name):
    path = CONFIG / f"{name}.db"
    if not path.exists():
        raise SystemExit(f"missing live database {path}: point DUNKINGSHEEP_DIR at a real "
                         "install, or restore the fixture with --write on such a host")
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return [sql for (sql,) in con.execute(
            "select sql from sqlite_master where sql is not null order by name")]
    finally:
        con.close()


def live():
    return {name: live_schema(name) for name in DATABASES}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="snapshot the live schemas")
    parser.add_argument("--check-drift", action="store_true",
                        help="fail when the fixture and the live databases disagree")
    args = parser.parse_args(argv)
    if args.write:
        payload = live()
        FIXTURE.parent.mkdir(parents=True, exist_ok=True)
        FIXTURE.write_text(json.dumps(payload, indent=1, sort_keys=True) + "\n")
        print(f"snapshotted {len(payload)} databases to {FIXTURE.relative_to(ROOT)}")
        return 0
    if args.check_drift:
        if not FIXTURE.exists():
            raise SystemExit(f"no fixture at {FIXTURE}; run --write once on this host")
        recorded = json.loads(FIXTURE.read_text())
        current = live()
        drifted = [name for name in DATABASES
                   if sorted(recorded.get(name) or []) != sorted(current[name])]
        if drifted:
            print("the fixture is stale for: " + ", ".join(drifted))
            print("re-snapshot with --write, then regenerate the manifest (the drift is a "
                  "real change in the installed state and belongs in the document)")
            return 1
        print(f"fixture matches the live databases ({len(DATABASES)} checked)")
        return 0
    parser.print_help()
    return 0


if __name__ == "__main__":
    sys.exit(main())
