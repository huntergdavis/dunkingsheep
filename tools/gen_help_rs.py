#!/usr/bin/env python3
"""Generate the Rust guide assets from dunk_help.py (T54, help.rs).

The guide is 700 lines of prose built by a tiny renderer; typing it again by hand is the thing
that drifts (docs/GUIDE.md is already kept in sync with it by a test). So the Rust crate embeds
the three renderings the Python produces and substitutes the version at run time:

    rust/crates/ds-core/src/help/guide.txt   guide(markdown=False), heading replaced by @HEADING@
    rust/crates/ds-core/src/help/guide.md    guide(markdown=True)   (== docs/GUIDE.md at VERSION)
    rust/crates/ds-core/src/help/short.txt   SHORT

The version is rendered as the placeholder @VERSION@ (the one f-string in guide()'s heading).

    python3 tools/gen_help_rs.py --write
    python3 tools/gen_help_rs.py --check     # exit 1 and name the stale file
"""
import argparse
import pathlib
import sys
import types

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-core/src/help"
PLACEHOLDER = "@VERSION@"
# The plain guide underlines its heading with `=` * len(heading), so the underline depends on the
# version's length. The first heading + underline are therefore stored as this marker, and the
# Rust side rebuilds them for the real version.
HEADING_MARKER = "@HEADING@\n"


def render():
    # dunk_help does `from dunk_core import VERSION`; the daemon module is not needed (and not
    # wanted: importing it pulls in the whole daemon), only the name.
    stub = types.ModuleType("dunk_core")
    stub.VERSION = PLACEHOLDER
    sys.modules["dunk_core"] = stub
    sys.path.insert(0, str(ROOT))
    import dunk_help
    plain = dunk_help.guide(markdown=False)
    heading = f"Dunking Sheep {PLACEHOLDER} guide"
    expected = f"{heading}\n{'=' * len(heading)}\n"
    if not plain.startswith(expected):
        raise SystemExit("guide() no longer opens with the underlined version heading; update this tool")
    plain = HEADING_MARKER + plain[len(expected):]
    return {
        "guide.txt": plain,
        "guide.md": dunk_help.guide(markdown=True),
        "short.txt": dunk_help.SHORT,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--write", action="store_true")
    group.add_argument("--check", action="store_true")
    args = parser.parse_args()
    files = render()
    stale = []
    for name, text in files.items():
        path = OUT / name
        if args.write:
            OUT.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        elif not path.exists() or path.read_text(encoding="utf-8") != text:
            stale.append(str(path.relative_to(ROOT)))
    if stale:
        print("stale (run tools/gen_help_rs.py --write): " + ", ".join(stale))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
