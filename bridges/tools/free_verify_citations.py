#!/usr/bin/env python3
"""Check a free-model report's CITATIONS mechanically, before any human reads its prose.

A free model writes confidently. Its first observed report on 2026-10-23 opened with
"AGREE" and then cited two paths that do not exist (`docs/re/ovl_0025_06af.md`,
`src/recovered/ovl_0025_06af.c`) plus an objdump command with an invented overlay base.
The prose was well formatted, correctly shaped, and wrong.

So a free report is a DRAFT until every path it names exists and every address it names
is inside the body under discussion. This tool answers exactly that, and nothing else:

  * every path-like token in the report  -> exists? (relative to the repo root)
  * every 0xNNNN / NN:NNNN address token  -> inside the body's declared extent?

It does not judge correctness of reasoning, and it must not be read as an endorsement:
"citations all resolve" means the report is worth reading, not that it is right.

Exit codes: 0 all citations resolve, 1 at least one is false, 2 usage/body-parse error.
"""
from __future__ import annotations

import os
import re
import sys
from pathlib import Path

ROOT = Path(os.environ.get("POOLRAD_REPO") or Path(__file__).resolve().parents[1])

PATH_RE = re.compile(r"\b((?:docs|src|tools|tests|assets|data|resources)/[A-Za-z0-9_./-]+\.[A-Za-z0-9]+)\b")
# A path named INSIDE a search/probe command is not a claim about the repo; it is the model
# looking for it. Counting probes as citations produced a false "FABRICATED" verdict on
# 2026-10-23 for a report whose every path was a `find`/`cat ... ||` probe.
PROBE_RE = re.compile(r"(?:^|[\s\"'])(?:cat|find|ls|grep|head|tail|sed|awk|xargs|rg|test)\b[^\n]*$")
TOOLCALL_RE = re.compile(r'"action"\s*:|"cmd"\s*:|^\s*\$ ')
ADDR_RE = re.compile(r"\b(?:0x)?([0-9A-Fa-f]{4,5})(?::(?:0x)?([0-9A-Fa-f]{4,5}))?\b")


def body_extent(body_id: str) -> tuple[int, int] | None:
    """Parse OVL_SSSS_OOOO / RES_SSSS_OOOO into a [start, end) offset range."""
    m = re.fullmatch(r"(?:OVL|RES)_([0-9A-Fa-f]{4})_([0-9A-Fa-f]{4})", body_id)
    if not m:
        return None
    start = int(m.group(2), 16)
    return start, start + 0x400  # default window: report addresses should be near the body


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print("usage: free_verify_citations.py <BODY_ID> <report.md> [...]", file=sys.stderr)
        return 2
    body, reports = argv[1], [Path(p) for p in argv[2:]]
    extent = body_extent(body)
    if extent is None:
        print(f"REFUSED: cannot parse body id {body!r}", file=sys.stderr)
        return 2
    lo, hi = extent
    bad = 0
    for rep in reports:
        try:
            text = rep.read_text(errors="replace")
        except OSError as exc:
            print(f"REFUSED: {rep}: {exc.strerror}")
            return 2
        cited_text = []
        for line in text.splitlines():
            if TOOLCALL_RE.search(line) or PROBE_RE.search(line):
                continue
            cited_text.append(line)
        paths = sorted({m for line in cited_text for m in PATH_RE.findall(line)})
        probed = sorted({m for line in text.splitlines() for m in PATH_RE.findall(line)} - set(paths))
        missing = [p for p in paths if not (ROOT / p).exists()]
        addrs = set()
        for a, b in ADDR_RE.findall(text):
            for tok in (a, b):
                if tok:
                    addrs.add(int(tok, 16))
        inside = sorted(a for a in addrs if lo <= a < hi)
        outside = sorted(a for a in addrs if not (lo <= a < hi))
        print(f"== {rep}")
        print(f"   paths cited : {len(paths)}  missing: {len(missing)}   (probes ignored: {len(probed)})")
        for p in missing:
            print(f"      MISSING  {p}")
            bad += 1
        print(f"   addresses   : {len(addrs)}  in [{lo:#06x},{hi:#06x}): {len(inside)}  outside: {len(outside)}")
        if missing:
            print("   VERDICT: FABRICATED CITATIONS - do not read the prose as evidence.")
        elif not paths:
            print("   VERDICT: NO CHECKABLE CITATIONS - treat as prose, not evidence.")
        else:
            print("   VERDICT: citations resolve. Worth reading; NOT an endorsement of the reasoning.")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
