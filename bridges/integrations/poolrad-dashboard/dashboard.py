#!/usr/bin/env python3
"""Local, dependency-free Poolrad evidence queue and verification dashboard.

This reads the source repository but writes only inside this directory (and an
isolated /tmp build directory). It never reads original game media by default.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import re
import subprocess
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo

HERE = Path(__file__).resolve().parent
DEFAULT_REPO = Path(os.getenv("POOLRAD_DASHBOARD_REPO", HERE.parents[2] / "poolrad-decomp-opencode-ds41flash"))
STATE = HERE / "state"


def timestamp() -> str:
    return datetime.now(ZoneInfo("America/Los_Angeles")).isoformat(timespec="seconds")


def head(repo: Path) -> str:
    return subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()


def rows(path: Path) -> list[dict[str, str]]:
    with path.open(newline="", encoding="utf-8") as stream:
        return list(csv.DictReader(stream))


def queue(repo: Path) -> tuple[list[dict], dict]:
    ledger = {r["id"]: r for r in rows(repo / "docs/re/overlay_function_ledger.csv")}
    classes = rows(repo / "docs/re/overlay_classification.csv")
    result = []
    for item in classes:
        detail = ledger.get(item["id"], {})
        identity = item["classification"] == "game_verified"
        # A ranking hint, not an owner assignment or a claim of full C status.
        score = (10 if item["grade"] == "A" else 0) + min(int(item["ver_calls"]), 10)
        score += min(int(item["strings"]), 5) + (3 if item["doc_globals"] else 0)
        result.append({
            "id": item["id"], "domain": "overlay", "classification": item["classification"],
            "confidence": item["confidence"], "grade": item["grade"],
            "identity_verified": identity, "full_c": "unlinked",
            "queue_state": "identity verified; C status unlinked" if identity else "triage candidate",
            "priority_hint": score, "strings": int(item["strings"]),
            "verified_calls": int(item["ver_calls"]),
            "decoded": int(item["decoded"]), "bound": int(item["bound"]),
            "ledger_class": detail.get("cls", ""),
        })
    resident = rows(repo / "docs/re/resident_callable_ledger.csv")
    for item in resident:
        identity = item["classification"] == "game_verified"
        score = min(int(item["overlay_far_sites"]), 20) + min(int(item["decoded_near_sites"]), 10)
        result.append({
            "id": item["id"], "domain": "resident", "classification": item["classification"],
            "confidence": item["confidence"], "grade": item["boundary"],
            "identity_verified": identity, "full_c": "unlinked",
            "queue_state": "identity verified; C status unlinked" if identity else "triage candidate",
            "priority_hint": score, "strings": 0, "verified_calls": 0,
            "decoded": 0, "bound": 0, "ledger_class": item["kind"],
        })
    result.sort(key=lambda x: (x["identity_verified"], -x["priority_hint"], x["id"]))
    summary = {
        "overlay_candidates": len(classes),
        "overlay_verified_identities": sum(r["identity_verified"] for r in result if r["domain"] == "overlay"),
        "overlay_unverified_candidates": sum(not r["identity_verified"] for r in result if r["domain"] == "overlay"),
        "resident_ledger_rows": len(resident),
        "resident_verified_identities": sum(r["identity_verified"] for r in result if r["domain"] == "resident"),
    }
    return result, summary


def mark(repo: Path, function_id: str, status: str, note: str) -> dict:
    available = {item["id"] for item in queue(repo)[0]}
    if function_id not in available:
        raise ValueError(f"unknown function id: {function_id}")
    path = STATE / "queue_marks.json"
    marks = read_json(path) or {}
    marks[function_id] = {"status": status, "note": note, "time": timestamp(),
                          "source_commit": head(repo)}
    write_json(path, marks)
    return marks[function_id]


def cited_evidence(repo: Path) -> list[dict]:
    # Only claim results actually present in published reports. Never combine
    # different runs or treat a bounded comparison as whole-frame parity.
    cases = [
        ("EGA first-view row 64", "docs/re/SLOT_A_FIRST_EGA_ROW64.md",
         r"\*\*320 indexed pixels match all 320\*\*", 320, 320,
         "one 320-pixel row at the saved first-view return"),
        ("Printer page, row 6 column 38", "docs/re/SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
         r"\*\*28,160/28,160\*\* pixel indices equal", 28160, 28160,
         "one reached printer call; 320×88 diagnostic crop"),
    ]
    output = []
    for label, source, proof, matched, compared, scope in cases:
        path = repo / source
        body = path.read_text(encoding="utf-8")
        output.append({
            "label": label, "source": source, "scope": scope,
            "matched": matched if re.search(proof, body) else None,
            "compared": compared if re.search(proof, body) else None,
            "status": "reported in source" if re.search(proof, body) else "source claim changed; re-review",
            "method": "published scoped report; not rerun by dashboard",
        })
    return output


def read_json(path: Path) -> dict | None:
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else None


def percent(count: int, ceiling: int, bound_kind: str) -> float:
    """Share of the reviewed bound. An upper bound is reported at one decimal."""
    if ceiling <= 0:
        return 0.0
    if bound_kind == "upper_bound":
        return (count * 1000 // ceiling) / 10
    return round(100 * count / ceiling, 1)


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def function_progress(repo: Path) -> dict:
    """Use reviewed accounting and a separately reviewed, same-binary census."""
    checkpoint = read_json(repo / "docs/re/function_accounting_checkpoint.json")
    if not checkpoint or checkpoint.get("schema_version") != 1:
        raise ValueError("reviewed function accounting checkpoint is missing or unsupported")
    strict = checkpoint["strict_full_c_originals"]
    identities = checkpoint["verified_game_identities"]
    if not all(type(value) is int and value >= 0 for value in (strict, identities)):
        raise ValueError("reviewed function counts must be nonnegative integers")
    progress = {
        "status": "pending", "strict_full_c": strict,
        "verified_identities": identities, "ceiling": None,
        "ceiling_gap": None,
        "bound_kind": None, "strict_percent": None,
        "identity_percent": None, "evidence_doc": None,
        "accounting_source": "docs/re/function_accounting_checkpoint.json",
        "scope": "original game functions in the overlay and resident program",
    }
    census = read_json(repo / "docs/re/function_ceiling_census.json")
    if not census or census.get("status") == "pending":
        return progress
    if census.get("status") != "reviewed":
        raise ValueError("function ceiling census has unsupported status")
    ceiling = census.get("count")
    if (type(ceiling) is not int or ceiling <= 0 or
            ceiling < max(strict, identities)):
        raise ValueError("reviewed function ceiling must cover the accounting counts")
    if census.get("bound_kind") not in ("exact", "upper_bound"):
        raise ValueError("reviewed census must say whether the count is exact or an upper bound")
    if census.get("scope") != "original-game-functions-overlay-and-resident":
        raise ValueError("census scope does not match the accounting checkpoint")
    if census.get("source_digests") != checkpoint.get("inputs"):
        raise ValueError("census and accounting checkpoint use different original binaries")
    evidence_doc = census.get("evidence_doc")
    if (not isinstance(evidence_doc, str) or
            not evidence_doc.startswith("docs/re/") or
            not (repo / evidence_doc).is_file() or
            census.get("reviewed_by") != "PM"):
        raise ValueError("reviewed census needs a PM-reviewed repository evidence document")
    progress.update({
        "status": "reviewed", "ceiling": ceiling,
        "ceiling_gap": ceiling - strict,
        "bound_kind": census["bound_kind"],
        "strict_percent": (
            (strict * 1000 // ceiling) / 10 if census["bound_kind"] == "upper_bound"
            else round(100 * strict / ceiling, 1)
        ),
        "identity_percent": (
            (identities * 1000 // ceiling) / 10 if census["bound_kind"] == "upper_bound"
            else round(100 * identities / ceiling, 1)
        ),
        "evidence_doc": evidence_doc,
    })
    return progress


def progress(repo: Path) -> dict:
    """The compact completion view: two percentages and one evidence line.

    The numbers come from the PM-reviewed accounting checkpoint and the
    reviewed ceiling census in the same checkout. Nothing is inferred from
    candidate rows, PR counts, or local drafts.
    """
    return compact_view(repo, function_progress(repo), commit=head(repo),
                        source="dashboard checkout")


def compact_view(repo: Path, accounting: dict, *, commit: str, source: str) -> dict:
    """Build the compact view from an already reviewed accounting record.

    ``accounting`` is the reviewed pair (checkpoint plus census) in the shape
    ``function_progress`` returns, so the published main commit and the local
    checkout produce the same compact view.
    """
    strict = accounting["strict_full_c"]
    identities = accounting["verified_identities"]
    ceiling = accounting["ceiling"]
    bound_kind = accounting["bound_kind"]
    reviewed = accounting["status"] == "reviewed" and ceiling is not None
    comparisons = cited_evidence(repo)
    evidence = [f"{item['matched']}/{item['compared']} px" for item in comparisons
                if item.get("matched") is not None]
    if reviewed:
        line = (f"PM-reviewed checkpoint {commit[:8]} · {strict} strict / {identities} identities"
                f" · ceiling {ceiling} ({bound_kind.replace('_', ' ')})"
                f" · {len(comparisons)} published scoped comparison(s)"
                + (f" ({', '.join(evidence)})" if evidence else "")
                + " · not whole-game completion")
    else:
        line = ("PM-reviewed accounting checkpoint unavailable in this checkout;"
                " completion percentages are unknown")
    return {
        "generated": timestamp(),
        "commit": commit,
        "source": source,
        "status": accounting["status"],
        "strict_full_c": strict,
        "identities": identities,
        "ceiling": ceiling,
        "bound_kind": bound_kind,
        "strict_percent": percent(strict, ceiling, bound_kind) if reviewed else None,
        "identity_percent": percent(identities, ceiling, bound_kind) if reviewed else None,
        "remaining": (ceiling - strict) if reviewed else None,
        "accounting_source": accounting["accounting_source"],
        "evidence_doc": accounting["evidence_doc"],
        "evidence_line": line,
        "comparisons": comparisons,
        "stale": False,
    }


def verify(repo: Path) -> dict:
    STATE.mkdir(exist_ok=True)
    build = Path("/tmp/poolrad-dashboard-build")
    steps = [
        ("configure", ["cmake", "-S", str(repo), "-B", str(build), "-DCMAKE_BUILD_TYPE=Debug",
                       "-DCMAKE_C_FLAGS=-Wall -Wextra -Wpedantic -Werror"]),
        ("build", ["cmake", "--build", str(build), "-j", "4"]),
        ("ctest", ["ctest", "--test-dir", str(build), "--output-on-failure", "-j", "4"]),
    ]
    report = {"commit": head(repo), "time": timestamp(), "steps": [], "status": "failed"}
    for label, command in steps:
        run = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, check=False)
        log_path = STATE / f"{label}.log"
        log_path.write_text(run.stdout, encoding="utf-8")
        last = re.search(r"(\d+)% tests passed, (\d+) tests failed out of (\d+)", run.stdout)
        report["steps"].append({"name": label, "exit": run.returncode,
                                "log": str(log_path),
                                "tests_total": int(last.group(3)) if last else None,
                                "tests_failed": int(last.group(2)) if last else None})
        if run.returncode:
            break
    else:
        report["status"] = "passed"
    report["head_unchanged"] = report["commit"] == head(repo)
    if not report["head_unchanged"]:
        report["status"] = "stale: repo head changed during verification"
    write_json(STATE / "verification.json", report)
    return report


def compare(dos: Path, portable_c: Path, width: int, height: int, label: str) -> dict:
    if width <= 0 or height <= 0:
        raise ValueError("width and height must be positive")
    native = dos.read_bytes()
    candidate = portable_c.read_bytes()
    expected = width * height
    if len(native) != expected or len(candidate) != expected:
        raise ValueError(f"both files must contain exactly {expected} indexed pixel bytes")
    matched = sum(a == b for a, b in zip(native, candidate))
    report = {"label": label, "time": timestamp(), "scope": f"{width}×{height} indexed pixels",
              "matched": matched, "compared": expected, "mismatched": expected - matched,
              "dos_sha256": hashlib.sha256(native).hexdigest(),
              "c_sha256": hashlib.sha256(candidate).hexdigest(),
              "method": "direct local byte comparison; input files retained outside dashboard",
              "qualification": "Caller must establish same state, palette, and capture provenance separately"}
    write_json(STATE / "local_diff.json", report)
    return report


def render(repo: Path) -> Path:
    """Write the compact owner view: two percentages and one evidence line.

    The research snapshot keeps the full candidate list; the page the owner
    sees does not. Long prose was removed on purpose: the numbers and one
    cited evidence line carry the truth, and the detail stays in the JSON
    payload and the cited source reports.
    """
    q, counts = queue(repo)
    rev = head(repo)
    marks = read_json(STATE / "queue_marks.json") or {}
    for item in q:
        annotation = marks.get(item["id"])
        if annotation:
            item["local_review"] = annotation
    verification = read_json(STATE / "verification.json")
    if verification and verification["commit"] != rev:
        verification = {**verification, "status": "stale: commit differs from current repo"}
    evidence = cited_evidence(repo)
    reports_dir = STATE / "reports"
    reports_dir.mkdir(parents=True, exist_ok=True)
    for item in evidence:
        source = repo / item["source"]
        (reports_dir / source.name).write_bytes(source.read_bytes())
    local_diff = read_json(STATE / "local_diff.json")
    compact = progress(repo)
    data = {"generated": timestamp(), "commit": rev, "summary": counts, "queue": q,
            "function_progress": function_progress(repo),
            "verification": verification, "published_comparisons": evidence, "local_diff": local_diff}
    write_json(STATE / "snapshot.json", data)
    owner_data = {key: data[key] for key in (
        "generated", "commit", "function_progress", "verification",
        "published_comparisons", "local_diff")}
    owner_data["progress"] = compact
    payload = json.dumps(owner_data).replace("<", "\\u003c").replace(">", "\\u003e")
    page = """<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Poolrad original C bodies</title>
<style>body{font:17px/1.45 system-ui;margin:0 auto;padding:1.6rem;max-width:900px;background:#101924;color:#f0f5f8}
h1{color:#faf2bb;font-size:1.5rem;margin:0 0 1.1rem}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:1rem}
.card{background:#203141;border:1px solid #48617a;border-radius:10px;padding:1.1rem 1.2rem}.num{font-size:3rem;font-weight:750;line-height:1;color:#faf2bb}
.label{font-weight:700;letter-spacing:.06em;margin-top:.5rem}.sub{color:#b9cbd8;font-size:.85rem;margin-top:.25rem}
.track{height:10px;background:#0d1721;border-radius:6px;overflow:hidden;margin-top:.8rem}.fill{height:100%;background:#a9db7d}
.evidence{color:#b9cbd8;font-size:.9rem;border-top:1px solid #35505f;margin-top:1.2rem;padding-top:.9rem}
.evidence a{color:#92d5ff}.meta{color:#7f97a4;font-size:.75rem;margin-top:.7rem}</style>
<h1>POOLRAD &middot; ORIGINAL C BODIES</h1>
<div class="cards">__CARDS__</div>
<p class="evidence">__EVIDENCE__</p>
<p class="meta">__META__</p>
<script id="data" type="application/json">""" + payload + """</script></html>"""
    cards = []
    for label, share, count in (("STRICT C", compact["strict_percent"], compact["strict_full_c"]),
                                ("IDENTITIES", compact["identity_percent"], compact["identities"])):
        if share is None:
            cards.append(f"<div class=\"card\"><div class=\"num\">—</div><div class=\"label\">{label}</div>"
                         "<div class=\"sub\">reviewed checkpoint unavailable</div></div>")
            continue
        width = max(0.0, min(100.0, float(share)))
        cards.append(f"<div class=\"card\"><div class=\"num\">{share}%</div><div class=\"label\">{label}</div>"
                     f"<div class=\"sub\">{count} / {compact['ceiling']} {compact['bound_kind'].replace('_', ' ')}</div>"
                     f"<div class=\"track\"><div class=\"fill\" style=\"width:{width}%\"></div></div></div>")
    links = " &middot; ".join(
        f"<a href=\"state/reports/{item['source'].split('/')[-1]}\">{item['label']}</a>"
        for item in compact["comparisons"])
    evidence_html = compact["evidence_line"] + (f" &middot; reports: {links}" if links else "")
    page = (page.replace("__CARDS__", "".join(cards))
                .replace("__EVIDENCE__", evidence_html)
                .replace("__META__", f"generated {compact['generated']} &middot; checkout {compact['commit'][:12]}"))
    output = HERE / "dashboard.html"
    output.write_text(page, encoding="utf-8")
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    sub = parser.add_subparsers(dest="action", required=True)
    sub.add_parser("render")
    sub.add_parser("verify")
    sub.add_parser("progress", help="print the compact completion view as JSON")
    diff = sub.add_parser("compare")
    diff.add_argument("--dos", type=Path, required=True)
    diff.add_argument("--c", type=Path, required=True)
    diff.add_argument("--width", type=int, required=True)
    diff.add_argument("--height", type=int, required=True)
    diff.add_argument("--label", required=True)
    queue_mark = sub.add_parser("mark")
    queue_mark.add_argument("--id", required=True)
    queue_mark.add_argument("--status", required=True,
                            choices=["watch", "needs-evidence", "ready-for-review", "blocked", "archived"])
    queue_mark.add_argument("--note", default="")
    args = parser.parse_args()
    repo = args.repo.resolve()
    if args.action == "verify":
        print(json.dumps(verify(repo), indent=2))
    elif args.action == "progress":
        print(json.dumps(progress(repo), indent=2))
    elif args.action == "compare":
        print(json.dumps(compare(args.dos, args.c, args.width, args.height, args.label), indent=2))
    elif args.action == "mark":
        print(json.dumps(mark(repo, args.id, args.status, args.note), indent=2))
    print(render(repo))


if __name__ == "__main__":
    main()
