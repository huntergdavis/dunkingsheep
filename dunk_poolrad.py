#!/usr/bin/env python3
"""Poolrad backlog-sync and progress (FLAT_FLEET.md 6.5): the queue fills itself.

The VP and CoS used to write every claimable Poolrad body by hand. This is the
deterministic replacement: run the repo's own strict-candidate exporter
(`tools/overlay/strict_candidate_queue.py`, read-only, in the shared checkout),
take the next N ranked candidates that are composable, not already credited and
not already filed, and file each as a `kind=work` task whose body carries the
base commit, the identity, the numbers the exporter computed, the method
pointer and the acceptance evidence. A planner lane files by hand only what
this cannot express.

`progress` reads the strict manifest and the reviewed identities, and says
CEILING where the denominator is an upper bound (ground truth `ceilings`).
"""

from __future__ import annotations

import csv
import json
import logging
import os
import re
import subprocess
import time
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("DUNKINGSHEEP_DIR",
                                 str(Path.home() / ".config" / "dunkingsheep")))
DEFAULT_REPO = os.environ.get("DUNKINGSHEEP_POOLRAD_REPO",
                              "/home/hunter/workspace/poolrad-decomp")
CEILING = 815          # an upper bound, not a verified denominator (ground truth `ceilings`)
QUEUE_JSON = CONFIG_DIR / "poolrad" / "strict-queue.json"
EXPORT_ARGS = [
    "--inventory", "resources/function-accounting-current/inventory.json",
    "--callgraph", "resources/function-accounting-current/callgraph.json",
    "--near-calls", "resources/function-accounting-current/near.json",
    "--ovr", "resources/DOS/POOLRAD/GAME.OVR", "--repo", ".", "--no-git",
]
STRICT_MANIFEST = "docs/re/strict_full_c_addresses.csv"
IDENTITIES = "docs/re/reviewed_function_identities.csv"
METHOD_DOC = "docs/re/STRICT_CANDIDATE_QUEUE.md"
TITLE_PREFIX = "Poolrad strict C:"

log = logging.getLogger("dunkingsheep.poolrad")


def _git(repo, *args):
    out = subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True,
                         timeout=30)
    return out.stdout.strip() if out.returncode == 0 else ""


def credited_ids(repo):
    try:
        with open(Path(repo) / STRICT_MANIFEST, newline="", encoding="utf-8") as handle:
            return {row["id"].strip() for row in csv.DictReader(handle) if row.get("id")}
    except OSError:
        return set()


def identity_count(repo):
    try:
        with open(Path(repo) / IDENTITIES, newline="", encoding="utf-8") as handle:
            return sum(1 for _ in csv.DictReader(handle))
    except OSError:
        return None


def export_queue(repo, out=QUEUE_JSON, timeout=300):
    """Run the repo's exporter read-only; the JSON lands in our state dir."""
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(["python3", "tools/overlay/strict_candidate_queue.py",
                           *EXPORT_ARGS, "--json", str(out)],
                          cwd=repo, capture_output=True, text=True, timeout=timeout)
    if proc.returncode != 0:
        raise RuntimeError(f"strict_candidate_queue.py failed ({proc.returncode}): "
                           f"{(proc.stderr or proc.stdout).strip()[-400:]}")
    with open(out, encoding="utf-8") as handle:
        return json.load(handle)


def candidates(queue, credited, filed_ids, limit, closed=None):
    """Ranked candidates worth filing: composable, not credited, not filed.

    `closed` maps a row id to the reason a reviewed disposition or a body already on
    main rules it out (t686). A closed row is never filed as "write the body"; the
    reason travels with the skip so the report can say which and why.
    """
    closed = closed or {}
    rows = sorted(queue.get("rows") or [], key=lambda r: (r.get("rank") or 10**9))
    picked = []
    for row in rows:
        rid = row.get("id")
        if not rid or rid in credited or rid in filed_ids or rid in closed:
            continue
        readiness = row.get("callee_readiness") or {}
        if not readiness.get("composable"):
            continue
        if row.get("classification") == "helper_candidate":
            continue
        picked.append(row)
        if len(picked) >= limit:
            break
    return picked


def task_title(row):
    return (f"{TITLE_PREFIX} {row['id']} ({row.get('bytes')} B, grade {row.get('grade')}, "
            f"rank {row.get('rank')})")


def task_body(row, base, repo):
    readiness = row.get("callee_readiness") or {}
    counts = readiness.get("counts") or {}
    lines = [
        f"POOLRAD STRICT FULL-C BODY {row['id']}",
        f"base: {base} (poolrad-decomp main at filing; rebase onto current main before the PR)",
        f"aliases: {', '.join(row.get('aliases') or [])}",
        f"unit {row.get('unit')} offset {row.get('offset')} ovr_offset {row.get('ovr_offset')}",
        f"bytes {row.get('bytes')}  instructions {row.get('instructions')}  grade {row.get('grade')}  "
        f"behaviour {row.get('behaviour')}  classification {row.get('classification')}",
        f"exporter rank {row.get('rank')} score {row.get('score')} confidence {row.get('confidence')}",
        f"callees: ready {readiness.get('ready')} of {sum(counts.values()) if counts else '?'} "
        f"({', '.join(f'{k}={v}' for k, v in sorted(counts.items()))})",
        f"prior translation: {row.get('prior_translation') or 'none'}",
        "",
        f"METHOD: {METHOD_DOC} (the strict full-C rules), docs/LANES.md (worker brief).",
        "Work in a private worktree of poolrad-decomp; never edit the shared checkout.",
        "ACCEPTANCE: a PR that adds the body's C and its evidence document, with the",
        "media/hashes a reviewer can reproduce; two reviewer-lane verdicts of `accept`;",
        "the accounting commit is written by the review timer from the verdict (the",
        "strict manifest row), never by the author. A claim one step wider than its",
        "evidence is the defect this fleet records most; say what was NOT verified.",
        f"filed by backlog-sync at {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}",
    ]
    return "\n".join(lines)


def filed_ids_from_tasks(tasks):
    out = set()
    for task in tasks:
        title = task.get("title") or ""
        if title.startswith(TITLE_PREFIX) and task.get("status") in ("open", "in_progress", "blocked"):
            parts = title[len(TITLE_PREFIX):].split()
            if parts:
                out.add(parts[0])
    return out


def disposition_closures(repo):
    """Row ids a reviewed DISPOSITION document on main has already closed (t686).

    `docs/re/OVL_0068_0294_DISPOSITION.md` says, in the repo's own words, that the
    body is a bounded model with no identity and no strict credit, and that the
    strict tables "must not" list it until its gaps close. backlog-sync filed it
    anyway (t575/t576), so a lane was asked to write exactly the claim the fleet
    records most: a credit one step wider than its evidence. The file name is the
    deterministic key - `<ID>_DISPOSITION.md` closes `<ID>` - so no prose parsing is
    involved and the answer cannot drift with a rewrite of the document.
    """
    out = {}
    root = Path(repo) / "docs" / "re"
    try:
        names = sorted(p.name for p in root.glob("*DISPOSITION*.md"))
    except OSError:
        return out
    for name in names:
        rid = name.split("DISPOSITION")[0].rstrip("_-")
        if rid:
            out[rid] = f"docs/re/{name}"
    return out


def registered_checker_ids(repo):
    """Row ids a registered checker in tools/overlay already covers (t686).

    A checker's CHECKER_ROWS is the repo's own statement that this body has a
    model to audit; when the model, its evidence doc and that checker are all on
    main the body is written and the only thing left is the REVIEW FLOW (two
    accepts, then the timer writes the credit row) - never an authoring task.
    """
    out = {}
    root = Path(repo) / "tools" / "overlay"
    try:
        paths = sorted(root.glob("*.py"))
    except OSError:
        return out
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if "CHECKER_ROWS" not in text:
            continue
        for rid in re.findall(r"(OVL_[0-9A-Fa-f_]+)", text.split("CHECKER_ROWS", 1)[1][:400]):
            out.setdefault(rid, f"tools/overlay/{path.name}")
    return out


def _unit_tokens(rid):
    """'OVL_006A_0824' -> {'006a_0824', '6a_0824'} - the shapes files use."""
    parts = str(rid or "").split("_")
    if len(parts) < 3:
        return set()
    unit, offset = parts[1].lower(), parts[2].lower()
    tokens = {f"{unit}_{offset}"}
    if unit.startswith("00"):
        tokens.add(f"{unit[2:]}_{offset}")
    elif unit.startswith("0"):
        tokens.add(f"{unit[1:]}_{offset}")
    return tokens


def already_on_main(repo, rid, checkers=None):
    """The model source + a registered checker for this row are both on main (t686).

    Returns the reason string, or None. The evidence document is not tested
    separately: the repo's pairing gate refuses a checker without one, so a
    registered CHECKER_ROWS entry already implies the pair.
    """
    checkers = registered_checker_ids(repo) if checkers is None else checkers
    checker = checkers.get(rid)
    if not checker:
        return None
    tokens = _unit_tokens(rid)
    src = Path(repo) / "src" / "recovered"
    try:
        names = [p.name.lower() for p in src.glob("*.c")]
    except OSError:
        names = []
    for name in names:
        if any(token in name for token in tokens):
            return f"already on main ({checker} audits {name})"
    return None


class Sync:
    def __init__(self, flock, repo=DEFAULT_REPO, exporter=export_queue):
        self.flock = flock
        self.repo = repo
        self.exporter = exporter

    def run(self, limit=6, dry_run=False, priority="medium"):
        report = {"repo": self.repo, "base": None, "filed": [], "skipped": None,
                  "errors": []}
        try:
            queue = self.exporter(self.repo)
        except Exception as error:  # noqa: BLE001 - say what failed, file nothing
            report["errors"].append(str(error))
            return report
        base = _git(self.repo, "rev-parse", "HEAD")
        report["base"] = base
        credited = credited_ids(self.repo)
        live = self.flock.tasks.list(scope=None, include_done=False, limit=500, kind="work")
        filed = filed_ids_from_tasks(live)
        # t686: a candidate a reviewed disposition has closed, or whose body and
        # checker are already on main, is not an authoring task. Both are decided
        # from the tree, not from a note, so the skip is reproducible.
        closed = {}
        for rid, where in disposition_closures(self.repo).items():
            closed[rid] = f"closed by {where}"
        checkers = registered_checker_ids(self.repo)
        for row in (queue.get("rows") or []):
            rid = row.get("id")
            if not rid or rid in closed:
                continue
            reason = already_on_main(self.repo, rid, checkers=checkers)
            if reason:
                closed[rid] = reason
        picked = candidates(queue, credited, filed, limit, closed=closed)
        report["skipped"] = {"credited": len(credited), "already_filed": len(filed),
                             "queue_rows": len(queue.get("rows") or []),
                             "closed": {rid: closed[rid] for rid in sorted(closed)}}
        for row in picked:
            if dry_run:
                report["filed"].append({"id": row["id"], "rank": row.get("rank"), "dry": True})
                continue
            try:
                task = self.flock.tasks.add("global", task_title(row),
                                            body=task_body(row, base, self.repo),
                                            priority=priority, kind="work",
                                            created_by="backlog-sync")
                report["filed"].append({"id": row["id"], "task": task["id"],
                                        "rank": row.get("rank")})
            except Exception as error:  # noqa: BLE001
                report["errors"].append(f"{row['id']}: {error}")
        return report


def progress(repo=DEFAULT_REPO, queue_path=QUEUE_JSON):
    credited = credited_ids(repo)
    summary = {}
    try:
        with open(queue_path, encoding="utf-8") as handle:
            summary = (json.load(handle).get("summary") or {})
    except (OSError, ValueError):
        summary = {}
    return {
        "credited": len(credited),
        "identities": identity_count(repo),
        "ceiling": CEILING,
        "ceiling_note": "CEILING: 815 is an upper bound, not a verified denominator",
        "composable_bodies": summary.get("composable_bodies"),
        "queue_candidates": summary.get("candidates"),
        "replaced": None,
        "replaced_note": "replaced (bodies the game actually runs) is computed by the "
                         "dashboard integration; not available here yet",
        "main": _git(repo, "rev-parse", "--short", "HEAD"),
        "queue_exported_at": (os.path.getmtime(queue_path) if os.path.exists(queue_path) else None),
    }
