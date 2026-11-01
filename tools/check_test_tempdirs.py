#!/usr/bin/env python3
"""Fail when Rust code builds a fixture directory under the shared temp dir by hand (t1490).

Tests used to write `std::env::temp_dir().join(format!("ds-<tag>-{}", std::process::id()))` and
never remove it: the pid differs on every `cargo test` run, so each run left a fresh set behind
(31,557 directories, 3.8 GB, filled the host's tmpfs). Use `crate::testsupport::dir(tag)` (a
`tempfile::TempDir`, removed on drop) or `crate::testsupport::Scratch` in ds-core, and
`tempfile::Builder` / `tempfile::tempdir()` elsewhere.

A line that really needs the shared temp dir carries `// tempdir-ok: <reason>`.

    python3 tools/check_test_tempdirs.py            # exit 1 on a violation
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CRATES = ROOT / "rust" / "crates"
PATTERN = re.compile(r"\btemp_dir\s*\(\s*\)")
ALLOWED_FILES = {"rust/crates/ds-core/src/testsupport.rs"}  # it documents the pattern it replaces


def violations(root=CRATES, base=ROOT):
    found = []
    for path in sorted(root.rglob("*.rs")):
        rel = path.relative_to(base).as_posix()
        if rel in ALLOWED_FILES or "/target/" in rel:
            continue
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if PATTERN.search(line) and "tempdir-ok:" not in line:
                found.append((rel, number, line.strip()))
    return found


def main():
    found = violations()
    for rel, number, text in found:
        print(f"{rel}:{number}: {text}")
    if found:
        print(
            f"\n{len(found)} use(s) of temp_dir() in Rust sources. Fixtures must clean up after "
            "themselves: crate::testsupport::dir(tag) / Scratch (ds-core) or tempfile::tempdir() "
            "(t1490). A line that truly needs the shared temp dir carries "
            "'// tempdir-ok: <reason>'.",
            file=sys.stderr,
        )
        return 1
    print("no temp_dir() fixtures in rust/crates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
