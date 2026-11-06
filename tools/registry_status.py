#!/usr/bin/env python3
"""Command registry status table (t3385, parent t1605).

One row per command in rust/crates/ds-core/src/registry.rs: whether a Rust handler exists,
whether the production dispatcher registers it, whether Rust test code names it, and whether a
recorded fixture or tool exercises it. Everything is derived from the tree by text search, so
the table is reproducible from a commit and never hand-edited.

  python3 tools/registry_status.py            rewrite docs/rust/COMMAND_REGISTRY_STATUS.md
  python3 tools/registry_status.py --check    exit 1 when that file is stale for this tree

Columns:
  in Python           the name is a `Command(` in dunk_api.py (the frozen oracle); the others are
                      Rust-only extensions listed in tools/gen_registry_rs.py
  Rust handler        a `register_*` fn in rust/crates/ds-core/src/api.rs or api/*.rs registers it
  wired in production that `register_*` fn is called from
                      rust/crates/ds-core/src/server_runtime/registrations.rs::production_handlers()
  Rust test           the name appears as a string literal in Rust test code (tests/ dirs,
                      tests.rs, *_tests.rs, #[cfg(test)] tails; ds-core/tests/registry_parity.rs
                      excluded because it lists every name): a weak signal, present is not parity
  traffic             the name is a `cmd` in rust/fixtures or tools/ (synthetic = the fixture's
                      _meta note says the entry was hand-made, not a live recording)
"""

import argparse
import collections
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_OUT = os.path.join("docs", "rust", "COMMAND_REGISTRY_STATUS.md")
REGISTRY = os.path.join("rust", "crates", "ds-core", "src", "registry.rs")
REGISTRATIONS = os.path.join("rust", "crates", "ds-core", "src", "server_runtime", "registrations.rs")
API_DIR = os.path.join("rust", "crates", "ds-core", "src")
# rust/fixtures/traffic.jsonl's _meta lines name these as generated or hand-sanitized entries.
SYNTHETIC = {"send_text", "task_metrics", "task_reap"}

CLI_SECTION = """
## CLI not_ported sites (rust/crates/ds-cli/src/dispatch.rs)

Python parser facts (`dunkingsheep`): every nested subparser group is `required = True` EXCEPT owner, report, state, lane, retired and dispatch; `guard` and `context ledger` restrict their action with `choices`. The Rust parser (`ds-cli/src/args.rs`, generated from the same spec by tools/gen_cli_rs.py) enforces the same choices and `subcommand_required`, so an invalid or missing choice is refused with argparse's usage error before dispatch. `rust/crates/ds-cli/tests/fallback_reach.rs` (t3385 batch 2) pins this.

| site | function | fallthrough for | reachable with input Python accepts? | status (owner ruling i45201) |
|---|---|---|---|---|
| memory_aux `_` | memory_aux | memory `<other>` | no (recall/search/context/hook-context/stats/status/ingest/backfill all handled) | unreachable: parser test, left alone |
| memory `Some(other)`/`None` | memory | memory `<other>`, bare memory | no (required group) | unreachable |
| superuser `Some(other)`/`None` | superuser | superuser `<other>`, bare superuser | no | unreachable |
| context `--local` write | context | context `<write>` --local | no (dead code: handled four lines above) | unreachable |
| context not-read-side | context | context ledger status | YES (`context init` and `context ledger adopt` are handled in `context_local`) | `context ledger status` ported in batch 2 (PR #773) |
| github `Some(other)`/`None` | github | github `<other>`, bare github | no | unreachable |
| owner_command `other` | owner_command | owner `<other>` | no (`run` routes every Python owner action; bare `owner` goes to owner_stats like Python) | unreachable |
| owner_inbox `other` | owner_inbox | owner `<other>` | no | unreachable |
| owner_outbox `Some(other)` | owner_outbox | owner `<other>` | no | unreachable |
| pm `None`/`!= digest` | pm | bare pm, pm `<other>` | no (required group) | unreachable |
| guard `other` | guard | guard `<other>` | no (choices) | unreachable |
| state `other` | state | state `<other>`, bare state | bare `state`: Python group NOT required; Python raises KeyError (traceback, exit 1) | mirrored in batch 2 (PR #773) |
| read_view `other` | read_view | other view | no (`run` routes only roster/message-state/reply-stats/project-summary) | unreachable |
| delivery_queue_route `other` | delivery_queue_route | other | no | unreachable |
| poolrad `Some(other)`/`None` | poolrad | poolrad `<other>`, bare | no | unreachable |
| review `Some(other)`/`None` | review | review `<other>`, bare | no | unreachable |
| report `Some(other)`/`None` | report | report `<other>`, bare report | bare `report`: Python group NOT required; Python falls into `report run` and raises AttributeError on the missing `prose` arg (traceback, exit 1) | mirrored in batch 2 (PR #773) |
| token `Some(other)`/`None` | token | token `<other>`, bare | no | unreachable |
| run inbox `_` | run | inbox `<other>` | no (count/send/list/read/recover/status/archive/stats/fetch/ack/cancel/delete handled) | unreachable |
| run task `_` | run | task `<other>` | no (all 20 Python subcommands handled) | unreachable |
| run turn `Some(other)`/`None` | run | turn `<other>`, bare turn | no (required group) | unreachable |
| run top-level `other` | run | unknown top-level | no (every Python top-level command has an arm) | unreachable |

ds-core/src/api.rs: the former `None => not_ported(command.name)` arm and `pub fn not_ported` are gone (t3385 batch 1): `production_handlers()` registers every registry command, `production_handlers_register_every_registry_command` enforces it, and a partial (test-only) table refuses a missing handler as `internal error: no handler registered for <name>`.
"""


def read(root, path):
    with open(os.path.join(root, path), encoding="utf-8") as handle:
        return handle.read()


def registry_commands(root):
    text = read(root, REGISTRY)
    groups_at = text.index("pub static GROUPS")
    return re.findall(r'^        name: "([^"]+)"', text[:groups_at], re.M)


def python_commands(root):
    return set(re.findall(r'\bCommand\(\s*["\'](\w+)["\']', read(root, "dunk_api.py")))


def register_bodies(root):
    """{command: (set of register_* fns, handler path)} from api.rs and api/*.rs (tests excluded)."""
    fns = collections.defaultdict(set)
    handlers = {}
    for dirpath, _, files in os.walk(os.path.join(root, API_DIR)):
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            if "tests" in rel:
                continue
            text = read(root, rel)
            for match in re.finditer(r"pub fn (register_\w+)\(&mut self\)\s*\{", text):
                depth, i = 0, match.end() - 1
                start = i
                while i < len(text):
                    if text[i] == "{":
                        depth += 1
                    elif text[i] == "}":
                        depth -= 1
                        if depth == 0:
                            break
                    i += 1
                body = text[start:i]
                for command, handler in re.findall(r'\(\s*"([a-z_]+)"\s*,\s*([\w:]+)', body):
                    fns[command].add(match.group(1))
                    handlers[command] = handler
    return fns, handlers


def production_registrations(root):
    text = read(root, REGISTRATIONS)
    match = re.search(r"pub\(crate\) fn production_handlers\(\) -> Dispatcher \{(.*?)\n\}", text, re.S)
    if not match:
        raise SystemExit("production_handlers() not found in " + REGISTRATIONS)
    return set(re.findall(r"handlers\.(register_\w+)\(\)", match.group(1)))


def test_literals(root, commands):
    hits = collections.defaultdict(set)
    for dirpath, dirs, files in os.walk(os.path.join(root, "rust", "crates")):
        dirs[:] = [d for d in dirs if d != "target"]
        for name in files:
            if not name.endswith(".rs"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, name), root)
            if rel.endswith(os.path.join("tests", "registry_parity.rs")):
                continue
            text = read(root, rel)
            if "/tests/" in rel.replace(os.sep, "/") or name == "tests.rs" or name.endswith(("_tests.rs", "_test.rs")):
                part = text
            elif "#[cfg(test)]" in text:
                part = text[text.index("#[cfg(test)]"):]
            else:
                continue
            for command in commands:
                if re.search(r'"%s"' % re.escape(command), part):
                    hits[command].add(rel)
    return hits


def traffic_hits(root, commands):
    hits = collections.defaultdict(set)
    for base in ("rust/fixtures", "tools"):
        for dirpath, _, files in os.walk(os.path.join(root, base)):
            for name in files:
                rel = os.path.relpath(os.path.join(dirpath, name), root)
                try:
                    text = read(root, rel)
                except (UnicodeDecodeError, OSError):
                    continue
                for command in commands:
                    escaped = re.escape(command)
                    if (re.search(r'"cmd"\s*:\s*"%s"' % escaped, text)
                            or re.search(r"cmd\s*=\s*['\"]%s['\"]" % escaped, text)
                            or re.search(r"--commands[^\n]*\b%s\b" % escaped, text)):
                        hits[command].add(rel)
    return hits


def git_head(root):
    try:
        head = subprocess.run(["git", "-C", root, "rev-parse", "--short=12", "HEAD"],
                              capture_output=True, text=True, check=True).stdout.strip()
        dirty = subprocess.run(["git", "-C", root, "status", "--porcelain", "--", "rust", "dunk_api.py", "tools"],
                               capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unknown"
    return head + (" (plus uncommitted changes)" if dirty else "")


def render(root):
    commands = registry_commands(root)
    pycmds = python_commands(root)
    fns, handlers = register_bodies(root)
    prod = production_registrations(root)
    tests = test_literals(root, commands)
    traffic = traffic_hits(root, commands)
    rows = []
    for command in commands:
        register_fns = sorted(fns.get(command, ()))
        rows.append({
            "command": command,
            "python": "yes" if command in pycmds else "no (Rust-only)",
            "handler": "yes" if register_fns else "NO",
            "handler_fn": handlers.get(command, ""),
            "register_fn": " ".join(register_fns),
            "wired": "yes" if any(fn in prod for fn in register_fns) else "NO",
            "test": "yes" if tests.get(command) else "NO",
            "test_files": len(tests.get(command, ())),
            "traffic": ("yes" + (" (synthetic)" if command in SYNTHETIC else "")) if traffic.get(command) else "no",
            "traffic_files": ", ".join(sorted(traffic.get(command, ()))),
        })
    counts = [
        ("commands", len(rows)),
        ("python_commands", sum(r["python"] == "yes" for r in rows)),
        ("handler_present", sum(r["handler"] == "yes" for r in rows)),
        ("wired", sum(r["wired"] == "yes" for r in rows)),
        ("test_present", sum(r["test"] == "yes" for r in rows)),
        ("live_traffic", sum(r["traffic"].startswith("yes") for r in rows)),
    ]
    out = ["# Command registry status (t1605 command composition registry)", ""]
    out.append("Generated by `tools/registry_status.py` from the tree at git head %s; regenerate with "
               "`python3 tools/registry_status.py` (the `--check` flag fails when this file is stale). "
               "Column definitions are in the script's docstring." % git_head(root))
    out += ["", "## Counts", ""]
    out += ["- %s: %d" % item for item in counts]
    out += ["", "## Table", "",
            "| # | command | in Python | Rust handler | handler fn | register fn | wired in production | Rust test | traffic | traffic files |",
            "|---|---|---|---|---|---|---|---|---|---|"]
    for index, r in enumerate(rows, 1):
        out.append("| %d | %s | %s | %s | %s | %s | %s | %s (%d) | %s | %s |" % (
            index, r["command"], r["python"], r["handler"], r["handler_fn"], r["register_fn"],
            r["wired"], r["test"], r["test_files"], r["traffic"], r["traffic_files"]))
    out += ["", "## Gaps", "", "### No Rust handler"]
    out += ["- " + r["command"] for r in rows if r["handler"] != "yes"]
    out += ["", "### Handler present but not wired in production_handlers()"]
    out += ["- %s (%s)" % (r["command"], r["register_fn"]) for r in rows if r["handler"] == "yes" and r["wired"] != "yes"]
    out += ["", "### No Rust test literal"]
    out += ["- " + r["command"] for r in rows if r["test"] != "yes"]
    out += ["", "### register_* functions not called in production"]
    out += ["- " + fn for fn in sorted({fn for fset in fns.values() for fn in fset} - prod)]
    return "\n".join(out) + "\n" + CLI_SECTION, dict(counts)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=ROOT, help="repository root (default: this checkout)")
    parser.add_argument("--out", default=DEFAULT_OUT, help="output path relative to the root")
    parser.add_argument("--check", action="store_true", help="exit 1 when the file is stale")
    args = parser.parse_args(argv)
    text, counts = render(args.root)
    path = os.path.join(args.root, args.out)
    if args.check:
        try:
            current = read(args.root, args.out)
        except OSError:
            current = ""
        # The head line changes with every commit; compare everything below it.
        strip = lambda s: "\n".join(line for line in s.splitlines() if not line.startswith("Generated by"))
        if strip(current) != strip(text):
            print("%s is stale; run python3 tools/registry_status.py" % args.out, file=sys.stderr)
            return 1
        print("%s is current" % args.out)
        return 0
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    print(" ".join("%s=%d" % item for item in counts.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
