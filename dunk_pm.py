"""Project PMs: lane->PM routing and the one-per-project digest.

A PM does not get a stream of reports. It publishes ONE digest per project per
window (30 minutes by convention), stored under a stable digest id
(`pm-digest:<project>`) so the VP reads a single updatable message instead of a
queue. The digest has four sections: what changed, what is blocked, what needs
an owner decision, and the current numbers with their evidence refs.

Routing is a registry, not a hard-coded rule: a project names its PM pane and
its member lanes. A lane that messages the VP out of habit is redirected to its
PM (or flagged, if the project asks for `flag`) so the VP's queue does not fill
with lane reports.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

DEFAULT_VP_PANE = os.environ.get("DUNKINGSHEEP_VP_PANE", "w9:pH")
ROUTING = Path(os.environ.get(
    "DUNKINGSHEEP_PM_ROUTING",
    Path.home() / ".config" / "dunkingsheep" / "pm-routing.json"))


class PmError(ValueError):
    """A named refusal for a bad routing file or digest payload."""


def load_routing(path=None, *, vp_pane=None):
    """Return {project: {pm, members, mode, vp_pane}}. Empty if no file."""
    path = Path(path or ROUTING).expanduser()
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise PmError(f"PM routing {path} is not readable JSON: {exc}")
    if not isinstance(data, dict):
        raise PmError(f"PM routing {path} must be an object of projects")
    routing = {}
    for project, entry in data.items():
        if not isinstance(entry, dict):
            raise PmError(f"PM routing {project} must be an object")
        pm = str(entry.get("pm") or "").strip()
        if not pm:
            raise PmError(f"PM routing {project} has no pm pane")
        members = [str(item).strip() for item in entry.get("members") or []]
        mode = str(entry.get("mode") or "redirect")
        if mode not in {"redirect", "flag"}:
            raise PmError(f"PM routing {project} mode must be redirect or flag")
        routing[project] = {"pm": pm, "members": members, "mode": mode,
                            "vp_pane": str(entry.get("vp_pane") or vp_pane or DEFAULT_VP_PANE)}
    return routing


def project_for(pane_id, routing):
    """The project whose member list contains this pane, if any."""
    for project, entry in routing.items():
        if pane_id and pane_id in entry["members"]:
            return project, entry
    return None, None


def redirect(pane_id, routing, *, vp_pane=None):
    """Decide where a lane's message to the VP should actually go.

    Returns (target_pane, project, mode) or (None, None, None). A project that
    is not a member of the target VP is left alone; the PM is never redirected
    to itself.
    """
    project, entry = project_for(pane_id, routing)
    if not project or not pane_id:
        return None, None, None
    if pane_id == entry["pm"]:
        return None, None, None
    return entry["pm"], project, entry["mode"]


POOLRAD_CHECKPOINT = Path(os.environ.get(
    "POOLRAD_CHECKPOINT",
    "~/workspace/poolrad-decomp/docs/re/function_accounting_checkpoint.json")).expanduser()


def checkpoint_numbers(project, path=None):
    """Read the project's numbers AT PUBLISH TIME. None = no source for this project.

    A digest is read instead of the reports, so a remembered number is a
    confident wrong answer with a source attached. On a failed read the number
    is 'unavailable', never yesterday's value.
    """
    if project != "poolrad":
        return None
    path = Path(path or POOLRAD_CHECKPOINT).expanduser()
    try:
        data = json.loads(path.read_text())
        bodies = int(data["strict_full_c_originals"])
        total = int(data["verified_game_identities"])
        commit = (data.get("reviewed_change") or {}).get("reviewed_commit") or "unknown"
        import datetime
        # Local wall-clock, readable in the digest: "as of 2026-10-21 19:40".
        as_of = datetime.datetime.fromtimestamp(
            path.stat().st_mtime).strftime("%Y-%m-%d %H:%M")
        ref = f"{path} (reviewed_commit {commit})"
        return [
            {"name": "strict C bodies", "value": bodies, "unit": f"/{total}",
             "evidence_ref": ref, "as_of": as_of},
            {"name": "verified game identities", "value": total, "unit": "",
             "evidence_ref": ref, "as_of": as_of},
        ]
    except (OSError, ValueError, KeyError, TypeError) as exc:
        return [{"name": "strict C bodies", "value": "unavailable", "unit": "",
                 "evidence_ref": f"{path}: {exc}", "as_of": None}]


def render_digest(project, *, changed=(), blocked=(), owner_decisions=(),
                  numbers=(), window="30 min", note=None):
    """Render the four-section digest. Empty sections say so, never vanish."""
    lines = [f"# PM digest: {project}", f"_window: {window}_", ""]
    if note:
        lines += [note, ""]
    lines.append("## Changed")
    lines += [f"- {item}" for item in changed] or ["- nothing"]
    lines += ["", "## Blocked"]
    lines += [f"- {item}" for item in blocked] or ["- nothing"]
    if len(blocked) > 6:
        lines.append(f"- PM note: {len(blocked)} blocked items; past six this is "
                     "no longer a digest - split the project or escalate one owner decision.")
    lines += ["", "## Owner decisions"]
    lines += [f"- {item}" for item in owner_decisions] or ["- nothing"]
    lines += ["", "## Numbers"]
    for item in numbers:
        if isinstance(item, dict):
            name = item.get("name", "?")
            value = item.get("value", "?")
            unit = item.get("unit", "")
            ref = item.get("evidence_ref") or "no evidence ref"
            as_of = item.get("as_of")
            suffix = f" (evidence: {ref}" + (f"; as of {as_of}" if as_of else "") + ")"
            lines.append(f"- {name}: {value} {unit}".rstrip() + suffix)
        else:
            lines.append(f"- {item}")
    if not numbers:
        lines.append("- nothing")
    return "\n".join(lines).strip() + "\n"


def digest_id(project):
    return f"pm-digest:{project}"


def parse_payload(raw):
    """Accept a JSON digest payload or a plain body string.

    JSON shape: {"changed": [...], "blocked": [...], "owner_decisions": [...],
    "numbers": [{"name","value","unit","evidence_ref"}], "note": "..."}.
    """
    text = (raw or "").strip()
    if not text:
        raise PmError("a digest needs a body on stdin")
    if text.startswith("{"):
        try:
            payload = json.loads(text)
        except ValueError as exc:
            raise PmError(f"digest payload is not valid JSON: {exc}")
        if not isinstance(payload, dict):
            raise PmError("digest payload must be a JSON object")
        return payload
    return {"note": text}
