#!/usr/bin/env python3
"""Record the Python `dunkingsheep context ...` read commands over a small fixed store (T44).

Builds a project store and a global store with the real Python (`dunk_context.Store.add`,
fixed `stated` dates), runs the REAL CLI for each step in order from the project directory
(cwd matters: the project store is `./.dunkingsheep`), and writes the store files plus every
step's stdout/exit code to rust/crates/ds-cli/tests/context_cmd.json. The Rust test
materialises the same files and runs the same steps through `context_cmd::context_read`.

Temp paths in output become the token <T>. Only the non-JSON (human) output is recorded.

    python3 tools/gen_context_cmd_fixture.py --write | --check
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-cli/tests/context_cmd.json"
CLI = str(ROOT / "dunkingsheep")

# (argv after `context`, in the order they run: reads log access, digest injects)
STEPS = [
    ["get", "flux-capacitor-rule"],
    ["get", "2020-01-02-global-truth"],
    ["get", "no-such-entry"],
    ["search", "flux"],
    ["search", "flux", "--class", "fact"],
    ["search", "nothingmatchesthis"],
    ["ledger", "status"],
    ["index"],
    ["check"],
    ["digest"],
    ["digest", "--subject", "flux-capacitor-rule"],
    ["digest", "--max-bytes", "400"],
    ["stats"],
    ["stats", "--global"],
    ["check", "--project"],
]


def build(base):
    sys.path.insert(0, str(ROOT))
    os.environ["DUNKINGSHEEP_DIR"] = str(base / "cfg")
    import dunk_context
    project = base / "proj"
    project.mkdir()
    store = dunk_context.Store(project / ".dunkingsheep")
    store.ledger = None
    store.add("Flux capacitors need 1.21 gigawatts.", cls="fact", subject="flux-capacitor-rule",
              type_="reference", authority="measured", source="lab", stated="2020-03-04",
              slug="flux-capacitor-rule")
    store.add("Draft idea about flux routing.", cls="provisional", subject="flux-routing",
              type_="reference", authority="agent", stated="2020-03-05", slug="flux-routing")
    store.add("Old flux note.", cls="history", subject="old-flux",
              type_="reference", authority="agent", stated="2019-01-01", slug="old-flux")
    # More than 10 AND more than 20 hits for "flux": the CLI passes no limit, so the Python
    # default (20) applies. A fixture with 3 hits cannot see the difference (review t1427).
    for n in range(25):
        store.add("Flux bulk note %d %s" % (n, "padding " * (n % 7)), cls="fact",
                  subject="bulk-flux-%02d" % n, type_="reference", authority="agent",
                  stated="2020-04-%02d" % (n % 28 + 1), slug="bulk-flux-%02d" % n)
    glob = dunk_context.Store(global_store=True)
    glob.ledger = None
    glob.add("Never publish secrets.", cls="ground-truth", subject="global-truth",
             type_="principle", authority="owner", source="owner", stated="2020-01-02",
             slug="2020-01-02-global-truth")


def snapshot_files(base):
    files = {}
    for sub in ("proj", "cfg"):
        for path in sorted((base / sub).rglob("*")):
            if path.is_file() and path.suffix == ".md":
                files[str(path.relative_to(base))] = path.read_text()
    return files


def run_steps(base):
    env = dict(os.environ, DUNKINGSHEEP_DIR=str(base / "cfg"), NO_COLOR="1",
               DUNKINGSHEEP_SELF="fixture")
    out = []
    for argv in STEPS:
        proc = subprocess.run([sys.executable, CLI, "--no-start-daemon", "context", *argv],
                              cwd=base / "proj", env=env, capture_output=True, text=True,
                              timeout=60)
        out.append({"argv": argv, "code": proc.returncode,
                    "stdout": proc.stdout.replace(str(base), "<T>"),
                    "stderr": proc.stderr.replace(str(base), "<T>")})
    return out


def render():
    with tempfile.TemporaryDirectory() as tmp:
        base = pathlib.Path(tmp)
        build(base)
        files = snapshot_files(base)
        steps = run_steps(base)
    return json.dumps({"files": files, "steps": steps}, indent=1, sort_keys=True) + "\n"


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
        print("context_cmd.json is out of date")
        return 1
    print("context_cmd.json matches the Python CLI")
    return 0


if __name__ == "__main__":
    sys.exit(main())
