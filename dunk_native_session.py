"""Fail-closed native-session activity guard for same-session Codex resume.

The guarded same-pane respawn replaces a pane's process; it does not stop a
turn that an older shared Codex app-server is still executing on the same
native session.  A blank/idle pane and the absence of a Dunking Sheep lease do
not prove that turn is finished (t416, measured 2026-10-29: turn
01a18644-... was still running when the replacement started turn
01a18645-...).

This module reads the authoritative native session state:
  - the exact session row in the private CODEX_HOME's state_5.sqlite;
  - the row's exact rollout JSONL path;
  - the rollout's task_started/task_complete lifecycle.

It refuses on an unfinished started turn, on ambiguous/missing/truncated
evidence, and on a rollout change between the check and the final respawn.
A quiet session is allowed only when the trace is authenticated by its exact
`session_meta` line and contains no active turn; an empty, missing, truncated or
wrong-session trace is never equivalent to quiet.  It never treats the pane
display, a lease, or process liveness as authority.

BOUND: the final stat re-read closes the window up to that read; a turn that
starts after it is not prevented. This is a bounded precondition, not atomic
coordination with the native session.
"""
from __future__ import annotations

import json
import os
import sqlite3
from dataclasses import dataclass
from pathlib import Path

from dunk_sqlite import connect as sqlite_connect


class NativeSessionRefusal(RuntimeError):
    """The native session cannot be proven quiescent; callers must refuse."""


class NativeSessionChanged(NativeSessionRefusal):
    """The rollout changed after the quiescence snapshot was taken."""


class NativeSessionBusy(NativeSessionRefusal):
    """The private state DB is busy; the quiescence check can be retried.

    Sanitized contract: a fixed message with no raw database text, marked
    retryable so callers can distinguish a busy store from a refusal.
    """

    retryable = True


@dataclass(frozen=True)
class ResumeRequest:
    session_id: str
    codex_home: str | None
    cwd: str | None


@dataclass(frozen=True)
class NativeSessionSnapshot:
    session_id: str
    rollout_path: str
    signature: tuple
    active_turns: tuple
    started_turns: tuple
    completed_turns: tuple

    def assert_unchanged(self):
        """Refuse if the exact rollout changed after this snapshot was read."""
        try:
            stat = os.stat(self.rollout_path)
        except OSError as error:
            raise NativeSessionChanged(
                f"native session rollout disappeared after the quiescence check: {error}"
            ) from None
        current = _signature(stat)
        if current != self.signature:
            raise NativeSessionChanged(
                "native session rollout changed after the quiescence check; "
                "the older turn may have started or advanced"
            )


def _signature(stat):
    return (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns)


def find_resume_request(argv):
    """Return the exact Codex resume request in an argv, or None.

    Only a token whose basename is ``codex`` starts the command; ``resume``
    after that names the native session.  A resume with no id (picker mode) is
    not a same-session request and is returned as session_id="".
    """
    tokens = [str(part) for part in (argv or ())]
    codex_at = None
    for index, token in enumerate(tokens):
        if os.path.basename(token).lower() == "codex":
            codex_at = index
            break
    if codex_at is None:
        return None
    home = None
    cwd = None
    session_id = ""
    found_resume = False
    for index, token in enumerate(tokens):
        if token.startswith("CODEX_HOME="):
            home = token.split("=", 1)[1]
        if token in ("-C", "--cd", "--cwd") and index + 1 < len(tokens):
            cwd = tokens[index + 1]
        if index > codex_at and token == "resume":
            found_resume = True
            if index + 1 < len(tokens) and not tokens[index + 1].startswith("-"):
                session_id = tokens[index + 1]
            break
    if not found_resume:
        return None
    return ResumeRequest(session_id=session_id, codex_home=home, cwd=cwd)


def _state_row(codex_home, session_id):
    path = Path(os.path.join(codex_home, "state_5.sqlite")).resolve()
    if not path.is_file():
        raise NativeSessionRefusal(
            f"native session guard cannot find private state DB {path}"
        )
    try:
        connection = sqlite_connect(path, readonly=True)
        connection.row_factory = sqlite3.Row
        try:
            row = connection.execute(
                "select id, rollout_path, cwd from threads where id = ?", (session_id,)
            ).fetchone()
        finally:
            connection.close()
    except sqlite3.Error as error:
        if isinstance(error, sqlite3.OperationalError) and (
            error.sqlite_errorcode in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED)
        ):
            raise NativeSessionBusy(
                "native session state DB busy; retry the request"
            ) from None
        raise NativeSessionRefusal(
            "native session guard could not read private state DB"
        ) from None
    if row is None:
        raise NativeSessionRefusal(
            f"native session {session_id!r} is not in the private state DB"
        )
    if str(row["id"]) != session_id:
        raise NativeSessionRefusal("native session state row id mismatch")
    rollout_path = os.path.realpath(str(row["rollout_path"] or ""))
    if not rollout_path or not os.path.isfile(rollout_path):
        raise NativeSessionRefusal(
            f"native session {session_id!r} rollout does not exist: {rollout_path!r}"
        )
    if session_id not in os.path.basename(rollout_path):
        raise NativeSessionRefusal(
            f"native session rollout path does not name session {session_id!r}: "
            f"{rollout_path!r}"
        )
    return row, rollout_path


def _read_rollout(path, session_id, expected_cwd=None):
    """Parse a rollout and return (signature, started, completed, active).

    Authenticated quiet means the first non-empty record is a session_meta for
    this exact session and cwd, and the complete file has no active turn.  An
    empty/missing/truncated file is a reading failure, not evidence of quiet.
    """
    try:
        before = _signature(os.stat(path))
        with open(path, "rb") as handle:
            data = handle.read()
        after = _signature(os.stat(path))
    except OSError as error:
        raise NativeSessionRefusal(f"native session rollout unreadable: {error}") from None
    if before != after:
        raise NativeSessionChanged(
            "native session rollout changed while it was being read"
        )
    if data and not data.endswith(b"\n"):
        raise NativeSessionRefusal("native session rollout ends in a partial line")

    started = []
    completed = set()
    state = {}
    authenticated = False
    for line_number, raw in enumerate(data.splitlines(), 1):
        if not raw.strip():
            continue
        try:
            event = json.loads(raw)
        except (TypeError, ValueError) as error:
            raise NativeSessionRefusal(
                f"native session rollout line {line_number} is malformed: {error}"
            ) from None
        if not isinstance(event, dict):
            raise NativeSessionRefusal(
                f"native session rollout line {line_number} is not an object"
            )
        if line_number == 1:
            if event.get("type") != "session_meta":
                raise NativeSessionRefusal(
                    "native session rollout has no authenticated session_meta"
                )
            meta = event.get("payload")
            if not isinstance(meta, dict):
                raise NativeSessionRefusal("native session_meta has no payload")
            meta_id = meta.get("session_id") or meta.get("id")
            if str(meta_id or "") != session_id:
                raise NativeSessionRefusal(
                    f"native session_meta names {meta_id!r}, not {session_id!r}"
                )
            meta_cwd = meta.get("cwd")
            if expected_cwd and meta_cwd:
                if os.path.realpath(str(meta_cwd)) != os.path.realpath(str(expected_cwd)):
                    raise NativeSessionRefusal(
                        f"native session_meta cwd {meta_cwd!r} does not match "
                        f"{expected_cwd!r}"
                    )
            authenticated = True
            continue
        if event.get("type") != "event_msg":
            continue
        payload = event.get("payload")
        if not isinstance(payload, dict):
            raise NativeSessionRefusal(
                f"native session rollout line {line_number} has no payload"
            )
        thread_id = payload.get("thread_id")
        if thread_id and str(thread_id) != session_id:
            raise NativeSessionRefusal(
                f"native session rollout line {line_number} names thread "
                f"{thread_id!r}, not {session_id!r}"
            )
        event_type = payload.get("type")
        if event_type not in ("task_started", "task_complete"):
            continue
        turn_id = payload.get("turn_id")
        if not turn_id:
            raise NativeSessionRefusal(
                f"native session {event_type} at line {line_number} has no turn id"
            )
        turn_id = str(turn_id)
        if event_type == "task_started":
            if turn_id in state:
                raise NativeSessionRefusal(
                    f"native session has duplicate task_started for turn {turn_id}"
                )
            state[turn_id] = {"started": event.get("timestamp"), "completed": None}
            started.append(turn_id)
        else:
            if turn_id not in state:
                raise NativeSessionRefusal(
                    f"native session task_complete for turn {turn_id} has no start"
                )
            if state[turn_id]["completed"] is not None:
                raise NativeSessionRefusal(
                    f"native session has duplicate task_complete for turn {turn_id}"
                )
            state[turn_id]["completed"] = event.get("timestamp")
            completed.add(turn_id)

    if not authenticated:
        raise NativeSessionRefusal(
            "native session rollout has no authenticated session_meta"
        )
    active = tuple(
        (turn_id, state[turn_id]["started"])
        for turn_id in started
        if state[turn_id]["completed"] is None
    )
    return after, tuple(started), tuple(sorted(completed)), active


def check_native_session_quiescence(argv, pane_cwd=None):
    """Return a native-session snapshot or refuse.

    None means the incoming command is not a same-session Codex resume.
    A same-session resume must carry an explicit isolated CODEX_HOME; it is
    resolved through the exact thread row, never guessed from the pane.
    """
    request = find_resume_request(argv)
    if request is None or not request.session_id:
        return None
    if not request.codex_home:
        raise NativeSessionRefusal(
            "same-session Codex resume requires an explicit isolated CODEX_HOME"
        )
    home = os.path.realpath(os.path.expanduser(request.codex_home))
    if home == os.path.realpath(os.path.expanduser("~/.codex")):
        raise NativeSessionRefusal(
            "same-session Codex resume must not use the shared ~/.codex home"
        )
    expected_cwd = request.cwd or pane_cwd
    row, rollout_path = _state_row(home, request.session_id)
    row_cwd = os.path.realpath(str(row["cwd"] or ""))
    if expected_cwd and row_cwd != os.path.realpath(expected_cwd):
        raise NativeSessionRefusal(
            f"native session cwd mismatch: state says {row_cwd!r}, "
            f"request says {os.path.realpath(expected_cwd)!r}"
        )
    signature, started, completed, active = _read_rollout(
        rollout_path, request.session_id, expected_cwd
    )
    if active:
        turns = ", ".join(
            f"{turn_id} (started {started_at or 'unknown'})"
            for turn_id, started_at in active
        )
        raise NativeSessionRefusal(
            f"native session {request.session_id} has unfinished turn(s): {turns}"
        )
    return NativeSessionSnapshot(
        session_id=request.session_id,
        rollout_path=rollout_path,
        signature=signature,
        active_turns=active,
        started_turns=started,
        completed_turns=completed,
    )
