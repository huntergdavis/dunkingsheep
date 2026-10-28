#!/usr/bin/env python3
"""
Who is asking? A process identity for task leases, independent of herdr.

The pane id (HERDR_PANE_ID / TMUX_PANE) is the registry key. The liveness check
is a process id plus its kernel start time, because a pid alone is reused. The
stable process is the **agent** (claude/codex/pi/opencode/...), not the
short-lived CLI that opens the socket: the CLI exits after every command while
the agent keeps working. So the daemon walks from the socket peer up its parent
chain and takes the nearest agent ancestor; with no agent (a human shell) it
takes the session leader, which is the stable terminal frame.

`/proc/<pid>/stat` gives the parent, session and starttime. A zombie or a
different starttime means the caller is dead even if the pid now belongs to
somebody else. This is the liveness `task_reap` uses when the shell pane is
still alive but the agent inside it crashed.
"""

import os

# The same agent executables the terminal backend knows. Kept as a plain set
# so callers.py stays independent of herdr/tmux.
AGENT_NAMES = frozenset({
    "pi", "claude", "codex", "gemini", "cursor", "devin", "agy", "cline",
    "omp", "mastracode", "opencode", "copilot", "kimi", "kiro", "droid",
    "amp", "grok", "hermes", "kilo", "qodercli", "qwen", "letta", "maki",
    "muse",
})
MAX_DEPTH = 12
DEAD_STATES = frozenset({"Z", "X", "x"})


def _stat(pid):
    """Parse the fields we need from /proc/<pid>/stat, or None."""
    try:
        with open(f"/proc/{int(pid)}/stat", "r", encoding="utf-8",
                  errors="replace") as handle:
            text = handle.read()
    except (OSError, TypeError, ValueError):
        return None
    try:
        # comm is parenthesised and may itself contain spaces or parens.
        head, tail = text.rsplit(")", 1)
        comm = head.split("(", 1)[1]
        fields = tail.split()
        return {
            "pid": int(pid),
            "comm": comm,
            "state": fields[0],
            "ppid": int(fields[1]),
            "session": int(fields[3]),
            "start": fields[19],
        }
    except (IndexError, ValueError):
        return None


def process_start(pid):
    info = _stat(pid)
    return info["start"] if info else None


def process_alive(pid, start=None):
    """True only for a non-zombie process whose starttime still matches."""
    info = _stat(pid)
    if not info or info["state"] in DEAD_STATES:
        return False
    if start is not None and str(info["start"]) != str(start):
        return False
    return True


def _cmdline(pid):
    try:
        with open(f"/proc/{int(pid)}/cmdline", "rb") as handle:
            return [part.decode("utf-8", "replace")
                    for part in handle.read().split(b"\0") if part]
    except (OSError, TypeError, ValueError):
        return []


def agent_name(pid):
    """The agent executable this process is, if any."""
    info = _stat(pid)
    if not info:
        return None
    comm = os.path.basename(info["comm"]).lower()
    if comm in AGENT_NAMES:
        return comm
    for part in _cmdline(pid):
        base = os.path.basename(part).lower()
        if base in AGENT_NAMES:
            return base
    return None


def _caller(pid, start, name, pane_id, source):
    return {"pane_id": pane_id, "pid": int(pid) if pid else None,
            "start": str(start) if start is not None else None,
            "name": name, "source": source}


def caller_from_peer(peer_pid, pane_id=None, max_depth=MAX_DEPTH):
    """Resolve the stable caller behind a socket peer.

    Walk parents to the nearest agent; if there is none, use the peer's session
    leader (the terminal/shell frame). The returned dict carries pane_id, pid,
    start and name; `caller_alive()` is the liveness check.
    """
    try:
        pid = int(peer_pid)
    except (TypeError, ValueError):
        return _caller(None, None, None, pane_id, "none")
    chain = []
    seen = set()
    for _ in range(max(1, int(max_depth))):
        if pid <= 0 or pid in seen:
            break
        seen.add(pid)
        info = _stat(pid)
        if not info:
            break
        chain.append(info)
        name = agent_name(pid)
        if name:
            return _caller(pid, info["start"], name, pane_id, "agent")
        pid = info["ppid"]
    if chain:
        leader = _stat(chain[0]["session"])
        if leader and leader["pid"] != chain[0]["pid"]:
            return _caller(leader["pid"], leader["start"], leader["comm"],
                           pane_id, "session")
        info = chain[0]
        return _caller(info["pid"], info["start"], info["comm"], pane_id,
                       "peer")
    return _caller(None, None, None, pane_id, "none")


def caller_alive(caller):
    """Liveness for a stored caller dict; missing identity means not alive."""
    if not caller:
        return False
    pid = caller.get("pid")
    if not pid:
        return False
    return process_alive(pid, caller.get("start"))


def caller_belongs_to_pane(caller, pane_pid):
    """Bind a kernel-attributed caller to the backend's pane process ancestry.

    A client-supplied pane label is not authentication. Both the stable caller's
    PID/start time and the ancestry leading to the pane process must match.
    Missing proc/backend information fails closed; no environment label is used.
    """
    return caller_pane_auth_failure(caller, pane_pid) is None


def caller_pane_auth_failure(caller, pane_pid):
    """Return the first failed binding condition, or None on authentication.

    Keep the same fail-closed walk as caller_belongs_to_pane. Reasons describe
    observations, not guesses about why a process disappeared or was reparented.
    """
    if not caller:
        return "caller_missing"
    if not caller.get("start"):
        return "caller_start_missing"
    if not pane_pid:
        return "pane_pid_missing"
    try:
        pid = int(caller["pid"])
    except (KeyError, TypeError, ValueError):
        return "caller_pid_invalid"
    try:
        root = int(pane_pid)
    except (KeyError, TypeError, ValueError):
        return "pane_pid_invalid"
    info = _stat(pid)
    if not info:
        return "caller_proc_missing"
    if info["state"] in DEAD_STATES:
        return "caller_dead"
    if str(info["start"]) != str(caller["start"]):
        return "caller_start_mismatch"
    seen = set()
    for _ in range(64):
        if not info:
            return "ancestry_proc_missing"
        if info["state"] in DEAD_STATES:
            return "ancestry_dead"
        if pid <= 1:
            return "pane_not_in_ancestry"
        if pid in seen:
            return "ancestry_cycle"
        if pid == root:
            return None
        seen.add(pid)
        pid = info["ppid"]
        info = _stat(pid)
    return "ancestry_depth_exceeded"
