"""Continuous agent-state capture: what it would take to put a lane back.

The owner asked for every agent to be restartable by the VP with its previous state,
recorded continuously by this daemon, deterministically, no LLM. This module is the
CAPTURE half only (the restore is gated on the VP's scope): a row per lane per
capture, with a null and a reason wherever a field is not readable.

Nothing here guesses. `model` and `thinking` come from the launching argv when it
carries them, and are null with a reason otherwise - a wrong model in a restore is
worse than no model, because it looks like a decision.
"""
import json
import os
import re
import sqlite3
import threading

import dunk_activity

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))
DB_PATH = os.path.join(CONFIG_DIR, "agent-state.db")

SCHEMA = """
CREATE TABLE IF NOT EXISTS agent_state (
    at REAL NOT NULL,
    pane_id TEXT NOT NULL,
    project TEXT,
    harness TEXT,
    cwd TEXT,
    argv TEXT,
    session_id TEXT,
    model TEXT,
    model_reason TEXT,
    thinking TEXT,
    thinking_reason TEXT,
    state TEXT,
    state_source TEXT,
    state_reason TEXT,
    task_id TEXT,
    source TEXT
);
CREATE INDEX IF NOT EXISTS agent_state_pane ON agent_state(pane_id, at DESC);
CREATE INDEX IF NOT EXISTS agent_state_time ON agent_state(at DESC);
"""

_MODEL_FLAGS = ("--model", "-m")
_THINKING_FLAGS = ("--thinking", "--reasoning", "--effort")


class StateStore:
    def __init__(self, path=DB_PATH):
        self.path = path
        self._lock = threading.RLock()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        self.db = sqlite3.connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            self.db.commit()

    def add(self, rows, at):
        with self._lock:
            self.db.executemany(
                "INSERT INTO agent_state (at, pane_id, project, harness, cwd, argv, session_id, "
                "model, model_reason, thinking, thinking_reason, state, state_source, state_reason, "
                "task_id, source) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                [(at, row["pane_id"], row.get("project"), row.get("harness"), row.get("cwd"),
                  json.dumps(row.get("argv")) if row.get("argv") else None,
                  row.get("session_id"), row.get("model"), row.get("model_reason"),
                  row.get("thinking"), row.get("thinking_reason"), row.get("state"),
                  row.get("state_source"), row.get("state_reason"), row.get("task_id"),
                  row.get("source")) for row in rows])
            self.db.commit()
        return len(rows)

    def latest(self, pane_id=None, project=None, limit=200):
        where, params = [], []
        if pane_id:
            where.append("pane_id = ?"); params.append(pane_id)
        if project:
            where.append("project = ?"); params.append(project)
        clause = (" WHERE " + " AND ".join(where)) if where else ""
        with self._lock:
            rows = [dict(row) for row in self.db.execute(
                f"SELECT * FROM agent_state{clause} ORDER BY at DESC LIMIT ?", (*params, int(limit)))]
        for row in rows:
            if row.get("argv"):
                try:
                    row["argv"] = json.loads(row["argv"])
                except ValueError:
                    row["argv"] = None
        return rows


def project_of(cwd):
    """The project label: the workspace directory name, or null."""
    if not cwd:
        return None
    parts = [part for part in str(cwd).strip("/").split("/") if part]
    if len(parts) >= 2 and parts[0] == "home":
        return parts[-1] if len(parts) <= 3 else parts[-1]
    return parts[-1] if parts else None


def _flag(argv, names):
    for index, token in enumerate(argv or ()):
        if token in names and index + 1 < len(argv):
            return argv[index + 1]
        for name in names:
            if str(token).startswith(name + "="):
                return str(token).split("=", 1)[1]
    return None


def capture(flock, *, source="periodic", now=None, store=None):
    """One capture: a row per agent lane, fields from argv/status/activity."""
    import time
    now = now or time.time()
    store = store or StateStore()
    launches = {}
    try:
        with open(os.path.join(CONFIG_DIR, "launches.json"), encoding="utf-8") as handle:
            panes = (json.load(handle) or {}).get("panes") or {}
        for pane_id, entry in panes.items():
            launches[pane_id] = dict(entry, pane_id=pane_id)
    except (OSError, ValueError):
        launches = {}
    rows = []
    for lane in flock.resolve_lanes():
        if not lane.get("agent"):
            continue
        launch = launches.get(lane["pane_id"]) or {}
        argv = launch.get("argv")
        model = _flag(argv, _MODEL_FLAGS)
        thinking = _flag(argv, _THINKING_FLAGS)
        record = dunk_activity.activity(lane, claude_status=getattr(flock, "claude_status", None),
                                       turns=getattr(flock, "turns", None), now=now)
        tasks = []
        try:
            tasks = flock.tasks.leases_for_pane(lane["pane_id"], now=now)
        except Exception:                       # noqa: BLE001
            tasks = []
        rows.append({
            "pane_id": lane["pane_id"],
            "project": project_of(lane.get("cwd")),
            "harness": lane.get("agent"),
            "cwd": lane.get("cwd"),
            "argv": argv,
            "session_id": launch.get("session") or launch.get("session_id"),
            "model": model,
            "model_reason": None if model else "the launching argv carries no model flag",
            "thinking": thinking,
            "thinking_reason": None if thinking else "the launching argv carries no thinking flag",
            "state": record["state"],
            "state_source": record["source"],
            "state_reason": record["reason"],
            "task_id": tasks[0]["task_id"] if tasks else None,
            "source": source,
        })
    store.add(rows, now)
    return {"at": now, "rows": len(rows), "source": source}
