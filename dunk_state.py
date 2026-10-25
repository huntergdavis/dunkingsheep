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
    session_reason TEXT,
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
        self.db = __import__("dunk_inbox").connect(path)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            self._migrate()
            self.db.commit()

    def _migrate(self):
        """Add columns an older database is missing, without losing its rows.

        session_reason exists because a null session_id used to be explained with the
        MODEL reason string ("the launching argv carries no model flag"), which is false
        for session_id and left a restore with nothing to refuse on. A field's reason
        must name its own field.
        """
        cols = {row[1] for row in self.db.execute("PRAGMA table_info(agent_state)")}
        if "session_reason" not in cols:
            self.db.execute("ALTER TABLE agent_state ADD COLUMN session_reason TEXT")

    def add(self, rows, at):
        with self._lock:
            self.db.executemany(
                "INSERT INTO agent_state (at, pane_id, project, harness, cwd, argv, session_id, "
                "session_reason, model, model_reason, thinking, thinking_reason, state, "
                "state_source, state_reason, task_id, source) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                [(at, row["pane_id"], row.get("project"), row.get("harness"), row.get("cwd"),
                  json.dumps(row.get("argv")) if row.get("argv") else None,
                  row.get("session_id"), row.get("session_reason"),
                  row.get("model"), row.get("model_reason"),
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


_SESSION_FLAGS = ("--session", "--resume")
_AGENT_EXES = ("claude", "pi", "codex", "opencode")
_SESSION_ROOTS = (
    os.path.expanduser("~/.pi/agent/sessions"),
    os.path.expanduser("~/.claude/projects"),
    os.path.expanduser("~/.codex/sessions"),
)


def _proc_children():
    """pid -> [child pids], from /proc. Linux only; an empty map elsewhere."""
    kids = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return kids
    for entry in entries:
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/stat", "rb") as handle:
                raw = handle.read().decode("utf-8", "replace")
            ppid = int(raw.rsplit(")", 1)[1].split()[1])
        except (OSError, ValueError, IndexError):
            continue
        kids.setdefault(ppid, []).append(int(entry))
    return kids


def _descendants(pid, kids=None):
    kids = kids if kids is not None else _proc_children()
    out, queue = [], [int(pid)]
    while queue:
        here = queue.pop()
        for child in kids.get(here, ()):
            out.append(child)
            queue.append(child)
    return out


def _cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            return [part for part in handle.read().decode("utf-8", "replace").split("\0") if part]
    except OSError:
        return []


def _open_session_file(pid):
    """The session file this process has open, if any. Identity from the PROCESS, not the cwd."""
    try:
        names = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        return None
    for name in names:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{name}")
        except OSError:
            continue
        if not target.endswith(".jsonl"):
            continue
        if any(target.startswith(root) for root in _SESSION_ROOTS):
            return target
    return None


def _session_id_from_path(path):
    stem = os.path.basename(path).rsplit(".", 1)[0]
    # pi: 2026-10-24T18-04-46-250Z_01a16f16-...  claude: <uuid>
    return stem.split("_")[-1] if "_" in stem else stem


def _env_of(pid):
    try:
        with open(f"/proc/{pid}/environ", "rb") as handle:
            raw = handle.read().decode("utf-8", "replace").split("\0")
    except OSError:
        return {}
    out = {}
    for item in raw:
        if "=" in item:
            key, value = item.split("=", 1)
            out[key] = value
    return out


def _proc_start(pid):
    """Wall-clock start time of a pid, from /proc/<pid>/stat starttime + boot time."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as handle:
            raw = handle.read().decode("utf-8", "replace")
        ticks = int(raw.rsplit(")", 1)[1].split()[19])
        with open("/proc/stat", encoding="utf-8") as handle:
            for line in handle:
                if line.startswith("btime "):
                    return int(line.split()[1]) + ticks / os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError, IndexError):
        return None
    return None


def _active_session_file(cwd, since, slack=90):
    """The session file this workspace is actually being written to.

    Evidence, not guessing: a candidate must live in THIS workspace's session directory
    AND have been written at or after the pane's agent started (minus a small slack for
    the file being created a moment before the process is observable). The newest such
    file wins, and the caller is told when more than one qualifies.
    """
    slug = _cwd_slug(cwd)
    if not slug:
        return None, 0
    cutoff = (since - slack) if since else None
    best, newer, count = None, 0.0, 0
    for root in _SESSION_ROOTS:
        directory = os.path.join(root, slug)
        try:
            names = os.listdir(directory)
        except OSError:
            continue
        for name in names:
            if not name.endswith(".jsonl"):
                continue
            path = os.path.join(directory, name)
            try:
                mtime = os.path.getmtime(path)
            except OSError:
                continue
            if cutoff is not None and mtime < cutoff:
                continue
            count += 1
            if mtime > newer:
                best, newer = path, mtime
    return best, count


def _cwd_slug(cwd):
    """The sessions-directory slug a harness uses for a working directory."""
    if not cwd:
        return None
    return "--" + str(cwd).strip("/").replace("/", "-") + "--"


def session_of(pane_pid, launch=None, kids=None, cwd=None):
    """(session_id, reason) for a live pane, read from its agent PROCESS.

    Why not launches.json alone: the flip left launch rows that carry another lane's
    session (two dead rows carried the VP's), so a recorded id is only trusted when its
    file exists, and the process in this pane wins over any row. Why not the cwd: several
    lanes share a workspace, so a directory can hold someone else's session.
    """
    launch = launch or {}
    recorded = launch.get("session") or launch.get("session_id")
    if not pane_pid:
        return recorded, ("the pane carries no pid, so its session cannot be read from the "
                          "process; using the launch row")
    try:
        pid = int(pane_pid)
    except (TypeError, ValueError):
        return recorded, "the pane pid is not a number, so its session cannot be read"
    pids = [pid] + _descendants(pid, kids)
    agent_pids = []
    for candidate in pids:
        argv = _cmdline(candidate)
        if not argv:
            continue
        head = os.path.basename(argv[0])
        if head in _AGENT_EXES or any(os.path.basename(tok) in _AGENT_EXES for tok in argv[:3]):
            agent_pids.append((candidate, argv))
    if not agent_pids:
        return None, "no agent process under this pane (it is a shell), so there is no session"
    for candidate, argv in agent_pids:
        value = _flag(argv, _SESSION_FLAGS)
        if not value:
            for index, token in enumerate(argv[:-1]):
                if token == "resume":
                    value = argv[index + 1]
                    break
        if value:
            if value.endswith(".jsonl"):
                return _session_id_from_path(value), None
            if os.path.sep in value:
                return _session_id_from_path(value), None
            return value, None
    # Environment next: a harness that exports its session (pi does) is authoritative for
    # the PROCESS - but only when the exported session belongs to this pane's workspace.
    # On 2026-10-24 the cos pane's environment carried the VP's session file, verbatim,
    # because the pane inherited it; trusting env without that check would have recorded
    # one lane's conversation against another lane, which is the defect this incident is
    # about. A mismatch is reported, never silently used.
    # A session file being written for this workspace right now outranks an environment
    # claim: the environment can be INHERITED from a pane created before this one (on
    # 2026-10-24 the tmux global env carried the VP's session, and a lane that inherited it
    # looked correct because the workspace matched while the conversation did not).
    started = _proc_start(agent_pids[0][0])
    active, candidates = _active_session_file(cwd, started)
    slug = _cwd_slug(cwd)
    for candidate, _argv in agent_pids:
        env = _env_of(candidate)
        sess_id = env.get("PI_SESSION_ID")
        sess_file = env.get("PI_SESSION_FILE") or ""
        if not sess_id and sess_file:
            sess_id = _session_id_from_path(sess_file)
        if sess_id:
            if slug and sess_file and slug not in sess_file:
                if active:
                    active_id = _session_id_from_path(active)
                    return active_id, (f"the environment names session {sess_id[:8]} from another "
                                       f"workspace - an inherited claim, not this pane's session; "
                                       f"recording {active_id[:8]}, the live file for this workspace")
                return None, (f"the process environment names session {sess_id[:8]} from another "
                              f"workspace ({os.path.basename(os.path.dirname(sess_file))}) - an "
                              f"inherited claim, not this pane's session")
            if active:
                active_id = _session_id_from_path(active)
                if active_id.split("-")[0] != sess_id.split("-")[0]:
                    return active_id, (f"the environment claims session {sess_id[:8]}, but the only "
                                       f"session file in this workspace written since the pane "
                                       f"started is {active_id[:8]} - recording the live file and "
                                       f"flagging the environment claim as inherited/stale")
            return sess_id, None
    for candidate, _argv in agent_pids:
        path = _open_session_file(candidate)
        if path:
            return _session_id_from_path(path), None
    if active:
        active_id = _session_id_from_path(active)
        if candidates > 1:
            return active_id, (f"no session in argv, environment or open files; recording the newest "
                               f"of {candidates} session files written since this pane started - "
                               f"{active_id[:8]} - which is the weakest admissible source")
        return active_id, (f"no session in argv, environment or open files; recording the only "
                           f"session file written in this workspace since the pane started "
                           f"({active_id[:8]})")
    if recorded:
        return recorded, ("no session in the agent's argv and none open; keeping the recorded "
                          "launch session")
    return None, ("the agent's argv carries no --session/--resume and it has no session file "
                  "open, so this capture cannot say which conversation is in this pane")


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
        session, session_reason = session_of(lane.get("pane_pid"),
                                             {"session": launch.get("session") or launch.get("session_id")},
                                             cwd=lane.get("cwd"))
        env = {}
        if lane.get("pane_pid"):
            for candidate in [int(lane["pane_pid"])] + _descendants(lane["pane_pid"]):
                found = _env_of(candidate)
                if found.get("PI_SESSION_ID") or found.get("PI_MODEL"):
                    env = found
                    break
        env_matches = bool(env) and (not session or env.get("PI_SESSION_ID") == session)
        if not model and env_matches and env.get("PI_MODEL"):
            model = env["PI_MODEL"]
        if not thinking and env_matches and env.get("PI_REASONING_LEVEL"):
            thinking = env["PI_REASONING_LEVEL"]
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
            "session_id": session,
            # Kept even when a session WAS resolved: a lane whose session came from a live
            # file while its environment claimed another one must still say so.
            "session_reason": session_reason,
            "model": model,
            "model_reason": None if model else (
                "no --model flag in the argv, and the process environment names no model"
                if launch else
                "no launch row for this pane: no argv to read a model from, and the process "
                "environment names no model"),
            "thinking": thinking,
            "thinking_reason": None if thinking else (
                "no --thinking flag in the argv, and the process environment names no "
                "reasoning level"
                if launch else
                "no launch row for this pane: no argv to read a thinking level from, and the "
                "process environment names no reasoning level"),
            "state": record["state"],
            "state_source": record["source"],
            "state_reason": record["reason"],
            "task_id": tasks[0]["task_id"] if tasks else None,
            "source": source,
        })
    store.add(rows, now)
    return {"at": now, "rows": len(rows), "source": source}
