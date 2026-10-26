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
import os
import time

STATES = ("working", "waiting", "idle", "unknown")

# How long an ENDED turn keeps supporting an "idle" claim. "Idle" is a claim about NOW: past
# this window the lane is silent, and silence is unknown rather than idle.
IDLE_CLAIM_S = float(os.environ.get("DUNKINGSHEEP_IDLE_CLAIM_S", str(6 * 3600)))


def turn_state(state, now=None, idle_claim_s=None):
    """THE one rule for what a pi turn means. Returns (state_or_None, since, reason).

    There must be exactly one derivation of this, because the desk and the dispatcher read the
    same turn and must not disagree about it (reviewer on t227: the roster said IDLE for a turn
    that had EXPIRED without ever ending, which is the case the TTL exists to prevent us from
    calling idle). `None` means "no answer", and the callers that need a word for that use
    "unknown" while the ones that can render a dash use null.
    """
    now = time.time() if now is None else now
    idle_claim_s = IDLE_CLAIM_S if idle_claim_s is None else idle_claim_s
    if not state:
        return None, None, ("no pi turn has ever been recorded for this pane (extension not "
                            "loaded, or the lane has not taken a turn since it started)")
    if state.get("expires_at", 0) > now:
        return "working", state.get("started_at"), None
    ended = state.get("ended_at")
    if ended:
        if now - ended <= idle_claim_s:
            return "idle", ended, None
        return None, ended, ("the last turn ended %dh ago: idle is a claim about now, and this "
                             "lane has been silent since" % int((now - ended) / 3600))
    # A turn that EXPIRED without an end is neither: the agent declared work and never said it
    # finished. The TTL exists so a crashed agent cannot hold mail forever - not so that we can
    # call it idle.
    return None, state.get("expires_at"), ("the last turn expired without an end: the agent "
                                           "declared work and never reported finishing")


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
    state, since, reason = turn_state(turns.get(pane_id) or {}, now=now)
    # An absent or unusable signal is UNKNOWN, never idle - the rule this module exists for.
    return {"state": state or "unknown", "since": since,
            "source": "pi turn events", "reason": reason}


def say(record):
    """One line for a log or a table."""
    if record["state"] == "unknown":
        return f"unknown ({record['source']}: {record['reason']})"
    since = record.get("since")
    return record["state"] + (f" since {int(now_hours(since))}h ago" if since else "")


def now_hours(at, now=None):
    now = now or time.time()
    return max(0.0, (now - float(at)) / 3600) if at else 0.0
