#!/usr/bin/env python3
"""T02 acceptance: config.rs's env table matches the tree's DUNKINGSHEEP_* names.

The rule (docs/rust/TASKS.md T02): the table in `rust/crates/ds-core/src/config.rs`
matches `grep -o 'DUNKINGSHEEP_[A-Z_]*'` over the Python tree. That is a claim about
two sets being equal, so it is checked rather than asserted in prose.

    python3 tools/check_env_table.py          # exit 1 and print the difference
    python3 tools/check_env_table.py --write  # rewrite the table in config.rs
"""
import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONFIG = ROOT / "rust/crates/ds-core/src/config.rs"
NAME = re.compile(r"DUNKINGSHEEP_[A-Z_]*")
# grep also yields the bare prefix from a f"DUNKINGSHEEP_{suffix}" format string.
# It is a prefix, not a variable, and it is excluded on both sides.
ARTEFACTS = {"DUNKINGSHEEP__", "DUNKINGSHEEP_"}   # f"DUNKINGSHEEP_{suffix}" prefixes


def tree_names():
    names = set()
    for path in list(ROOT.glob("*.py")) + [ROOT / "dunkingsheep"]:
        if path.is_file():
            names.update(NAME.findall(path.read_text(errors="ignore")))
        if path == ROOT / "dunkingsheep":
            break
    for path in ROOT.rglob("*.py"):
        if path.is_file() and ".git" not in path.parts:
            names.update(NAME.findall(path.read_text(errors="ignore")))
    return {name for name in names if name not in ARTEFACTS}


def rust_names():
    text = CONFIG.read_text()
    block = text.split("pub const ENV_VARS: &[&str] = &[", 1)[1].split("];", 1)[0]
    return set(re.findall(r'"([A-Z0-9_]+)"', block))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="rewrite the table")
    args = parser.parse_args(argv)
    wanted, have = tree_names(), rust_names()
    missing, extra = sorted(wanted - have), sorted(have - wanted)
    if not missing and not extra:
        print(f"env table matches the tree ({len(wanted)} names)")
        return 0
    for name in missing:
        print(f"missing from config.rs: {name}")
    for name in extra:
        print(f"in config.rs but not in the tree: {name}")
    if args.write:
        text = CONFIG.read_text()
        head, rest = text.split("pub const ENV_VARS: &[&str] = &[", 1)
        _, tail = rest.split("];", 1)
        lines, row = [], []
        for name in sorted(wanted):
            row.append(f'"{name}"')
            if len(", ".join(row)) > 92:
                lines.append("    " + ", ".join(row) + ",")
                row = []
        if row:
            lines.append("    " + ", ".join(row) + ",")
        CONFIG.write_text(head + "pub const ENV_VARS: &[&str] = &[\n" + "\n".join(lines) + "\n];" + tail)
        print(f"rewrote {CONFIG.relative_to(ROOT)} with {len(wanted)} names")
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
