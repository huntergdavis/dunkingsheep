"""Turn the live Dunking Sheep pane list into desk channels."""

from __future__ import annotations

import re


def discovered_id(pane_id: str) -> str:
    return "pane-" + re.sub(r"[^a-z0-9]+", "-", pane_id.casefold()).strip("-")


def match_pane(config: dict, panes: list[dict]) -> dict | None:
    # A declared handoff target must win over labels and shared working dirs.
    # If it is gone, stay offline rather than silently selecting another agent.
    pinned = config.get("pane")
    if pinned:
        return next((pane for pane in panes if pane.get("pane_id") == pinned), None)
    match = config["match"].casefold()
    for pane in panes:
        cwd = pane.get("cwd", "").casefold().rstrip("/")
        if pane.get("tab_label", "").casefold() == match or cwd == match or cwd.endswith("/" + match):
            return pane
    for pane in panes:
        if match in pane.get("tab_label", "").casefold() or match in pane.get("workspace_label", "").casefold():
            return pane
    return None


def is_lane(pane: dict | None) -> bool:
    """A pane the desk can message: it names an agent OR has declared a turn.

    t1147: the sn-desk lane is a real, typeable agent whose `agent` field is EMPTY (the
    same gap t996 measured for pi lanes), so `bool(pane["agent"])` left the Front Desk's
    composer on "Agent unavailable" while the daemon said alive/guarded/desk_can_type.
    A declared turn (`agent_status` working/idle/blocked) is evidence of a lane; a bare
    shell reports neither.
    """
    if not pane:
        return False
    return bool(pane.get("agent")) or pane.get("agent_status") in ("working", "idle", "blocked")


def presence(raw_status: str | None) -> str:
    return {"working": "working", "idle": "idle", "blocked": "waiting"}.get(raw_status, "unknown")


def build_roster(configured: list[dict], panes: list[dict], *, include_terminals: bool = False,
                 can_message=None) -> list[dict]:
    """`can_message(pane) -> bool` is the caller's authority for CONFIGURED channels (the web
    app asks the daemon for process evidence); without it the status heuristic is used."""
    can_message = can_message or is_lane
    rows = []
    used = set()
    for config in configured:
        pane = match_pane(config, panes)
        if pane:
            used.add(pane["pane_id"])
        rows.append((config, pane))
    for pane in panes:
        if (is_lane(pane) or include_terminals) and pane.get("pane_id") not in used:
            rows.append(({
                "id": discovered_id(pane["pane_id"]),
                "name": pane.get("tab_label") or pane.get("agent") or pane["pane_id"],
                "kind": "discovered", "dashboard": False,
            }, pane))
    output = []
    for config, pane in rows:
        status = presence(pane.get("agent_status")) if pane else "offline"
        output.append({
            "id": config["id"], "name": config["name"], "kind": config["kind"],
            "dashboard": config.get("dashboard", False),
            "online": pane is not None, "status": status,
            "activity_group": "active" if can_message(pane) and status in {"working", "waiting"} else "standby",
            "pane_id": pane.get("pane_id") if pane else None,
            "workspace": pane.get("workspace_label") if pane else None,
            "tab_label": pane.get("tab_label") if pane else None,
            "agent": pane.get("agent") if pane else None,
            "can_message": bool(pane) and bool(can_message(pane)),
        })
    return output
