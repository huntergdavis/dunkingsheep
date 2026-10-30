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


def _merge_pr(repo, commit):
    """The PR number that carried `commit`, from the merge commit's subject (t733).

    `git log --merges --ancestry-path` lists the merges above the commit newest
    first; the OLDEST of those is the one that brought it in, so the list is read
    backwards. A commit that reached main without a merge commit (a fast-forward
    or a direct push) has no PR here, and the answer is None - never a guess from
    a number that happens to appear in an unrelated subject.
    """
    log = _git(repo, "log", "--merges", "--ancestry-path", f"{commit}..HEAD",
               "--format=%s")
    for subject in reversed([line for line in log.splitlines() if line.strip()]):
        match = re.search(r"#(\d+)", subject)
        if match:
            return int(match.group(1))
    return None


def _model_name_matches(name, rid):
    """True when a recovered-model filename carries this row's unit AND offset (t733).

    The files insert a human label between the two, so the tokens are not adjacent:
    OVL_0020_11AC is `overlay_u20_list_walk_11ac_cpu.c` and OVL_03B0_06FB is
    `overlay_u3b0_row_clear_06fb_cpu.c`. Matching on adjacency alone missed two of
    the three rows this task was opened for. Leading zeros are optional on both
    halves (`006a` -> `u6a`), and each half must land on a token boundary so
    `u20` does not match `u200` and `11ac` does not match `11ac0`.
    """
    parts = str(rid or "").split("_")
    if len(parts) < 3:
        return False
    unit = (parts[1].lower().lstrip("0") or "0")
    offset = (parts[2].lower().lstrip("0") or "0")
    low = str(name or "").lower()
    unit_hit = re.search(rf"(^|[_/])u{re.escape(unit)}([_/.]|$)", low)
    offset_hit = re.search(rf"(^|_)0*{re.escape(offset)}([_/.]|$)", low)
    return bool(unit_hit and offset_hit)


def model_evidence(repo, rid):
    """Where this row's model ALREADY lives on main: (model, commit, pr) or None (t733).

    Measured 2026-10-29 (ds-worker-4): OVL_0038_0CC3, OVL_0020_11AC and
    OVL_03B0_06FB were filed as 'write the strict full-C body' when the model,
    header, checker and doc were already on main and only the LEDGER CREDIT was
    missing. Filing that task hands a lane work that cannot be done. This looks at
    the tree, not at a note: a file under src/recovered whose name carries the
    row's unit+offset tokens, the commit that added it, and the PR that carried it.
    """
    src = Path(repo) / "src" / "recovered"
    try:
        names = sorted(p.name for p in src.glob("*.c"))
    except OSError:
        return None
    for name in names:
        if not _model_name_matches(name, rid):
            continue
        relative = f"src/recovered/{name}"
        commit = _git(repo, "log", "-1", "--format=%H", "--", relative)
        if not commit:
            continue
        return {"model": relative, "commit": commit, "pr": _merge_pr(repo, commit)}
    return None


def credit_pending_path(path=None):
    return Path(path) if path else CONFIG_DIR / "poolrad" / "credit-pending.json"


def record_credit_pending(entries, path=None):
    """Merge `entries` ({id: evidence}) into the credit-pending list (t733).

    This file is the OWNER's queue: each body whose model is already on main but
    whose ledger row is missing, with the commit and PR that added the model, so a
    credit review can be filed against that head instead of an authoring task
    against nothing. Merged by id and rewritten atomically; an entry already there
    keeps its first-seen evidence.
    """
    path = credit_pending_path(path)
    data = {}
    try:
        with open(path, encoding="utf-8") as handle:
            loaded = json.load(handle)
        if isinstance(loaded, dict):
            data = loaded
    except (OSError, ValueError):
        data = {}
    bodies = data.get("bodies") if isinstance(data.get("bodies"), dict) else {}
    for rid, evidence in (entries or {}).items():
        if rid in bodies:
            continue
        bodies[rid] = dict(evidence, first_seen=round(time.time(), 3))
    payload = {"bodies": bodies, "count": len(bodies),
               "updated_at": round(time.time(), 3)}
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = str(path) + ".tmp"
        handle = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(handle, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, indent=1, sort_keys=True)
        os.replace(temporary, path)
    except OSError as error:                       # never lose a sync to a write
        log.warning("credit-pending record not written (%s): %s", path, error)
        return None
    return payload


def credit_pending(path=None):
    """The recorded credit-pending bodies, {id: evidence} (t733)."""
    try:
        with open(credit_pending_path(path), encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return {}
    bodies = data.get("bodies") if isinstance(data, dict) else None
    return dict(bodies) if isinstance(bodies, dict) else {}


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
    def __init__(self, flock, repo=DEFAULT_REPO, exporter=export_queue, credit_path=None):
        self.flock = flock
        self.repo = repo
        self.exporter = exporter
        # Injectable so a test (and a scratch run) never writes the owner's real
        # credit-pending list (t733).
        self.credit_path = credit_path

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
        # t733: a row whose MODEL is already on main is not an authoring task - it is
        # a missing LEDGER ROW. Filing it as "write the strict full-C body" hands a
        # lane work that cannot be done (OVL_0038_0CC3/OVL_0020_11AC/OVL_03B0_06FB,
        # 2026-10-29); the body goes on the OWNER's credit-pending list with the
        # commit and the PR that added the model, so credit can be reviewed against
        # that head. Decided from the tree, so the skip is reproducible.
        credit = {}
        for row in (queue.get("rows") or []):
            rid = row.get("id")
            if not rid:
                continue
            if rid not in credited:
                evidence = model_evidence(self.repo, rid)
                if evidence:
                    credit[rid] = evidence
                    closed[rid] = (f"model already on main ({evidence['model']}); "
                                   "ledger credit pending, nothing to author")
            if rid in closed:
                continue
            reason = already_on_main(self.repo, rid, checkers=checkers)
            if reason:
                closed[rid] = reason
        report["credit_pending"] = credit
        if credit and not dry_run:
            written = record_credit_pending(credit, path=self.credit_path)
            report["credit_pending_file"] = (str(credit_pending_path(self.credit_path))
                                             if written else None)
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


def progress(repo=DEFAULT_REPO, queue_path=QUEUE_JSON, credit_path=None):
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
        # t733: bodies whose model is already on main and whose ledger row is not.
        # The OWNER files one review per entry against the PR head that added it.
        "credit_pending": len(credit_pending(credit_path)),
    }
