"""Process-bound Codex evidence for owned-prompt recovery (t508).

No newest-file/session inference: the running Codex process must name an exact
resume session, or its hook record must name that same PID/start and pane.
The authenticated native rollout proves both quiescence and prompt absence.
"""
from __future__ import annotations

import json
import os
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
import threading

from dunk_callers import process_alive
from dunk_native_session import (NativeSessionRefusal, NativeSessionSnapshot,
                                 _read_rollout, _signature, _state_row,
                                 find_resume_request)

_CACHE = OrderedDict()
_CACHE_LOCK = threading.Lock()


@dataclass(frozen=True)
class PromptSession:
    snapshot: NativeSessionSnapshot
    text: str
    pid: int
    start: str

    def assert_unchanged(self):
        if not process_alive(self.pid, self.start):
            raise NativeSessionRefusal("the bound Codex process changed")
        self.snapshot.assert_unchanged()


def _process_environment(pid):
    # Read only the fields needed for binding; never retain or log other env.
    with open(f"/proc/{int(pid)}/environ", "rb") as handle:
        entries = handle.read().split(b"\0")
    names = {b"CODEX_HOME", b"DUNKINGSHEEP_CODEX_STATE_DIR"}
    return {k.decode(): v.decode() for entry in entries
            for k, sep, v in [entry.partition(b"=")] if sep and k in names}


def _registered_binding(pane, pid, start):
    """Opt-in operator binding for legacy bare Codex launches.

    No binding file is created by recovery. Operators independently verify an
    exact native session and register it for one process lifetime, then review
    and roll out configuration separately from this source change.
    """
    path = os.environ.get("DUNKINGSHEEP_RECOVERY_BINDINGS")
    if not path:
        return None
    record = json.loads(Path(path).read_text()).get(pane.get("pane_id"))
    if not isinstance(record, dict):
        return None
    if (record.get("pid") != pid or str(record.get("start")) != start
            or os.path.realpath(record.get("cwd") or "")
            != os.path.realpath(pane.get("cwd") or "")):
        raise NativeSessionRefusal("registered native binding has the wrong process generation/cwd")
    if not record.get("codex_home") or not record.get("session_id"):
        raise NativeSessionRefusal("registered native binding is incomplete")
    return record


def resolve_prompt_session(backend, pane):
    """Resolve identity only: safe to call before Enter without reading history."""
    entries = backend.agent_child_pids(pane.get("pane_pid") or pane.get("pid"))
    roots = [p for p in (entries or []) if p.get("comm") == "codex"
             and p.get("state") not in ("Z", "X", "x")]
    if len(roots) != 1:
        raise NativeSessionRefusal("one live Codex process is required")
    root = roots[0]
    pid, start = int(root["pid"]), str(root["start"])
    if not process_alive(pid, start):
        raise NativeSessionRefusal("the Codex process identity is stale")
    env = _process_environment(pid)
    registered = _registered_binding(pane, pid, start)
    request = find_resume_request(root.get("argv"))
    session_id = request.session_id if request else None
    if registered:
        if session_id and session_id != registered["session_id"]:
            raise NativeSessionRefusal("registered session disagrees with the running resume")
        if env.get("CODEX_HOME") and os.path.realpath(env["CODEX_HOME"]) != os.path.realpath(registered["codex_home"]):
            raise NativeSessionRefusal("registered home disagrees with the running Codex home")
        session_id = registered["session_id"]
    if not session_id:
        directory = env.get("DUNKINGSHEEP_CODEX_STATE_DIR")
        if not directory:
            raise NativeSessionRefusal("no process-bound native session id")
        matches = []
        for path in Path(directory).glob("*.json"):
            state = json.loads(path.read_text())
            if (state.get("root_pid") == pid and str(state.get("root_start")) == start
                    and state.get("pane_id") == pane.get("pane_id")
                    and os.path.realpath(state.get("cwd") or "")
                    == os.path.realpath(pane.get("cwd") or "")):
                matches.append(state.get("session_id"))
        if len(matches) != 1 or not matches[0]:
            raise NativeSessionRefusal("no unique process-bound native session id")
        session_id = matches[0]
    home = env.get("CODEX_HOME") or (registered or {}).get("codex_home")
    if not home:
        raise NativeSessionRefusal("Codex process has no explicit CODEX_HOME")
    _row, path = _state_row(home, session_id)
    cwd = pane.get("cwd") or pane.get("foreground_cwd")
    if not cwd or os.path.realpath(_row["cwd"]) != os.path.realpath(cwd):
        raise NativeSessionRefusal("native session cwd mismatch")
    return pid, start, session_id, path, cwd


def read_prompt_session(backend, pane):
    """Read one kernel-bound Codex session or raise; never guess a session."""
    pid, start, session_id, path, cwd = resolve_prompt_session(backend, pane)
    key = (pid, start, session_id, path, cwd, _signature(os.stat(path)))
    with _CACHE_LOCK:
        cached = _CACHE.get(key)
    if cached is not None:
        cached.assert_unchanged()
        return cached
    signature, started, completed, active = _read_rollout(path, session_id, cwd)
    if active:
        raise NativeSessionRefusal("the native session has an active turn")
    snapshot = NativeSessionSnapshot(session_id, path, signature, active, started, completed)
    snapshot.assert_unchanged()
    # Use decoded message bodies, not raw JSON (escapes split a wrapped prompt).
    bodies = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            event = json.loads(line)
            payload = event.get("payload") or {}
            if event.get("type") == "response_item" and payload.get("type") == "message":
                bodies.extend(str(part.get("text") or "")
                              for part in payload.get("content", []) if isinstance(part, dict))
            elif event.get("type") == "event_msg" and payload.get("type") in (
                    "user_message", "agent_message"):
                bodies.append(str(payload.get("message") or ""))
    if _signature(os.stat(path)) != signature:
        raise NativeSessionRefusal("native session changed during the prompt read")
    evidence = PromptSession(snapshot, "\n".join(bodies), pid, start)
    evidence.assert_unchanged()
    with _CACHE_LOCK:
        _CACHE[key] = evidence
        while len(_CACHE) > 8:
            _CACHE.popitem(last=False)
    return evidence
