#!/usr/bin/env python3
"""Free-model leaderboard (owner #1426): ranked by verified output, not by runs.

Read at collection time, nothing hand-entered:
- data/free_runs/<BODY>/verdict.txt + run.log: one decode attempt per body
  directory (a re-run overwrites it), with the byte gate's verdict;
- data/bakeoff/<stamp>/results.tsv: one row per attempt in a bake-off.

Outcome per attempt, from the gate's own words: VERIFIED (BYTES MATCH: n bytes,
byte-identical to GAME.OVR), WRONG BYTES (BYTES DIFFER), NO DUMP (nothing
checkable), UNCHECKED (claimed bytes the gate did not test), EMPTY (no output).

A verified decode means the bytes matched the game file. It does not mean the
model's interpretation was right, and it is not a credit. Accepted and assisted
lines need per-line attribution from merged PRs or the credit table; none
exists yet, so those cells are null with that reason.
"""

from __future__ import annotations

import os
import re
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo

ROOT = Path(__file__).resolve().parent.parent
FREE_RUNS = Path(os.environ.get("VP_FREE_RUNS", ROOT / "data" / "free_runs"))
BAKEOFF = Path(os.environ.get("VP_BAKEOFF", ROOT / "data" / "bakeoff"))
LOCAL = ZoneInfo("America/Los_Angeles")
NO_ATTRIBUTION = "no per-line attribution exists yet: no merged PR or credit-table row names the model that seeded a body"
MATCH = re.compile(r"BYTES MATCH:\s*(\d+)\s*bytes")
OUTCOMES = ("verified", "wrong_bytes", "no_dump", "unchecked", "empty")


def iso(stamp):
    return stamp.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ") if stamp else None


def cell(value, as_of, reason=None):
    return {"value": value, "as_of": iso(as_of) if as_of else None, "reason": reason}


def tier(model):
    """free by catalog name; opencode-go rides the fixed Go subscription; anything else is metered."""
    name = (model or "").rsplit("/", 1)[-1]
    if name == "big-pickle" or name.endswith(":free") or name.endswith("-free"):
        return "free"
    return "subscription" if (model or "").startswith("opencode-go/") else "metered"


def outcome(text):
    """(outcome, verified bytes) from a verdict line or file, in the gate's own words."""
    found = MATCH.search(text)
    if found:
        return "verified", int(found.group(1))
    upper = text.upper()
    for needle, name in (("BYTES DIFFER", "wrong_bytes"), ("NO DUMP", "no_dump"),
                         ("UNCHECKED", "unchecked"), ("EMPTY", "empty")):
        if needle in upper:
            return name, 0
    return None, 0


def free_run_attempts(root):
    attempts, problems = [], []
    if not root.is_dir():
        return attempts, [f"{root} absent"]
    for verdict in sorted(root.glob("*/verdict.txt")):
        text = verdict.read_text(errors="replace")
        model = re.search(r"^model:\s*(\S+)", text, re.M)
        kind, size = outcome(text)
        if not model or kind is None:
            problems.append(f"{verdict.parent.name}: no model or no gate verdict")
            continue
        when = None
        log = verdict.parent / "run.log"
        stamp = re.search(r" at (\d{4}-\d\d-\d\d \d\d:\d\d:\d\d)", log.read_text(errors="replace")) if log.is_file() else None
        if stamp:
            when = datetime.strptime(stamp.group(1), "%Y-%m-%d %H:%M:%S").replace(tzinfo=LOCAL)
        else:
            when = datetime.fromtimestamp(verdict.stat().st_mtime, timezone.utc)
        attempts.append({"model": model.group(1), "body": verdict.parent.name, "outcome": kind,
                         "bytes": size, "at": when, "source": "free_runs"})
    return attempts, problems


def bakeoff_attempts(root):
    attempts, problems = [], []
    if not root.is_dir():
        return attempts, [f"{root} absent"]
    for results in sorted(root.glob("*/results.tsv")):
        try:
            when = datetime.strptime(results.parent.name, "%Y%m%d-%H%M%S").replace(tzinfo=LOCAL)
        except ValueError:
            when = datetime.fromtimestamp(results.stat().st_mtime, timezone.utc)
        for number, line in enumerate(results.read_text(errors="replace").splitlines(), 1):
            fields = line.split("\t")
            if len(fields) < 3 or not fields[0].strip():
                continue
            kind, size = outcome(fields[2])
            if kind is None:
                problems.append(f"{results.parent.name} line {number}: unrecognised result")
                continue
            attempts.append({"model": fields[0].strip(), "body": fields[1].strip(), "outcome": kind,
                             "bytes": size, "at": when, "source": f"bakeoff {results.parent.name}"})
    return attempts, problems


def collect(*, now=None, free_runs=None, bakeoff=None):
    now = now or datetime.now(timezone.utc)
    runs, problems_a = free_run_attempts(Path(free_runs or FREE_RUNS))
    bake, problems_b = bakeoff_attempts(Path(bakeoff or BAKEOFF))
    attempts = runs + bake
    problems = problems_a + problems_b
    if not attempts:
        reason = "; ".join(problems) or "no decode attempts recorded"
        return {"status": "nulled", "reason": reason, "rows": [], "as_of": iso(now)}
    models = {}
    for a in attempts:
        m = models.setdefault(a["model"], {"counts": dict.fromkeys(OUTCOMES, 0), "bodies": {}, "last": a["at"]})
        m["counts"][a["outcome"]] += 1
        if a["outcome"] == "verified":
            # The same body verified twice is one decode; keep its largest matched span.
            m["bodies"][a["body"]] = max(m["bodies"].get(a["body"], 0), a["bytes"])
        m["last"] = max(m["last"], a["at"])
    rows = []
    for model, m in models.items():
        runs_n = sum(m["counts"].values())
        rows.append({
            "model": model, "tier": tier(model),
            "runs": cell(runs_n, now), "empty": cell(m["counts"]["empty"], now),
            "no_dump": cell(m["counts"]["no_dump"], now), "wrong_bytes": cell(m["counts"]["wrong_bytes"], now),
            "unchecked": cell(m["counts"]["unchecked"], now),
            "verified_decodes": cell(len(m["bodies"]), now),
            "verified_bytes": cell(sum(m["bodies"].values()), now),
            "verified_share": cell(round(len(m["bodies"]) / runs_n, 2) if runs_n else None, now),
            "accepted_lines": cell(None, None, NO_ATTRIBUTION),
            "assisted_lines": cell(None, None, NO_ATTRIBUTION),
            "last_seen": iso(m["last"]),
        })
    rows.sort(key=lambda r: (-r["verified_bytes"]["value"], -r["verified_decodes"]["value"], r["runs"]["value"]))
    return {
        "status": "ok" if not problems else "partial", "as_of": iso(now),
        "source": f"{FREE_RUNS.name}/*/verdict.txt + run.log ({len(runs)} attempts); "
                  f"{BAKEOFF.name}/*/results.tsv ({len(bake)} attempts)",
        "problems": problems[:5],
        "rows": rows,
        "attempts": cell(len(attempts), now),
        "verified_decodes": cell(sum(r["verified_decodes"]["value"] for r in rows), now),
        "method": "ranked by verified bytes, then verified decodes, then fewest runs. A body verified more than "
                  "once by one model counts once. free_runs keeps one attempt per body (a re-run overwrites it).",
        "truth": "A verified decode means the bytes matched the game file. It does not mean the model's "
                 "interpretation was right, and it is not a credit.",
    }


if __name__ == "__main__":
    import json
    print(json.dumps(collect(), indent=2))
