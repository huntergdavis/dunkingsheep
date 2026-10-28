#!/usr/bin/env python3
"""In-pane Codex turn producer for Dunking Sheep.

Codex hooks (UserPromptSubmit / Stop) are the only turn boundary Codex exposes
that runs *inside* the agent's own process tree.  That matters: the daemon's
turn RPC authenticates the caller by kernel PID/start-time ancestry under the
backend's pane root (`Flock.require_turn_caller`), so a detached adapter that
merely asserts a pane label, cwd or session id is refused by design.

The measured fact behind this file (2026-10-28): with Codex's default shared
app-server daemon, hook processes run under the daemon and their ancestry does
not contain the pane root (3639953 -> 3493002 -> 1).  With `codex --no-daemon`
the app-server runs in the pane's own Codex process, and a hook is a direct
child of it, so the ancestry check passes without relaxing anything.

Contract (same shape as integrations/pi/turn-generation.patch):
  * start and end calls are serialised per Codex session;
  * the daemon's `--json turn start` token is captured and sent back on end;
  * an end is NEVER sent without a start token (no unbound/guessed turn);
  * a hook turn is matched by Codex's own `turn_id`, so a replayed or stale
    Stop cannot end the next turn;
  * the token is bound to the Codex process that started it; a replaced pane
    root makes the end refuse instead of falling back to a label;
  * failures are logged and swallowed: a hook must exit 0.  Codex treats a
    nonzero UserPromptSubmit exit as "block this prompt", so propagating a
    refusal here would stop the agent from taking any turn.

The pane is never supplied as a label.  The CLI resolves the caller's pane from
the terminal it is actually in, and the daemon re-checks the ancestry.  If the
terminal identity is unavailable, this producer emits no event.

State files live under ~/.local/state/dunkingsheep/codex-turns/ (override with
DUNKINGSHEEP_CODEX_STATE_DIR).  They contain a session id, Codex's turn id, the
daemon's turn token and the starter's pid/start time.  Tokens and prompts are
never written to the log.
"""
from __future__ import annotations

import fcntl
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

DUNK_BIN = os.environ.get("DUNKINGSHEEP_BIN") or "dunkingsheep"
TTL_S = float(os.environ.get("DUNKINGSHEEP_CODEX_TURN_TTL_S", "1800"))
END_WAIT_S = float(os.environ.get("DUNKINGSHEEP_CODEX_END_WAIT_S", "2.0"))
MAX_AGE_S = TTL_S + 60.0
MAX_DEPTH = 64
AGENT_NAMES = frozenset({"codex"})
SAFE_ID = re.compile(r"^[A-Za-z0-9._-]{1,200}$")


def _state_dir() -> Path:
    explicit = os.environ.get("DUNKINGSHEEP_CODEX_STATE_DIR")
    if explicit:
        return Path(explicit)
    xdg = os.environ.get("XDG_STATE_HOME")
    base = Path(xdg) if xdg else Path.home() / ".local" / "state"
    return base / "dunkingsheep" / "codex-turns"


def _log(message: str, *, path: Path | None = None) -> None:
    """Append one diagnostic line; never include turn tokens or prompt text."""
    try:
        directory = path or (_state_dir().parent)
        directory.mkdir(parents=True, exist_ok=True)
        with open(directory / "codex-turn.log", "a", encoding="utf-8") as handle:
            handle.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S%z')} {message}\n")
    except OSError:
        pass


def _read_event() -> dict | None:
    try:
        raw = sys.stdin.read()
        if not raw.strip():
            return None
        event = json.loads(raw)
    except (OSError, ValueError):
        return None
    return event if isinstance(event, dict) else None


def _stat(pid: int) -> dict | None:
    """The fields of /proc/<pid>/stat this producer needs, or None."""
    try:
        with open(f"/proc/{int(pid)}/stat", "r", encoding="utf-8",
                  errors="replace") as handle:
            text = handle.read()
        head, tail = text.rsplit(")", 1)
        comm = head.split("(", 1)[1]
        fields = tail.split()
        return {"pid": int(pid), "comm": comm, "state": fields[0],
                "ppid": int(fields[1]), "start": fields[19]}
    except (OSError, TypeError, ValueError, IndexError):
        return None


def _agent_name(pid: int, stat=_stat) -> str | None:
    info = stat(pid)
    if not info:
        return None
    comm = os.path.basename(info["comm"]).lower()
    if comm in AGENT_NAMES:
        return comm
    try:
        with open(f"/proc/{int(pid)}/cmdline", "rb") as handle:
            parts = [p.decode("utf-8", "replace")
                     for p in handle.read().split(b"\0") if p]
    except (OSError, TypeError, ValueError):
        parts = []
    for part in parts:
        if os.path.basename(part).lower() in AGENT_NAMES:
            return os.path.basename(part).lower()
    return None


def _root(pid: int | None = None, stat=_stat, agent_name=_agent_name) -> dict | None:
    """Nearest Codex agent above this process: the pane's own root in the
    --no-daemon deployment.  Returns {pid, start, comm} or None."""
    pid = os.getpid() if pid is None else int(pid)
    seen: set[int] = set()
    for _ in range(MAX_DEPTH):
        if pid <= 0 or pid in seen:
            return None
        seen.add(pid)
        info = stat(pid)
        if not info:
            return None
        name = agent_name(pid, stat)
        if name:
            return {"pid": info["pid"], "start": str(info["start"]),
                    "comm": name}
        pid = info["ppid"]
    return None


class _Lock:
    def __init__(self, path: Path):
        self.path = path
        self.handle = None

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.handle = open(self.path, "a+", encoding="utf-8")
        fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX)
        return self

    def __exit__(self, *_exc):
        if self.handle is not None:
            try:
                fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
            finally:
                self.handle.close()
                self.handle = None


def _paths(session_id: str) -> tuple[Path, Path]:
    directory = _state_dir()
    return directory / f"{session_id}.json", directory / f"{session_id}.lock"


def _read_state(path: Path) -> dict | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def _write_state(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temp = path.with_suffix(path.suffix + ".tmp")
    with open(temp, "w", encoding="utf-8") as handle:
        json.dump(state, handle, sort_keys=True)
    os.chmod(temp, 0o600)
    os.replace(temp, path)


def _remove_state(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        pass


def _run(args: list[str], cwd: str | None, timeout: float = 10.0):
    """Run the daemon CLI; return (code, parsed_json_or_None, stderr)."""
    try:
        proc = subprocess.run([DUNK_BIN, *args], cwd=cwd or None,
                              capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as error:
        return 1, None, str(error)
    parsed = None
    if proc.stdout.strip():
        try:
            parsed = json.loads(proc.stdout)
        except ValueError:
            parsed = None
    return proc.returncode, parsed, (proc.stderr or "")[:300]


def _valid_event(event: dict | None, need_turn: bool) -> str | None:
    if not isinstance(event, dict):
        return "no hook input"
    session = event.get("session_id")
    if not isinstance(session, str) or not SAFE_ID.match(session):
        return "missing or invalid session_id"
    if not isinstance(event.get("cwd"), str) or not event["cwd"]:
        return "missing cwd"
    if need_turn and (not isinstance(event.get("turn_id"), str)
                      or not SAFE_ID.match(event["turn_id"])):
        return "missing or invalid turn_id"
    return None


def start(event: dict | None) -> None:
    error = _valid_event(event, need_turn=True)
    if error:
        _log(f"start refused reason={error}")
        return
    session = event["session_id"]
    turn_id = event["turn_id"]
    root = _root()
    if not root:
        _log(f"start refused reason=no_codex_ancestor session={session[:12]}")
        return
    path, lock_path = _paths(session)
    now = time.time()
    with _Lock(lock_path):
        state = _read_state(path)
        if (state and state.get("codex_turn_id") == turn_id
                and state.get("root_pid") == root["pid"]
                and state.get("root_start") == root["start"]
                and float(state.get("expires_at", 0)) > now):
            _log(f"start duplicate session={session[:12]} root={root['pid']}")
            return
        code, result, stderr = _run([
            "--json", "turn", "start",
            "--source", "codex-hook", "--ttl-s", str(int(TTL_S)),
        ], event["cwd"])
        daemon_turn = result.get("turn_id") if isinstance(result, dict) else None
        if (code != 0 or not isinstance(daemon_turn, str) or not daemon_turn
                or not isinstance(result.get("pane_id"), str)):
            _log(f"start failed session={session[:12]} rc={code} "
                 f"error={stderr!r}")
            return
        _write_state(path, {
            "codex_turn_id": turn_id,
            "daemon_turn_id": daemon_turn,
            "session_id": session,
            "cwd": event["cwd"],
            "pane_id": result["pane_id"],
            "root_pid": root["pid"],
            "root_start": root["start"],
            "started_at": now,
            "expires_at": now + TTL_S,
        })
    _log(f"start ok session={session[:12]} pane={result['pane_id']} "
         f"root={root['pid']}")


def end(event: dict | None) -> None:
    error = _valid_event(event, need_turn=True)
    if error:
        _log(f"end refused reason={error}")
        return
    session = event["session_id"]
    turn_id = event["turn_id"]
    root = _root()
    if not root:
        _log(f"end refused reason=no_codex_ancestor session={session[:12]}")
        return
    path, lock_path = _paths(session)
    deadline = time.time() + END_WAIT_S
    state = None
    while True:
        with _Lock(lock_path):
            state = _read_state(path)
            if state is not None:
                break
        if time.time() >= deadline:
            break
        # A fast settled turn can beat its own start hook; wait briefly for a
        # start to publish its token.  Never invent one.
        time.sleep(0.05)
    if state is None:
        _log(f"end skipped reason=no_start_token session={session[:12]}")
        return
    if state.get("session_id") != session:
        _log(f"end refused reason=session_mismatch session={session[:12]}")
        return
    if state.get("codex_turn_id") != turn_id:
        _log(f"end refused reason=stale_turn session={session[:12]}")
        return
    if (state.get("root_pid") != root["pid"]
            or str(state.get("root_start")) != root["start"]):
        # A different Codex process now owns this pane.  The old token is not
        # evidence about the new turn, and the old root must not be guessed at.
        _log(f"end refused reason=replaced_root session={session[:12]} "
             f"stored={state.get('root_pid')} current={root['pid']}")
        return
    now = time.time()
    if float(state.get("expires_at", 0)) <= now:
        with _Lock(lock_path):
            if (_read_state(path) or {}).get("codex_turn_id") == turn_id:
                _remove_state(path)
        _log(f"end refused reason=expired_turn session={session[:12]}")
        return
    daemon_turn = state.get("daemon_turn_id")
    if not isinstance(daemon_turn, str) or not daemon_turn:
        with _Lock(lock_path):
            if (_read_state(path) or {}).get("codex_turn_id") == turn_id:
                _remove_state(path)
        _log(f"end skipped reason=no_start_token session={session[:12]}")
        return
    # Consume the token before the RPC: a replay cannot send it twice, and a
    # failed end leaves the daemon TTL as the backstop rather than an old token.
    with _Lock(lock_path):
        current = _read_state(path)
        if current is not None and current.get("codex_turn_id") == turn_id:
            _remove_state(path)
    code, result, stderr = _run([
        "--json", "turn", "end", "--turn-id", daemon_turn, "--reason", "stop",
    ], state.get("cwd"))
    ended = bool(isinstance(result, dict) and result.get("ended"))
    dispatch = result.get("dispatch") if isinstance(result, dict) else None
    action = dispatch.get("action") if isinstance(dispatch, dict) else None
    reason = dispatch.get("reason") if isinstance(dispatch, dict) else None
    _log(f"end session={session[:12]} rc={code} ended={ended} "
         f"dispatch={action} dispatch_reason={reason} error={stderr!r}")


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    action = args[0] if args else ""
    event = _read_event()
    if action == "start":
        start(event)
    elif action == "end":
        end(event)
    else:
        _log(f"ignored action={action!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
