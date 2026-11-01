#!/usr/bin/env python3
"""Record the Python `dunkingsheep context init/add/promote/supersede/retire --local` and
`ledger adopt` over a temp project (T44 slice 14).

Runs the REAL CLI for each step in order from <T>/proj (DUNKINGSHEEP_DIR=<T>/cfg), captures stdout /
stderr / exit code, then the content of every file under the project store afterwards. Today's
date and the temp dir are replaced by the tokens <TODAY> and <T>, so the Rust test can compare on
any day and in any directory. Output: rust/crates/ds-cli/tests/context_local.json.

    python3 tools/gen_context_local_fixture.py --write | --check
"""
import argparse
import datetime
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-cli/tests/context_local.json"
CLI = str(ROOT / "dunkingsheep")

# (env additions, argv after `context`); {A} is the slug of the first add.
STEPS = [
    ({}, ["init"]),
    ({}, ["add", "--local", "--text", "Body A", "--class", "fact", "--subject", "alpha",
          "--type", "reference", "--authority", "measured", "--source", "lab", "--stated", "2020-03-04"]),
    ({}, ["add", "--local", "--text", "Body B", "--class", "provisional", "--subject", "beta",
          "--stated", "2020-03-05"]),
    ({}, ["add", "--local", "--class", "fact", "--subject", "gamma", "--stated", "2020-03-06", "body-c.md"]),
    ({}, ["promote", "2020-03-04-alpha", "--local"]),
    ({"DUNKINGSHEEP_SUPERUSER": "1"}, ["promote", "nonexistent", "--local"]),
    ({"DUNKINGSHEEP_SUPERUSER": "1"}, ["promote", "2020-03-04-alpha", "--local"]),
    ({}, ["supersede", "2020-03-05-beta", "body-d.md", "--local"]),
    ({}, ["retire", "2020-03-06-gamma", "--reason", "obsolete\nsecond line", "--local"]),
    ({}, ["ledger", "adopt"]),
    ({"DUNKINGSHEEP_SUPERUSER": "1"}, ["ledger", "adopt"]),
    ({}, ["add", "--local", "--text", "x", "--class", "bogus", "--subject", "s"]),
    ({}, ["add", "--local", "--class", "fact", "--subject", "s", "no-such-file.md"]),
    ({"DUNKINGSHEEP_SUPERUSER": "1"}, ["init", "--json"]),
]


def norm(text, base):
    return text.replace(str(base), "<T>").replace(datetime.date.today().isoformat(), "<TODAY>")


def render():
    with tempfile.TemporaryDirectory() as tmp:
        base = pathlib.Path(tmp)
        (base / "proj").mkdir()
        (base / "proj" / "body-c.md").write_text("Body C from a file.\n")
        (base / "proj" / "body-d.md").write_text("Body D supersedes B.\n")
        steps = []
        for extra, argv in STEPS:
            env = dict(os.environ, DUNKINGSHEEP_DIR=str(base / "cfg"), NO_COLOR="1",
                       DUNKINGSHEEP_SELF="fixture")
            env.pop("DUNKINGSHEEP_SUPERUSER", None)
            env.update(extra)
            proc = subprocess.run([sys.executable, CLI, "--no-start-daemon", "context", *argv],
                                  cwd=base / "proj", env=env, capture_output=True, text=True,
                                  timeout=90)
            steps.append({"env": extra, "argv": argv, "code": proc.returncode,
                          "stdout": norm(proc.stdout, base), "stderr": norm(proc.stderr, base)})
        files = {}
        store = base / "proj" / ".dunkingsheep"
        for path in sorted(store.rglob("*")):
            if path.is_file() and path.suffix == ".md":
                files[norm(str(path.relative_to(store)), base)] = norm(path.read_text(), base)
    return json.dumps({"steps": steps, "files": files}, indent=1, sort_keys=True) + "\n"


def main():
    ap = argparse.ArgumentParser()
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    ns = ap.parse_args()
    text = render()
    if ns.write:
        OUT.write_text(text)
        print("wrote", OUT.relative_to(ROOT))
        return 0
    if not OUT.exists() or OUT.read_text() != text:
        print("context_local.json is out of date")
        return 1
    print("context_local.json matches the Python CLI")
    return 0


if __name__ == "__main__":
    sys.exit(main())
