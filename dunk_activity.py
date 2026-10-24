"""What is an agent DOING - from the tool that knows, not from the screen.

Screen scraping answered this for a year and lied in both directions: a working pi
lane read 'ready' because pi reports no pane status, and a live pane read 'dead'
because its transcript quoted the marker. Each harness now exposes its own truth:

    claude  `claude agents --json --cwd <dir>` -> busy / waiting / idle (+ waitingFor)
    pi      its extension's turn events        -> a declared turn with a TTL
    codex   turn/started and turn/completed on its local socket (NOT WIRED YET)

THE RULE, and the reason this module exists: an ABSENT signal is UNKNOWN, never
idle. "Assume idle" has cost us four stalls in one day; "unknown" costs a lane one
question.
"""
import time

STATES = ("working", "waiting", "idle", "unknown")


def activity(pane, *, claude_status=None, turns=None, now=None):
    """The best available answer about one lane's activity, with its source.

    Returns {"state", "since", "source", "reason"}. `reason` is non-None only when
    the state could not be established - never to decorate a real answer.
    """
    now = now or time.time()
    harness = str(pane.get("agent") or "").lower()
    if harness == "claude":
        return _claude(pane, claude_status, now)
    if harness == "pi":
        return _pi(pane, turns, now)
    if harness == "codex":
        return {"state": "unknown", "since": None, "source": "codex",
                "reason": "codex has no activity source wired yet (its turns arrive "
                          "on a local socket; this daemon does not subscribe)"}
    return {"state": "unknown", "since": None, "source": harness or "none",
            "reason": f"no activity source for harness {harness!r}"}


def _claude(pane, claude_status, now):
    if claude_status is None:
        return {"state": "unknown", "since": None, "source": "claude",
                "reason": "no claude status client available"}
    try:
        session = claude_status.session_for_pane(pane)
    except Exception as error:                      # noqa: BLE001 - a probe must not raise
        return {"state": "unknown", "since": None, "source": "claude",
                "reason": f"claude agents --json failed: {error}"}
    if not session:
        # A missing entry is NOT idle: the session may be starting, or in another
        # cwd. Saying 'unknown' is the only honest answer.
        return {"state": "unknown", "since": None, "source": "claude agents --json",
                "reason": "no claude session matches this pane's cwd"}
    status = session.get("status")
    mapping = {"busy": "working", "waiting": "waiting", "idle": "idle"}
    state = mapping.get(str(status))
    if state is None:
        return {"state": "unknown", "since": None, "source": "claude agents --json",
                "reason": f"claude reported status {status!r}"}
    return {"state": state, "since": None, "source": "claude agents --json",
            "reason": None, "waiting_for": session.get("waitingFor")}


def _pi(pane, turns, now):
    pane_id = pane.get("pane_id")
    if turns is None:
        return {"state": "unknown", "since": None, "source": "pi turn events",
                "reason": "no turn state available"}
    state = turns.get(pane_id) or {}
    if state and state.get("expires_at", 0) > now:
        return {"state": "working", "since": state.get("started_at"),
                "source": "pi turn events", "reason": None}
    if state:
        # A turn that has ENDED is the only evidence of idleness here, and its end
        # time is exact: the extension posted it.
        return {"state": "idle", "since": state.get("ended_at") or state.get("expires_at"),
                "source": "pi turn events", "reason": None}
    # pi docs no times are recorded for this pane at all: no turn has ever started,
    # or the extension is not loaded. Both are unknown, not idle.
    return {"state": "unknown", "since": None, "source": "pi turn events",
            "reason": "no pi turn has ever been recorded for this pane (extension "
                      "not loaded, or the lane has not taken a turn since it started)"}


def say(record):
    """One line for a log or a table."""
    if record["state"] == "unknown":
        return f"unknown ({record['source']}: {record['reason']})"
    since = record.get("since")
    return record["state"] + (f" since {int(now_hours(since))}h ago" if since else "")


def now_hours(at, now=None):
    now = now or time.time()
    return max(0.0, (now - float(at)) / 3600) if at else 0.0
