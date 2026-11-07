#!/usr/bin/env python3
"""Golden for `watchdog --dry-run` on the fixture state (t1400, T53 slice 2).

Records the Python `dunkingsheep watchdog --dry-run --json --now NOW` output for the golden
config and the fixture state, as canonical JSON, in rust/fixtures/watchdog/golden/. The Rust test
(ds-core watchdog/golden_tests.rs) runs the same config over the same state and compares parsed
JSON with this file.

Inputs: rust/fixtures/state (inbox.db, tasks.db, roles.json) plus rust/fixtures/watchdog/extra
(dunks.json, health.json: empty, so the dunk and health rules run instead of skipping on a
missing file). The clock is pinned with --now. Paths in the output are replaced by "@STATE@".

    python3 tools/watchdog_dry_run_golden.py --write   # regenerate dry_run.expected.json
    python3 tools/watchdog_dry_run_golden.py --check   # fail if the committed JSON differs
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FIXTURES = os.path.join(ROOT, "rust", "fixtures")
GOLDEN = os.path.join(FIXTURES, "watchdog", "golden")
CONFIG = os.path.join(GOLDEN, "watchdog.golden.json")
EXPECTED = os.path.join(GOLDEN, "dry_run.expected.json")
STATE_SRC = os.path.join(FIXTURES, "state")
EXTRA_SRC = os.path.join(FIXTURES, "watchdog", "extra")
NOW = 1793448000.0
PLACEHOLDER = "@STATE@"


def record():
    with tempfile.TemporaryDirectory() as tmp:
        state = os.path.join(tmp, "state")
        os.makedirs(state)
        for name in ("inbox.db", "tasks.db", "roles.json"):
            shutil.copy(os.path.join(STATE_SRC, name), state)
        for name in ("dunks.json", "health.json"):
            shutil.copy(os.path.join(EXTRA_SRC, name), state)
        with open(CONFIG, encoding="utf-8") as handle:
            config_text = handle.read().replace(PLACEHOLDER, state)
        config_path = os.path.join(tmp, "watchdog.json")
        with open(config_path, "w", encoding="utf-8") as handle:
            handle.write(config_text)
        env = dict(os.environ, DUNKINGSHEEP_DIR=state)
        proc = subprocess.run(
            [sys.executable, "-I", os.path.join(ROOT, "dunkingsheep"), "watchdog",
             "--dry-run", "--json", "--now", repr(NOW), "--config", config_path,
             "--error-file", os.path.join(tmp, "err.txt")],
            capture_output=True, text=True, env=env, cwd=tmp)
    if proc.returncode != 0:
        raise SystemExit(f"watchdog --dry-run exited {proc.returncode}: {proc.stderr}")
    text = proc.stdout.replace(state, PLACEHOLDER)
    report = json.loads(text)
    return json.dumps(report, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    opts = parser.parse_args()
    text = record()
    if opts.write:
        with open(EXPECTED, "w", encoding="utf-8") as handle:
            handle.write(text)
        print(f"wrote {EXPECTED}")
        return 0
    if not os.path.exists(EXPECTED) or open(EXPECTED, encoding="utf-8").read() != text:
        print("dry-run output differs from the committed dry_run.expected.json", file=sys.stderr)
        return 1
    print("dry-run output matches")
    return 0


if __name__ == "__main__":
    sys.exit(main())
