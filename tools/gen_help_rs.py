#!/usr/bin/env python3
"""Generate the Rust guide assets from dunk_help.py (T54, help.rs).

The guide is 700 lines of prose built by a tiny renderer; typing it again by hand is the thing
that drifts (docs/GUIDE.md is already kept in sync with it by a test). So the Rust crate embeds
the three renderings the Python produces and substitutes the version at run time:

    rust/crates/ds-core/src/help/guide.txt   guide(markdown=False), heading replaced by @HEADING@
    rust/crates/ds-core/src/help/guide.md    guide(markdown=True)   (== docs/GUIDE.md at VERSION)
    rust/crates/ds-core/src/help/short.txt   SHORT

Paragraphs the Rust guide changed or added are in tools/guide_rust_overlay.json; edit that file
(and docs/GUIDE.md, which a test keeps equal to guide.md), then run --write.

The version is rendered as the placeholder @VERSION@ (the one f-string in guide()'s heading).

    python3 tools/gen_help_rs.py --write
    python3 tools/gen_help_rs.py --check     # exit 1 and name the stale file
"""
import argparse
import json
import pathlib
import sys
import types

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-core/src/help"
OVERLAY = ROOT / "tools/guide_rust_overlay.json"
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
        "guide.txt": rust_only(plain, "guide.txt"),
        "guide.md": rust_only(dunk_help.guide(markdown=True), "guide.md"),
        "short.txt": dunk_help.SHORT,
    }


def rust_only(text, name):
    """Apply the Rust-only guide changes. dunk_help.py is frozen at retirement, so every
    paragraph the Rust guide changed or added is DATA in tools/guide_rust_overlay.json: an
    `old` passage of the Python rendering and the `new` passage that replaces it. Each `old`
    must occur exactly once, so a stale entry fails here instead of being skipped."""
    for entry in json.loads(OVERLAY.read_text(encoding="utf-8"))["replacements"]:
        if text.count(entry["old"]) != 1:
            raise SystemExit(f"{name}: a guide overlay passage occurs {text.count(entry['old'])} "
                             f"times in the Python rendering, expected once: {entry['old'][:60]!r}")
        text = text.replace(entry["old"], entry["new"])
    return text


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
