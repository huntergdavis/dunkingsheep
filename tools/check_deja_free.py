#!/usr/bin/env python3
"""Owner directive #913: deja must be fully retired, with no residual dependency.

This audit fails if an ACTIVE surface still depends on deja:

- harness MCP configs, hooks and instruction files (pi, Claude, Codex, opencode)
- systemd user units and PATH binaries
- Dunking Sheep runtime code (repo root ``*.py``, the CLI, ``packaging/``)
- scheduled dunks (the daemon's prompts)

Historical docs, tests and the archival comparison harness are *reported* but do
not fail: they describe the retired system, they do not use it.

    python3 tools/check_deja_free.py            # full audit
    python3 tools/check_deja_free.py --json     # machine-readable
    python3 tools/check_deja_free.py --repo-only  # CI: runtime only

Exit 0 when clean, 1 when an active reference remains.
"""

import argparse
import glob
import json
import os
import shutil
import sys

HOME = os.path.expanduser("~")
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

ACTIVE_FILES = (
    "~/.pi/agent/mcp.json",
    "~/.pi/agent/AGENTS.md",
    "~/.claude/settings.json",
    "~/.codex/config.toml",
    "~/.codex/AGENTS.md",
    "~/.config/opencode/opencode.jsonc",
    "~/.config/opencode/AGENTS.md",
    "~/.config/dunkingsheep/dunks.json",
)
ACTIVE_DIRS = (
    "~/.pi/agent/extensions",
    "~/.config/systemd/user",
)
PATH_BINARIES = ("deja", "deja-real", "deja-health")

RUNTIME_FILES = (
    "*.py",
    "dunkingsheep",
    "packaging/**/*",
    "ops/**/*",
)
HISTORICAL_DIRS = ("docs", "tests", "tools", "packaging/systemd")


def _expand(path):
    return os.path.expanduser(path)


def _lines_with_deja(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            return [(number, line.rstrip("\n"))
                    for number, line in enumerate(handle, 1)
                    if "deja" in line.lower()]
    except OSError:
        return []


def _is_retirement_note(line):
    lowered = line.lower()
    return any(marker in lowered for marker in
               ("retired", "do not call", "do not use", "no deja",
                "deja-free", "superseded"))


def scan_active(problems, checked):
    for pattern in ACTIVE_FILES:
        path = _expand(pattern)
        if not os.path.isfile(path):
            continue
        checked.append(path)
        for number, line in _lines_with_deja(path):
            if _is_retirement_note(line):
                continue
            problems.append(f"{path}:{number}: {line.strip()[:160]}")
    for pattern in ACTIVE_DIRS:
        base = _expand(pattern)
        if not os.path.isdir(base):
            continue
        for path in glob.glob(os.path.join(base, "**", "*"), recursive=True):
            if not os.path.isfile(path):
                continue
            checked.append(path)
            for number, line in _lines_with_deja(path):
                if _is_retirement_note(line):
                    continue
                problems.append(f"{path}:{number}: {line.strip()[:160]}")
    for name in PATH_BINARIES:
        found = shutil.which(name)
        if found:
            problems.append(f"binary on PATH: {found}")


def scan_runtime(problems, checked):
    for pattern in RUNTIME_FILES:
        for path in glob.glob(os.path.join(ROOT, pattern), recursive=True):
            if not os.path.isfile(path) or path.endswith("check_deja_free.py"):
                continue
            checked.append(path)
            for number, line in _lines_with_deja(path):
                if _is_retirement_note(line):
                    continue
                problems.append(f"{path}:{number}: {line.strip()[:160]}")


def scan_historical():
    found = {}
    for directory in HISTORICAL_DIRS:
        base = os.path.join(ROOT, directory)
        if not os.path.isdir(base):
            continue
        count = 0
        for path in glob.glob(os.path.join(base, "**", "*"), recursive=True):
            if os.path.isfile(path) and not path.endswith(".pyc"):
                count += len(_lines_with_deja(path))
        if count:
            found[directory] = count
    return found


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--repo-only", action="store_true",
                        help="skip the machine's harness configs/systemd/PATH")
    args = parser.parse_args(argv)

    problems, checked = [], []
    scan_runtime(problems, checked)
    if not args.repo_only:
        scan_active(problems, checked)
    historical = scan_historical()
    report = {
        "ok": not problems,
        "directive": "owner #913 - deja fully retired",
        "problems": problems,
        "checked": checked,
        "historical_references": historical,
        "note": "historical docs/tests/archival harness are reported, not failed",
    }
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print("deja-free audit:", "OK" if report["ok"] else "FAILED")
        for item in problems:
            print("  ACTIVE:", item)
        if historical:
            print("  historical references (ok):",
                  ", ".join(f"{k}={v}" for k, v in sorted(historical.items())))
        print(f"  checked {len(checked)} active/runtime files")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
