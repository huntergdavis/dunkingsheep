#!/usr/bin/env python3
"""
Dunking Sheep core: the flock of dunkers and their timers.

This module is the single mechanism behind every control surface. The TUI's
keyboard, the unix-socket API, the command-line tool and the MCP server all
end up calling methods on `Flock`. Nothing in here knows about curses, sockets
or JSON-RPC; it only knows dunkers, herdr panes and time.

A *dunker* (or *dunk*) is: a target herdr pane, a text, an interval, and a
running flag. While running, a per-dunker thread counts down and delivers the
text (plus Enter) to the pane through `HerdrClient`, then starts over.

Orchestration extras on top of the classic Dunking Bird model:

- text templates: `{id}`, `{name}`, `{count}`, `{target}`, `{interval}`,
  `{time}` and `{date}` expand at send time (other braces are left alone),
  so a dunk can tell the agent it lands on which dunk to remove.
- `only_when="idle"`: defer a send while herdr reports the target agent as
  working or blocked; re-check every few seconds.
- `max_sends=N`: remove the dunk automatically after its N-th send.
- `skip_if_unconsumed=True`: backpressure. A scheduled send becomes a no-op
  when the previous send is still sitting unread in the target's input box (see
  `Flock._previous_send_consumed`). Judged by reading the pane, never by whether
  the agent is merely busy.
- `hold_while_typing=True` (the default): never type over a human. Before a
  send the daemon reads the target's input box with its styling and, if the box
  holds text someone is composing, holds the send and re-checks every minute
  until the box is clear (see `Flock._human_typing`). The last of those reads
  happens with the send lock held (`Flock.send_guarded`), so a draft that
  appears while this send queues behind another one still stops it.
- confirmed submit: after typing, the daemon reads the input box back and
  presses Enter again while its own text is still sitting there (see
  `Flock.submit_text`), because one Enter does not reliably submit a long
  message. A send that never leaves the box is reported as failed, not sent.
- persistence: the flock is saved to a JSON file on every structural change
  and restored (running dunks included) when a new flock is created.
- herdr layout control: `list_workspaces`, `create_workspace`, `create_tab`,
  `split_pane`, `start_agent`, `rename`, `focus`, `close`, plus the
  `herdr_command` passthrough for anything else the herdr CLI can do. An admin
  agent builds its team here, then dunks on it.
"""

import contextlib
import functools
import hashlib
import json
import logging
import os
import re
import shlex
import threading
import time

from pathlib import Path

VERSION = "2.70.86"

def code_identity(path=None):
    """Hash the flat application module tree, not just dunk_core.py.

    All top-level Python modules plus the CLI entrypoint are included, even if
    lazily imported. Tests, tools, integrations and third-party libraries are
    outside this application tree. A file path remains supported for callers
    asking for a single-file identity. Mtimes are informational only.
    """
    target = Path(path) if path is not None else Path(__file__).resolve().parent
    modules, mtimes, errors = {}, [], []
    try:
        files = sorted(target.glob("*.py")) if target.is_dir() else [target]
        if target.is_dir() and (target / "dunkingsheep").exists():
            files.append(target / "dunkingsheep")
        if not files:
            raise OSError("empty application module tree")
        for source in files:
            try:
                modules[source.name] = hashlib.sha256(source.read_bytes()).hexdigest()
                mtimes.append(source.stat().st_mtime)
            except OSError:
                modules[source.name] = None
                errors.append(source.name)
    except OSError:
        errors.append("<module tree>")
    digest = (hashlib.sha256(json.dumps(modules, sort_keys=True).encode()).hexdigest()[:12]
              if not errors else None)
    return {"code_hash": digest, "code_mtime": max(mtimes, default=None),
            "code_modules": modules, "code_errors": sorted(errors)}


# Freeze once, before importing application dependencies. Status must compare
# this process's import-time tree with disk, never two fresh disk snapshots.
# As with Python imports themselves, deployment must not rewrite source during
# startup; this is a source-tree identity, not attestation of loaded bytecode.
CODE_IDENTITY = code_identity()


def code_is_stale(identity=None, current=None):
    """Compare the import-time application tree with the current disk tree."""
    identity = CODE_IDENTITY if identity is None else identity
    current = code_identity() if current is None else current
    if not identity.get("code_hash") or not current.get("code_hash"):
        return None
    if "code_modules" in identity and "code_modules" in current:
        return identity["code_modules"] != current["code_modules"]
    return identity["code_hash"] != current["code_hash"]


def code_status():
    """One disk read per status request, including names and read failures."""
    current = code_identity()
    before, after = CODE_IDENTITY["code_modules"], current["code_modules"]
    errors = sorted(set(CODE_IDENTITY["code_errors"] + current["code_errors"]))
    changed = sorted(name for name in before.keys() | after.keys()
                     if name not in errors and before.get(name) != after.get(name))
    return {"code_hash": CODE_IDENTITY["code_hash"],
            "code_mtime": CODE_IDENTITY["code_mtime"],
            "code_stale": code_is_stale(CODE_IDENTITY, current),
            "code_changed_modules": changed,
            "code_identity_errors": errors}


from dunk_inbox import Inbox, InboxError
from dunk_tasks import Tasks, TaskError, GLOBAL as TASK_GLOBAL, validate_scope as validate_task_scope
from dunk_superusers import Superusers, SuperuserError, DEFAULT_VP_NAME
from dunk_memory import MemoryStore, MemoryIngestor, memory_sources
from dunk_metrics import Metrics, SCHEMA_VERSION as METRICS_SCHEMA_VERSION
import dunk_metrics
from dunk_terminal import get_backend
from dunk_callers import process_alive
import dunk_activity
from dunk_claude import ClaudeStatus


CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
STATE_FILE = os.path.join(CONFIG_DIR, "dunks.json")
LAUNCHES_FILE = os.path.join(CONFIG_DIR, "launches.json")

# Agent states during which an `only_when="idle"` dunk holds its send.
BUSY_STATUSES = ("working", "blocked")
# Pane ids are 'w<workspace>:p<number>' where the workspace may be a LETTER:
# wR:p5. A digits-only pattern meant 'wR:p9' was not recognised as a pane id at
# all, so a dead pane id fell through to becoming a mailbox name.
_PANE_ID_RE = re.compile(r"^(?:[A-Za-z0-9_.-]+:p\d+|%\d+)$")
# A herdr pane id in its general spelling (w9:pH, wR:p5, w11:p1). The narrow
# form above only covers the numeric suffix, and the lanes whose ids end in a
# letter - the vp lane among them - slipped through to become brand new mailbox
# names: 'w9:pH' accepted mail and queued it into nothing (2026-10-24).
_HERDR_ID_RE = re.compile(r"^w\d+:p[A-Za-z0-9_.-]+$")
PANE_ID_RE = _PANE_ID_RE
_TERM_ID_RE = re.compile(r"^term_[0-9A-Za-z]+$")
# 'codex-poolrad2 (wR:p5)': a name with its pane id appended. The parenthetical is
# the ONLY unambiguous part, so it decides where the mail goes.
_EMBEDDED_PANE_RE = re.compile(r"\(\s*([A-Za-z0-9_.-]+:p\d+|%\d+)\s*\)\s*$")
IDLE_POLL_S = 5
# A poll loop must never spin, even when a test sets the floor to 0 (t212): each poll forks tmux, and
# through the guard shim that is two more processes. A leaked test daemon with a zero delay is a fork
# storm - 218 guards were measured at load 78 on 2026-10-27.
MIN_IDLE_POLL_S = 0.5


def idle_poll_wait(stop_event, idle_poll_s=None):
    """One idle-poll wait, floored (t212).

    Extracted so it can be tested BEHAVIOURALLY: a fake stop event records the argument it was given,
    so a loop that stops applying the floor fails the test. A test that computes max() itself and
    asserts a property of Python's max() is a tautology - it stays green if the loop never consults the
    floor (reviewer, PR #48).
    """
    interval = IDLE_POLL_S if idle_poll_s is None else idle_poll_s
    return stop_event.wait(max(interval, MIN_IDLE_POLL_S))
# A restored dunk whose scheduled send is already in the past fires after this.
RESTORE_GRACE_S = 5
DEFAULT_TEXT = "continue"
DEFAULT_INTERVAL_MINUTES = 10.0
ONLY_WHEN_CHOICES = (None, "idle")
# Deterministic task coordination cadence. The reaper requeues expired/dead
# claims; the pusher feeds idle followers without any agent polling.
TASK_REAP_S = float(os.environ.get("DUNKINGSHEEP_REAP_S", "15"))
# A turn declared by a harness (pi/claude) holds sends until turn-end. A missed
# turn-end (crash/force-quit) must never hold mail forever: the state expires,
# the hold releases, and turn.expired is recorded with the reason.
TURN_TTL_S = float(os.environ.get("DUNKINGSHEEP_TURN_TTL_S", "1800"))
# THE TURN-END TRIGGER (owner's top priority, t276). A turn lasts 33-448 s while a
# keep-moving dunk fires every 60-120 MINUTES, so a lane sat unprompted ~95% of the time
# by construction. turn.end is a signal the daemon already receives and did nothing
# with; now it hands the lane its next prompt while it is still warm. This does NOT
# touch the timer cadence (faster timers against dialogs only make more blocked sends),
# and it is not the codex/opencode hook (that is t264): it fires only where a harness
# already reports a turn end.
TURN_END_DISPATCH = os.environ.get(
    "DUNKINGSHEEP_TURN_END_DISPATCH", "1").strip().lower() not in (
        "0", "false", "no", "off")
# How long turn_end waits for its nudge to land before answering. The pi extension
# calls `turn end` with a 10 s timeout and never reads the answer, so this is a bound on
# the REQUEST, not on the send: anything not delivered by then stays in the outbox and
# the worker retries it once the box clears. It is deliberately not longer - a turn end
# must never become a slow request.
TURN_END_WAIT_S = float(os.environ.get("DUNKINGSHEEP_TURN_END_WAIT_S", "4"))
# The sidebar poll resolves the caller's own mailbox by walking the pane list
# every time; that walk is the ~2-4 s cost, not the inbox SQL. Cache only the
# self->mailbox mapping, briefly.
MAILBOX_CACHE_S = float(os.environ.get("DUNKINGSHEEP_MAILBOX_CACHE_S", "30"))
# Daemon-side view cache for the poll-shaped commands. Derived from: the dunk
# list, poll_status, and the caller's inbox count. Invalidated by every dunk /
# inbox / task mutation (they all call _save or inbox/task writes); max
# staleness is this TTL past the last invalidation. `fresh=True` bypasses it
# for a send/ack/claim that needs certainty.
VIEW_CACHE_S = float(os.environ.get("DUNKINGSHEEP_VIEW_CACHE_S", "5"))
TASK_PUSH_S = float(os.environ.get("DUNKINGSHEEP_PUSH_S", "10"))
# How many trailing pane lines to inspect for a still-unconsumed previous send,
# and how much of that text to look for (whitespace-collapsed).
# Reading the pane's visible screen to decide whether a send was consumed.
CONSUMPTION_READ_LINES = 40
CONSUMPTION_PREFIX_CHARS = 80
# Prompt sigils that mark the start of an agent's input box (Claude ❯, Codex ›).
INPUT_BOX_SIGILS = ("\u276f", "\u203a", ">")
# Substrings an agent shows *inside the input box* when it has collapsed a long
# or pasted input it has not yet submitted. Matched only within the box region,
# so transcript-collapse chips ("ctrl + t to view transcript") never count.
MAX_UNKNOWN_SKIPS = 3
COLLAPSED_INPUT_MARKERS = (
    "ctrl+o to expand",
    "paste again to expand",
    "pasted text",
    "pasted content",
    "[image",
)
# While a human is typing in the target's input box, a dunk re-checks this often.
TYPING_RECHECK_S = 60
# A queued direct message re-checks this often (it is waiting to be delivered,
# not scheduled for later, so it should land soon after the human finishes).
MESSAGE_RECHECK_S = 5
# The box must read clear on two checks this far apart before anything is
# sent, so a pause between thoughts is not mistaken for being done.
TYPING_SETTLE_S = 1.0
# Delivered / failed / cancelled messages kept for list_messages.
MESSAGE_HISTORY = 50
# How many decided message ids travel with the state file (t249). The file only ever holds
# QUEUED rows, so a verdict older than this cannot correspond to a row still in it.
DECIDED_KEPT = 500
# Pressing Enter once does not reliably submit. Measured on a live Codex pane
# 2026-10-10: a ~2.2k-character message stayed in the input box 3 times out of
# 3, at every pre-Enter delay from 0.2 s to 2 s and every paste chunk size from
# 120 to 900 characters, and a second Enter submitted it every time. So the
# submit is confirmed by reading the box back rather than timed, and Enter is
# repeated while our own text is still sitting there. A blind second Enter
# would be unsafe: if the first one worked, it would submit whatever the human
# typed next.
# Quiet period before each Enter press. Measured on a live Codex pane: what
# decides whether a big paste submits is how long the pane has been quiet, not
# the key itself, and pressing Enter again straight away does not help. With
# these gaps a 2.2k message submits; the long tail only costs time when a send
# is actually stuck, since a normal one is confirmed on the first press.
SUBMIT_GAPS = (0.2, 1.0, 3.0, 6.0)
# How many press-and-check rounds the GUARDED (nudge) path may spend before refusing.
# Three is deliberate: the first press often lands while a codex composer is mid-redraw.
GUARDED_PRESS_ATTEMPTS = 3
# A single empty redraw is not submission evidence. Keep the guarded path
# bounded, but require two later reads before accepting an empty composer.
GUARDED_VERIFY_GAPS = (0.2, 0.4)
# Enter is a line feed to some TUIs and does nothing in their composer; the
# carriage return (C-m) is what submits. Measured 2026-10-24 on six revived
# claude lanes: five sat with the text visible for four minutes and the send
# still reported success, while `tmux send-keys C-m` submitted at once. So the
# retries alternate: the first press stays Enter (correct for pi/codex), every
# later one is C-m.
# Two presses of Enter first: a composer that needs two Enters for a long paste
# (Codex) must not be fed a carriage return before it has had its second.
SUBMIT_KEY_AFTER = 2
# How many recent sends per pane to recognise as ours. Remembering only the
# last one meant a single mis-read lost track of our own text for good, and
# every later message queued behind it until a human ran `flush`.
REMEMBERED_SENDS = 4
# How long our own text may sit unsubmitted in a pane's input box while
# messages wait behind it before the daemon presses Enter again to clear it.
# Text the daemon did not type is never submitted this way (see
# `Flock._unwedge`), so this only bounds how long we wait on ourselves, and a
# notification an agent is waiting for should not take longer than this.
STALE_BOX_S = 30.0
# Phrases an agent only shows while a full-screen panel or dialog is open
# (/usage, settings, a confirmation). Such a screen is not an input box: its
# selected row is drawn with the same prompt sigil, which made the daemon read
# a modal as "someone is typing". That cost a Poolrad worker a /usage read on
# 2026-10-12 - the command had run, the panel was open, and the daemon spent
# four Enter presses on the panel before reporting the send failed, while a
# follow-up Escape queued behind the modal for ever.
# Only this many lines from the bottom of the screen are the live interface;
# anything above is transcript.
MODAL_TAIL_LINES = 8

MODAL_MARKERS = (
    "esc to cancel",
    "esc to close",
    "esc to exit",
    "enter to confirm",
    "press enter to continue",
    "to go back",
)

# Every inbox notice starts with this, which is how one is recognised as
# replaceable rather than stacked behind another.
NOTICE_PREFIX = "[inbox] "
# How long to wait before confirming that a draft in the box is really there
# and not a frame of an agent redrawing its pane.
DRAFT_CONFIRM_S = 0.5
# How long a panel may sit open on a pane, unchanged, while something waits to
# be delivered there, before the daemon presses the key the panel itself
# advertises to dismiss it. A panel is not a draft, so this never discards
# anything a person wrote: the markers are the phrases only a dialog shows.
MODAL_WAIT_S = 30.0

# Key names that mean "submit this".
ENTER_KEYS = ("Enter", "enter", "Return", "return", "KP_Enter")
SUBMIT_ATTEMPTS = len(SUBMIT_GAPS)
# A pane whose agent session died still shows an editable composer and still
# accepts typed text; Enter can never submit because there is no server. Pressing
# Enter four times reports "did not submit", which looks like a dunk problem.
# These markers are the pane's own words, checked BEFORE any key is pressed.
DEAD_SESSION_MARKERS = (
    "server connection could not be restored",
    "automatic reconnect could not restore this session",
    "reconnect failed",
    "session expired",
)
# WHICH HARNESSES QUEUE WHAT WE TYPE WHILE THEY WORK. Measured, not assumed:
#   claude -> yes: the composer shows "ctrl+x ctrl+s to send now" and the message is
#             processed after the current turn (proven 2026-10-24, not interleaved).
#   pi     -> yes: "Alt+Up to edit all queued messages", processed after the turn.
#   codex  -> NO: text typed into the composer during a turn is NOT retained. Measured
#             on wR:p6 during a 54-minute turn: four notices reported sent, the
#             composer sat at its placeholder, and the scrollback contained exactly one
#             composer line - i.e. accepted and lost, which is worse than held.
# The policy is "a busy agent is not a hold" - but only where the harness actually
# queues. For a harness that does not, holding is the lesser evil, and the reason says
# so. Anything unknown is treated as not-queueing (conservative).
HARNESS_QUEUES_WHILE_BUSY = {"claude": True, "pi": True}

# A pane can be alive, have unread mail, and still be unreachable: its input box
# holds something that was never submitted, so every later send is held as "a
# human is typing" and the lane silently starves. These are the states that block
# delivery, and each one has a name because "busy" hides them.
# These banners sit at the BOTTOM of a pane: that is where an agent prints its
# connection error, its goal state and its input box. Scanning the whole visible
# screen made a pane look dead because its own transcript quoted the phrase -
# including the pane that had just written the string into a file.
DELIVERY_TAIL_LINES = 12
STATUS_TAIL_LINES = 3

# A pane that is mid-work says so and offers an interrupt key. Absence of a draft
# is NOT evidence of idleness - par-e sat mid-turn for 75 minutes with a clear box.
WORKING_MARKERS = ("esc to interrupt", "esc to cancel", "ctrl+c to interrupt")
PAUSED_GOAL_MARKERS = ("goal paused", "paused (/goal resume)", "/goal resume")
CONTEXT_FULL_MARKERS = ("/clear to save", "context is full", "context limit reached")
# The harness's empty-box HINT, not a state. Claude draws it next to an empty
# composer ("new task? /clear to save 497.4k tokens") and colours it grey, not
# dim, so colour alone cannot tell it from a banner. Reading it as "context full"
# made a healthy lane permanently unreachable while it looked alive.
PLACEHOLDER_PATTERNS = (
    re.compile(r"new task\?\s*/clear to save [\d.,]+[km]? tokens?", re.I),
    re.compile(r"^\s*(new task\?|type your (request|message)\b)", re.I),
    # Each harness's own empty-box hint. Usually drawn dim (so the bright-text
    # reader already ignores it), but a bright redraw of the same words is not a
    # draft either.
    re.compile(r"^\s*ask (codex|claude|open ?code) to do anything", re.I),
    # codex's other composer hints, measured on pane %17 (2026-10-24): 'tab' is
    # drawn bold and the rest plain, so the reader must match after stripping ANSI.
    re.compile(r"^\s*tab to (queue message|send|interrupt|search)\b", re.I),
    re.compile(r"^\s*(send a message|type a message|ask anything)\b", re.I),
)
# How long a "busy" report from another process may block delivery while the pane
# itself shows no turn and a clear box. A remembered state needs an expiry.
CLAUDE_BUSY_TTL_S = float(os.environ.get("DUNKINGSHEEP_CLAUDE_BUSY_TTL_S", "600"))
# The typing mutex: short by design, loud when contended.
TYPING_LOCK_TIMEOUT_S = float(os.environ.get("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S", "10"))
TYPING_LOCK_LOUD_S = float(os.environ.get("DUNKINGSHEEP_TYPING_LOCK_LOUD_S", "0.25"))
# A send that could not start because the box is not ours to type into is HELD, not
# failed: a dialog someone is reading and a human mid-sentence are the same answer -
# wait, do not report a failure.
# A send that could not start because the box would not clear is DEFERRED, not a claim
# that a person is present (that state does not exist on this path any more).
BOX_HOLD = "the box would not clear (text left untouched)"
SUBMIT_CONFIRM_S = 2.0
# Most submits land at once, so look soon and back off rather than paying a
# fixed delay on every send.
SUBMIT_POLL_S = (0.1, 0.2, 0.4, 0.8, 1.0)
# A box can read clear for an instant while an agent redraws. Declaring the
# send done on that one reading stranded a message in the VP pane on
# 2026-10-11 and blocked everything queued behind it. Require two later,
# readable checks; an absent input box during redraw cannot prove delivery.
SUBMIT_VERIFY_GAPS = (0.4, 1.0)

TEMPLATE_RE = re.compile(r"\{(id|name|count|target|interval|time|date)\}")
# CSI escape sequences (colours, dim, cursor moves) as `herdr pane read
# --format ansi` emits them.
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
# Characters agents draw around the input box that are never typed input:
# braille animation dots, box-drawing rules and block glyphs, whitespace.
DECORATION_RE = re.compile(r"[─-▟⠀-⣿\s]+")

log = logging.getLogger("dunkingsheep")


class _NoticeFolded(Exception):
    """Internal: a notice was merged into one already waiting."""


# Non-agent senders that may appear on inbox mail (see Flock.inbox_send).
SYSTEM_SENDERS = ("watchdog",)


class DunkError(ValueError):
    """A caller mistake (bad id, bad interval, ambiguous target). Safe to show."""


class AmbiguousTarget(DunkError):
    """A target matched more than one pane. Distinct from "no such pane",
    because a mailbox may be a name with no pane behind it, but it must never
    be a name that stands for several panes at once."""


def _as_dunk_error(method):
    """An unknown message id is a caller mistake like any other, so it should
    come back as a plain message rather than as an internal error."""
    @functools.wraps(method)
    def wrapper(*args, **kwargs):
        try:
            return method(*args, **kwargs)
        except (InboxError, TaskError) as error:
            raise DunkError(str(error)) from None
    return wrapper


def _truthy(value, default=True):
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("1", "true", "yes", "y", "on")


def format_interval(minutes):
    """Human form of an interval: '10', '1.5', or '30s' for under a minute.
    Every form round-trips through parse_interval."""
    minutes = float(minutes)
    if 0 < minutes < 1:
        return f"{minutes * 60:g}s"
    if minutes == int(minutes):
        return str(int(minutes))
    return f"{minutes:g}"


def parse_interval(value):
    """Accept minutes as a number or a string like '60', '1.5h', '90s', '2m'."""
    if isinstance(value, bool):
        raise DunkError("interval must be a number of minutes")
    if isinstance(value, (int, float)):
        minutes = float(value)
    else:
        text = str(value).strip().lower()
        match = re.fullmatch(r"([0-9]*\.?[0-9]+)\s*(s|sec|secs|m|min|mins|h|hr|hrs|)", text)
        if not match:
            raise DunkError(f"bad interval {value!r} (try 10, 90s, 1.5h)")
        number, unit = float(match.group(1)), match.group(2)
        if unit.startswith("s"):
            minutes = number / 60.0
        elif unit.startswith("h"):
            minutes = number * 60.0
        else:
            minutes = number
    if not minutes > 0:
        raise DunkError("interval must be positive")
    return minutes


def expand_template(text, dunker):
    """Expand the known `{placeholders}` in `text`; leave other braces alone."""
    now = time.localtime()
    values = {
        "id": dunker.id,
        "name": dunker.name or dunker.id,
        "count": str(dunker.send_count + 1),
        "target": dunker.target_label(),
        "interval": format_interval(dunker.interval_minutes),
        "time": time.strftime("%H:%M:%S", now),
        "date": time.strftime("%Y-%m-%d", now),
    }
    return TEMPLATE_RE.sub(lambda m: values[m.group(1)], text)


class Dunker:
    """One dunk. Plain data plus a little presentation logic."""

    FIELDS = (
        "id", "name", "target_pane_id", "target_agent", "target_tab",
        "target_workspace", "target_cwd", "interval_minutes", "text",
        "running", "only_when", "max_sends", "send_count", "created_at",
        "started_at", "last_sent_at", "next_send_at", "last_error",
        "skip_if_unconsumed", "skip_count", "total_skips", "last_skipped_at",
        "awaiting_consumption", "last_sent_text", "unknown_skips",
        "hold_while_typing", "hold_count", "last_held_at",
    )

    def __init__(self, id, **kwargs):
        self.id = id
        self.name = kwargs.get("name") or ""
        self.target_pane_id = kwargs.get("target_pane_id")
        self.target_agent = kwargs.get("target_agent")
        self.target_tab = kwargs.get("target_tab")
        self.target_workspace = kwargs.get("target_workspace")
        self.target_cwd = kwargs.get("target_cwd")
        self.interval_minutes = float(
            kwargs.get("interval_minutes") or DEFAULT_INTERVAL_MINUTES
        )
        self.text = kwargs.get("text", DEFAULT_TEXT)
        if self.text is None:
            self.text = DEFAULT_TEXT
        self.running = bool(kwargs.get("running", False))
        self.only_when = kwargs.get("only_when") or None
        self.max_sends = kwargs.get("max_sends") or None
        self.send_count = int(kwargs.get("send_count") or 0)
        self.created_at = kwargs.get("created_at") or time.time()
        self.started_at = kwargs.get("started_at")
        self.last_sent_at = kwargs.get("last_sent_at")
        self.next_send_at = kwargs.get("next_send_at")
        self.last_error = kwargs.get("last_error")
        # Backpressure (all persisted; old state files simply lack them).
        self.skip_if_unconsumed = bool(kwargs.get("skip_if_unconsumed", False))
        # skip_count is the CURRENT run's skips and resets when the dunk sends;
        # total_skips is the lifetime number, which is history and must never be
        # the thing a threshold fires on.
        self.skip_count = int(kwargs.get("skip_count") or 0)
        self.total_skips = int(kwargs.get("total_skips") or self.skip_count)
        self.last_skipped_at = kwargs.get("last_skipped_at")
        # True from a successful send until the target is seen consuming it.
        self.awaiting_consumption = bool(kwargs.get("awaiting_consumption", False))
        self.last_sent_text = kwargs.get("last_sent_text")
        # Consecutive inconclusive box reads; capped so a parser that does not
        # recognise a harness cannot skip for ever.
        self.unknown_skips = int(kwargs.get("unknown_skips") or 0)
        # Never type over a human (persisted; old state files default to on).
        self.hold_while_typing = bool(kwargs.get("hold_while_typing", True))
        self.hold_count = int(kwargs.get("hold_count") or 0)
        self.last_held_at = kwargs.get("last_held_at")
        # Volatile (not persisted).
        self.status = kwargs.get("status", "Ready")
        self.detector_fallback = False
        self.last_skip_reason = None
        self.stop_event = None
        self.thread = None

    # -- presentation -------------------------------------------------------

    def target_label(self):
        if not self.target_pane_id:
            return "(no target)"
        tab = self.target_tab or self.target_pane_id
        if self.target_agent:
            return f"{tab} / {self.target_agent}"
        return tab

    def set_target(self, pane):
        """Point at a herdr pane dict (as returned by HerdrClient.list_panes_grouped)."""
        self.target_pane_id = pane.get("pane_id")
        self.target_agent = pane.get("agent")
        self.target_cwd = pane.get("cwd")
        self.target_tab = pane.get("tab_label") or pane.get("tab_id")
        self.target_workspace = pane.get("workspace_label") or pane.get("workspace_id")

    def to_dict(self):
        data = {field: getattr(self, field) for field in self.FIELDS}
        data["status"] = self.status
        data["target_label"] = self.target_label()
        if self.running and self.next_send_at:
            data["next_send_in_s"] = max(0, int(round(self.next_send_at - time.time())))
        else:
            data["next_send_in_s"] = None
        return data

    @classmethod
    def from_dict(cls, data):
        data = dict(data)
        dunk_id = data.pop("id")
        data.pop("status", None)
        data.pop("target_label", None)
        data.pop("next_send_in_s", None)
        return cls(dunk_id, **data)


class Message:
    """One direct message waiting in (or delivered from) a pane's outbox."""

    def __init__(self, id, pane_id, target_label, text, source, deliver):
        self.id = id
        self.pane_id = pane_id
        self.target_label = target_label
        self.text = text
        self.source = source          # "send_text" or the dunk id for a fired dunk
        self.deliver = deliver        # callable -> (ok, detail)
        self.status = "queued"        # queued | sent | failed | cancelled
        self.hold_while_typing = True
        self.kind = "text"            # "keys" presses are how a panel is dismissed
        self.is_notice = False        # an inbox notice: one per pane, never stacked
        self.notify_for = []          # inbox message ids whose notice this is
        self.detail = None
        self.holds = 0
        self.created_at = time.time()
        self.finished_at = None
        self.done = threading.Event()

    def _queued_message(self):
        if self.holds:
            return f"queued: the box in {self.target_label} would not clear"
        return f"queued for {self.target_label}, delivering once the input box is clear"

    # The four outcomes a caller can act on, as one machine-readable field. Prose
    # is for people; a caller that parses sentences gets it wrong (cos, 2026-10-24:
    # "sent=false" substring matching, then a delivered message read as a 409).
    # ok          submitted and confirmed (box cleared, or a turn started)
    # stored      accepted into a durable queue the LANE drains itself (owner_push);
    #             it has not been read, which is a different fact from "sent"
    # queued      held behind a human typing / not yet delivered by the typing path
    # unconfirmed neither witness fired: may have arrived, do not resend blindly
    # refused     a named refusal, nothing was written or typed
    OUTCOMES = ("ok", "stored", "queued", "unconfirmed", "refused")

    def outcome(self):
        if self.status == "queued":
            return "queued"
        if self.status == "sent":
            return "ok"
        detail = (self.detail or "").lower()
        if "unconfirmed" in detail or "did not submit" in detail:
            # Neither witness fired: it may well have arrived, so this is NOT a
            # refusal and must not invite a blind resend.
            return "unconfirmed"
        return "refused"

    def to_dict(self):
        queued = self.status == "queued"
        return {
            "message_id": self.id, "pane_id": self.pane_id, "target_label": self.target_label,
            "text": self.text, "source": self.source, "status": self.status,
            "queued": queued, "ok": None if queued else self.status == "sent",
            "outcome": self.outcome(),
            "message": (self._queued_message() if queued else self.detail or self.status),
            "holds": self.holds, "created_at": self.created_at, "finished_at": self.finished_at,
            "waiting_s": int(time.time() - self.created_at) if queued else None,
        }


class Flock:
    """Owns every dunker and its timer thread. Thread-safe."""

    def __init__(self, herdr=None, state_file=STATE_FILE,
                 launches_file=LAUNCHES_FILE, persist=True,
                 restore=True, inbox=None, metrics=None, tasks=None,
                 superusers=None, memory=None):
        self.herdr = get_backend(herdr)
        self.state_file = state_file
        self.launches_file = launches_file
        # pane_id -> {agent, session, session_kind, cwd, argv, command, source}
        # Exact launch memory for post-upgrade recovery.
        self.launches = {}
        self.persist_enabled = persist
        self._load_launches()
        self._lock = threading.RLock()
        # Per-request caller identity (pane id + stable agent pid/start). The
        # server sets it from SO_PEERCRED; direct callers leave it empty.
        self._caller_state = threading.local()
        # Claude session status comes from `claude agents --json`, cached.
        self.claude_status = ClaudeStatus()
        # self pane -> (mailbox key, resolved_at); bounded by MAILBOX_CACHE_S.
        self._own_mailbox_cache = {}
        self._view_cache = {"at": 0.0, "pane": None, "data": None}
        # pane_id -> {started_at, expires_at, ttl_s, source, caller}
        self.turns = {}
        # Turn-end dispatch bookkeeping, both to stop a nudge loop: pane_id -> when, and
        # (pane_id, task_id) -> when that task was last prompted on a turn end.
        self.turn_end_nudges = {}
        self.turn_end_task_nudges = {}
        self.turn_end_dispatching = set()
        # One lock per pane, not one for the whole flock. A send holds its
        # lock across the confirm-and-retry sequence, which can run for tens of
        # seconds when an agent will not take Enter. With a single lock that
        # stalled every other pane's send behind it, and a caller waiting on
        # the daemon saw a timeout: that is what a Poolrad /usage read hit on
        # 2026-10-12. Panes are independent; their sends should be too.
        self.send_lock = threading.Lock()          # kept: tests and callers hold it
        self._pane_locks = {}
        self.dunkers = []
        self.next_id = 1
        self.started_at = time.time()
        self.last_message = ""
        self.closed = False
        # Direct-message outbox: pane_id -> [Message], plus the per-pane
        # delivery thread and a bounded history of finished messages.
        self.outbox = {}
        # Ids whose outcome is already decided, carried ACROSS restarts (t249). message_history is
        # per-process, so a restore that consulted only it would always see an empty set on the
        # path it exists to protect - the verdict has to be persisted, or read from the inbox.
        self.decided = {}
        self.outbox_threads = {}
        self._claude_busy_since = {}
        self.started_at = time.time()
        # ONE lock for anything that types anywhere (owner, 2026-10-23). Every typing
        # surface we own goes through this daemon, so a process-wide lock is the whole
        # truth; a human typing in a terminal directly is what the pre-send box read
        # is for. Reentrant because the send path nests (guarded send -> submit).
        self.typing_lock = threading.RLock()
        self._typing_owner = None          # (pane_id, since) of the current holder
        self.typing_contentions = 0
        # The last text we typed into each pane, so text stranded by a failed
        # submit is recognised as ours and flushed instead of being mistaken
        # for a human's draft (which would stall that pane's queue for good).
        self.sent_text_by_pane = {}
        self._delivered_text = {}      # pane_id -> recent texts WE delivered
        # pane_id -> the key that last submitted there (Enter for most, C-m for
        # TUIs whose composer ignores Enter: measured on claude, 2026-10-24).
        self._submit_key = {}
        self._warned_wedged = {}
        self._warned_modal = {}
        # Why the most recent refusal happened, per pane, so a held event can
        # say which of draft / dialog / stranded it was.
        self._last_block_reason = {}
        self.inbox = Inbox() if inbox is None else inbox
        self.tasks = Tasks() if tasks is None else tasks
        self.superusers = Superusers() if superusers is None else superusers
        self._owns_memory = memory is None and self.persist_enabled
        self.memory = (memory if memory is not None
                       else (MemoryStore() if self.persist_enabled else None))
        self.memory_ingestor = MemoryIngestor(self.memory) if self.memory else None
        # Panes that asked the daemon to feed them work when they go idle.
        # pane_id -> scope (global or that pane's private queue).
        self.task_waiters = {}
        self._task_stop = threading.Event()
        self._next_job_wake = threading.Event()
        self._task_threads = []
        self.quarantined_mailboxes = self._quarantine_shared_mailboxes()
        if metrics is not None:
            self.metrics = metrics
        elif self.persist_enabled:
            # Telemetry lives beside the state file, so a flock persisting into
            # a temporary directory keeps its events there too. Hard-coding the
            # real path meant any in-process test that persisted appended to the
            # live store, and tidying that up is not a test's business.
            self.metrics = Metrics(path=os.path.join(
                os.path.dirname(os.path.abspath(self.state_file)), "metrics.db"))
        else:
            self.metrics = Metrics(path=":memory:")
        self.metrics.prune()
        self.message_history = []
        self.next_message_id = 1
        from dunk_next_job import NextJobs
        self.next_jobs = NextJobs(self)
        if restore:
            self._restore()
        if self.persist_enabled:
            self._start_task_workers()

    # -- persistence --------------------------------------------------------

    def _restore(self):
        if not self.persist_enabled or not os.path.exists(self.state_file):
            return
        try:
            with open(self.state_file, "r", encoding="utf-8") as handle:
                payload = json.load(handle)
        except (OSError, ValueError) as error:
            log.warning("could not read %s: %s", self.state_file, error)
            return
        with self._lock:
            self.next_id = int(payload.get("next_id") or 1)
            for data in payload.get("dunkers", []):
                try:
                    dunker = Dunker.from_dict(data)
                except Exception as error:  # noqa: BLE001 - skip bad rows
                    log.warning("skipping corrupt dunk %r: %s", data, error)
                    continue
                dunker.status = "Restored"
                self.dunkers.append(dunker)
                self.next_id = max(self.next_id, _numeric_id(dunker.id) + 1)
            self.next_message_id = int(payload.get("next_message_id") or self.next_message_id)
            for key, value in (payload.get("decided") or {}).items():
                if isinstance(value, dict):
                    self.decided[str(key)] = {"status": str(value.get("status") or "unknown"),
                                              "created_at": value.get("created_at")}
                else:                       # a file written by the first version of this change
                    self.decided[str(key)] = {"status": str(value), "created_at": None}
            self._restore_outbox(payload.get("outbox") or [])
            for dunker in list(self.dunkers):
                if dunker.running:
                    now = time.time()
                    if not dunker.next_send_at or dunker.next_send_at < now:
                        dunker.next_send_at = now + RESTORE_GRACE_S
                    self._spawn_timer(dunker)
        log.info("restored %d dunk(s) from %s", len(self.dunkers), self.state_file)

    def _save(self):
        self._invalidate_view()
        if not self.persist_enabled:
            return
        with self._lock:
            payload = {
                "version": 1,
                "next_id": self.next_id,
                "next_message_id": self.next_message_id,
                "dunkers": [
                    {field: getattr(d, field) for field in Dunker.FIELDS}
                    for d in self.dunkers
                ],
                # Held mail must survive a restart. Losing a queued notice is worse
                # than delaying it: on 2026-10-23 a restart to clear a stuck hold
                # silently discarded every held message and three lanes were never
                # told anything.
                "outbox": [
                    {"id": message.id, "pane_id": message.pane_id,
                     "target_label": message.target_label, "text": message.text,
                     "source": message.source, "kind": message.kind,
                     "hold_while_typing": message.hold_while_typing,
                     "is_notice": message.is_notice,
                     "notify_for": list(message.notify_for),
                     "created_at": message.created_at}
                    for queue in self.outbox.values() for message in queue
                    if message.status == "queued"],
                # The verdicts, so the NEXT process can tell a stale snapshot row from a held one.
                # Bounded: an id older than the last DECIDED_KEPT outcomes cannot be in a file that
                # only ever holds queued rows.
                "decided": dict(list(self.decided.items())[-DECIDED_KEPT:]),
            }
        # SERIALISE SNAPSHOT AND REPLACEMENT (from %35's #45, adopted here): two concurrent saves can
        # otherwise interleave so that an OLDER snapshot wins, undoing a delivery or a cancellation
        # that happened in between - the same "state only this process believes" defect one layer
        # down. The lock is an RLock and _save() is called from paths that already hold it, so this is
        # safe from re-entry.
        try:
            os.makedirs(os.path.dirname(self.state_file), exist_ok=True)
            with self._lock:
                tmp = self.state_file + ".tmp"
                with open(tmp, "w", encoding="utf-8") as handle:
                    json.dump(payload, handle, indent=2)
                os.replace(tmp, self.state_file)
        except OSError as error:
            log.warning("could not save %s: %s", self.state_file, error)

    # -- launch memory (post-upgrade recovery) --------------------------------

    def _load_launches(self):
        if not self.persist_enabled or not os.path.exists(self.launches_file):
            return
        try:
            with open(self.launches_file, "r", encoding="utf-8") as handle:
                payload = json.load(handle)
        except (OSError, ValueError) as error:
            log.warning("could not read %s: %s", self.launches_file, error)
            return
        panes = payload.get("panes") if isinstance(payload, dict) else None
        if isinstance(panes, dict):
            self.launches = {pane_id: dict(entry) for pane_id, entry in panes.items()
                             if isinstance(entry, dict)}

    def _restore_outbox(self, rows):
        """Rebuild held messages after a restart and log what came back.

        A notice is re-typed from its stored text, which is why the text is the
        thing to persist: a restore that resends a *different* string would be a
        new message wearing an old id.
        """
        # A ROW THE FILE STILL HOLDS MAY ALREADY HAVE BEEN DELIVERED (t249): the save used to
        # happen only on enqueue, so a message that finished after the last save stayed in the
        # file. Anything already in message_history has had its outcome decided; restoring it
        # would type it again. The file is a snapshot, the history is the verdict.
        already_decided = {m.id for m in self.message_history
                           if getattr(m, "status", None) in ("sent", "failed", "cancelled")}
        # THE PERSISTED VERDICTS (t249): message_history is empty in a fresh process, so this is
        # what actually protects a restart. The inbox record is consulted too - the notice path
        # writes its outcome there, so a row whose inbox notices were all delivered is decided
        # even if this daemon never saw the delivery.
        def verdict_for(row):
            """The persisted verdict for THIS row, or None when the id was reused.

            A verdict is about a message, so it needs the message's identity: same id AND same
            created_at. A mismatch means the id was reused by a later message (or the file was
            paired with a counter that went backwards), and refusing it would drop a new message
            while logging a clean restore. Unverifiable (either side missing) is also a mismatch:
            restoring is recoverable, silently dropping is not.
            """
            entry = self.decided.get(row.get("id"))
            if not entry:
                return None
            status = entry.get("status") if isinstance(entry, dict) else entry
            if status not in ("sent", "failed", "cancelled"):
                return None
            stamped = entry.get("created_at") if isinstance(entry, dict) else None
            row_stamp = row.get("created_at")
            if stamped is None or row_stamp is None:
                log.warning("message %s has a %s verdict but no timestamp to match it against; "
                            "restoring it rather than dropping it", row.get("id"), status)
                return None
            try:
                if abs(float(stamped) - float(row_stamp)) > 1e-3:
                    log.warning("message %s was reused (verdict %s for created_at %s, this row is "
                                "%s); restoring it", row.get("id"), status, stamped, row_stamp)
                    return None
            except (TypeError, ValueError):
                return None
            return status

        already_decided |= {row.get("id") for row in rows if verdict_for(row)}
        decided_reason = {}
        for row in rows:
            notify_ids = [str(x) for x in (row.get("notify_for") or [])]
            if not notify_ids:
                continue
            try:
                records = [self.inbox.status(inbox_id) for inbox_id in notify_ids]
            except Exception:                      # noqa: BLE001 - a read must not lose mail
                continue
            states = [(rec.get("notify") or {}).get("state") for rec in records]
            mail_states = [rec.get("state") for rec in records]
            # "cancelled" belongs here too (reviewer, PR #43): it is a real message status
            # (dunk_core.py:3917), and a notice cancelled in the inbox must not be typed later.
            #
            # "failed" IS NOT THE SAME AS "SENT" (reviewer, PR #43): in the delivery loop it means
            # "this attempt failed, retrying", so a row whose notices all say failed may still be the
            # ONLY nudge for mail nobody has read. Refusing to restore it would end a retry loop by
            # silently dropping the message - the failure mode this whole change exists to prevent,
            # arriving from the other side. So a failed notice only decides the row when the mail it
            # announced is no longer waiting: every one of those inbox messages has left the queue
            # (read, archived or cancelled). Otherwise the row is RESTORED and the reason logged.
            if states and all(state in ("sent", "cancelled") for state in states):
                already_decided.add(row.get("id"))
                # Keep WHY, not just THAT (reviewer, PR #43): the skip's log used to fall through to
                # the word "decided", which is grammatical and uninformative. The reason is computed
                # here, so it travels from here.
                decided_reason[row.get("id")] = (
                    "every notice is %s, so the nudge arrived"
                    % "/".join(sorted({str(x) for x in states})))
            elif states and all(state in ("sent", "failed", "cancelled") for state in states):
                if mail_states and all(state != "queued" for state in mail_states):
                    already_decided.add(row.get("id"))
                    decided_reason[row.get("id")] = (
                        "every notice is %s and its mail is no longer waiting (%s)"
                        % ("/".join(sorted({str(x) for x in states})),
                           "/".join(sorted({str(x) for x in mail_states}))))
                    log.info("message %s is decided: every notice is %s and its mail is no longer "
                             "waiting (states %s)", row.get("id"),
                             "/".join(sorted({str(x) for x in states})),
                             "/".join(sorted({str(x) for x in mail_states})))
                else:
                    log.warning("message %s has %d failed notice(s) but its mail is STILL UNREAD "
                                "(states %s); restoring it rather than dropping the only nudge",
                                row.get("id"), len(notify_ids),
                                "/".join(sorted({str(x) for x in mail_states})))
        restored = []
        for row in rows:
            try:
                if row.get("id") in already_decided:
                    # The reason comes from the verdict we ALREADY have. Reading it out of
                    # message_history raised StopIteration on a real restart (the history is empty
                    # there), the per-row except caught it, and the log said "could not restore a
                    # held message:" with an EMPTY reason - a refusal reported as a failure, which
                    # is how a correct skip sends the next reader hunting a bug (reviewer, PR #43).
                    verdict = (decided_reason.get(row.get("id"))
                               or verdict_for(row)
                               or next((m.status for m in self.message_history
                                        if m.id == row.get("id")), "decided"))
                    # The template carries the reason as a CLAUSE, not as a status word: the old
                    # "it already %s before the restart" fitted "decided" and produced "it already
                    # every notice is sent, so the nudge arrived before the restart" (reviewer, PR
                    # #43). The sentence has to read as prose, because it is the only account a
                    # vanished row gets.
                    log.info("not restoring message %s: %s (decided before the restart)",
                             row.get("id"), verdict)
                    continue
                pane_id = row["pane_id"]
                text = row.get("text") or ""
                kind = row.get("kind") or "text"
                if kind == "keys":
                    keys = text.split()
                    deliver = (lambda p=pane_id, k=keys: self.press_keys(p, k))
                else:
                    deliver = (lambda p=pane_id, t=text: self.submit_text(p, t))
                message = self._new_message(pane_id, row.get("target_label") or pane_id,
                                            text, row.get("source") or "restored", deliver)
                message.id = row.get("id") or message.id
                message.kind = kind
                message.hold_while_typing = bool(row.get("hold_while_typing", True))
                message.is_notice = bool(row.get("is_notice"))
                message.notify_for = list(row.get("notify_for") or [])
                message.created_at = float(row.get("created_at") or time.time())
                self._enqueue(message)
                restored.append(message)
            except Exception as error:      # noqa: BLE001 - one bad row must not lose the rest
                log.warning("could not restore a held message: %s", error)
        if restored:
            log.warning("resumed %d held message(s) after restart: %s",
                        len(restored), ", ".join(m.id for m in restored))
        return restored

    def _save_launches(self):
        if not self.persist_enabled:
            return
        with self._lock:
            payload = {"version": 1, "panes": self.launches}
        try:
            os.makedirs(os.path.dirname(self.launches_file), exist_ok=True)
            temporary = self.launches_file + ".tmp"
            with open(temporary, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, indent=2, sort_keys=True)
            os.chmod(temporary, 0o600)
            os.replace(temporary, self.launches_file)
        except OSError as error:
            log.warning("could not write %s: %s", self.launches_file, error)

    def _record_launch(self, pane_id, **fields):
        if not pane_id:
            return
        with self._lock:
            entry = self.launches.setdefault(pane_id, {})
            for key, value in fields.items():
                if value is not None:
                    entry[key] = value
            entry["recorded_at"] = time.time()
        self._save_launches()

    def _snapshot_launches(self, panes):
        """Keep launches.json current from herdr's agent_session metadata."""
        if not self.persist_enabled:
            return
        changed = False
        with self._lock:
            for pane in panes or []:
                session = pane.get("agent_session") or {}
                agent = pane.get("agent") or session.get("agent")
                value = session.get("value")
                if not agent or not value:
                    continue
                entry = self.launches.setdefault(pane.get("pane_id"), {})
                fields = {
                    "agent": agent,
                    "session": value,
                    "session_kind": session.get("kind"),
                    "cwd": pane.get("cwd") or pane.get("foreground_cwd"),
                    "terminal_title": pane.get("terminal_title_stripped"),
                }
                for key, field in fields.items():
                    if field is not None and entry.get(key) != field:
                        entry[key] = field
                        changed = True
                if changed and entry.get("source") is None:
                    entry["source"] = "herdr"
        if changed:
            self._save_launches()

    # -- lookup ---------------------------------------------------------------

    def _find(self, dunk_id):
        dunk_id = str(dunk_id).strip()
        for dunker in self.dunkers:
            if dunker.id == dunk_id:
                return dunker
        # Be forgiving: accept a bare number or a row number.
        if dunk_id.isdigit():
            for dunker in self.dunkers:
                if dunker.id == f"d{dunk_id}":
                    return dunker
        raise DunkError(f"no dunk with id {dunk_id!r}")

    def get(self, dunk_id):
        with self._lock:
            return self._find(dunk_id).to_dict()

    def list(self):
        with self._lock:
            return [d.to_dict() for d in self.dunkers]

    def status(self, check_herdr=True):
        with self._lock:
            running = sum(1 for d in self.dunkers if d.running)
            total = len(self.dunkers)
        # The sidebar poll runs often; asking the terminal backend for
        # availability spawns a subprocess. `check_herdr=False` keeps that out
        # of the hot path and leaves the field None (not "down").
        available = self.herdr.is_available() if check_herdr else None
        return {
            "version": VERSION,
            **code_status(),
            "started_at": self.started_at,
            "pid": os.getpid(),
            "dunkers": total,
            "running": running,
            "herdr_available": available,
            "herdr_hint": (None if available is not False
                           else self.herdr.server_error_hint()),
            "state_file": self.state_file if self.persist_enabled else None,
            "uptime_s": int(time.time() - self.started_at),
            "queued_messages": sum(len(q) for q in self.outbox.values()),
            "inbox_unread": sum(m["unread"] for m in self.inbox.stats()),
            "tasks": self.tasks.stats()["totals"],
            "task_followers": len(self.task_waiters),
            "quarantined_mailboxes": self.quarantined_mailboxes or None,
            "message": self.last_message,
        }

    def poll_status(self):
        """The cheap half of status: counts only, no global inbox/task scans.

        The sidebar poll must not pay for `inbox.stats()` and `tasks.stats()`
        (they scan the whole stores); those are for the human `status` command.
        """
        with self._lock:
            running = sum(1 for d in self.dunkers if d.running)
            total = len(self.dunkers)
            queued = sum(len(queue) for queue in self.outbox.values())
            message = self.last_message
        return {
            "version": VERSION,
            **code_status(),
            "started_at": self.started_at,
            "pid": os.getpid(),
            "dunkers": total,
            "running": running,
            "queued_messages": queued,
            "message": message,
            "herdr_available": None,
            "herdr_hint": None,
            "uptime_s": int(time.time() - self.started_at),
        }

    def summary(self):
        with self._lock:
            n = len(self.dunkers)
            running = sum(1 for d in self.dunkers if d.running)
        label = f"{n} dunker{'s' if n != 1 else ''}"
        return f"{label} ({running} running)" if running else label

    # -- targets ----------------------------------------------------------------

    def list_panes(self, self_pane_id=None):
        panes = self.herdr.list_panes_grouped()
        for pane in panes:
            pane["is_self"] = bool(self_pane_id) and pane.get("pane_id") == self_pane_id
            activity = self.lane_activity(pane)
            pane["agent_status"] = activity["state"]
            pane["agent_status_source"] = activity["source"]
            pane["agent_status_reason"] = activity["reason"]
            pane["agent_status_inferred"] = activity["inferred"]
        return panes

    # The age bound lives with the rule (dunk_activity.IDLE_CLAIM_S); this alias exists so tests
    # and callers can read it off the flock.
    IDLE_CLAIM_S = dunk_activity.IDLE_CLAIM_S

    def lane_status(self, pane):
        return self.lane_activity(pane)["state"]

    def lane_activity(self, pane):
        """What the lane is DOING, from the best evidence this backend has.

        The tmux backend has no agent-status API, so it reports "unknown" for every pane and
        the ops board showed 'unknown' for all of them (owner t227). But we do have a turn
        signal: the harness calls turn start/end, and turn_end deliberately keeps the ended
        state because that is the only evidence of IDLENESS we get. So:

            a declared, unexpired turn -> "working"
            an ENDED turn             -> "idle"
            no turn signal at all     -> None  (the desk renders an em dash, not "unknown")

        None, not "unknown": a backend that cannot answer should say nothing rather than
        answer wrongly, and the difference between "we do not know" and "the lane is unknown"
        is exactly what the board was missing.
        """
        backend = (pane.get("agent_status") or "").strip().lower()
        if backend and backend not in ("unknown", ""):
            return {"state": backend, "source": "terminal backend",
                    "reason": None, "inferred": False}
        pane_id = pane.get("pane_id")
        if not pane_id:
            return {"state": None, "source": "none",
                    "reason": "no activity source: pane identity missing", "inferred": True}
        with self._lock:
            state = dict(self.turns.get(pane_id) or {})
        # ONE derivation (reviewer on t227): dunk_activity.turn_state owns the rule, so the desk
        # (which reads activity()) and this (which feeds the CLI and the dispatcher) cannot
        # disagree about what the same turn means.
        harness = pane.get("agent") or "lane"
        derived, _since, reason = dunk_activity.turn_state(
            state, now=time.time(), idle_claim_s=self.IDLE_CLAIM_S, harness=harness)
        return {"state": derived, "source": f"{harness} declared turns",
                "reason": reason, "inferred": True}

    # -- inboxes ------------------------------------------------------------
    # Machine-to-machine messaging that never types into a terminal. See
    # dunk_inbox.py for why, and for the storage.

    def _quarantine_shared_mailboxes(self):
        """Move mail that an older resolver filed under an agent *type* out of
        reach. Those mailboxes were shared by every pane of that type, so the
        messages in them may have been readable by the wrong agent and we
        cannot tell who they were meant for. They are renamed so no pane
        resolves to them, and reported, rather than delivered to a guess."""
        try:
            types = {str(p.get("agent")).lower() for p in self.herdr.list_panes()
                     if p.get("agent")}
        except Exception:  # noqa: BLE001 - herdr may be down; nothing to do yet
            return []
        moved = []
        for mailbox in types:
            try:
                count = self.inbox.rename_mailbox(mailbox, f"unrouted:{mailbox}")
            except InboxError:
                continue
            if count:
                moved.append({"was": mailbox, "now": f"unrouted:{mailbox}",
                              "messages": count})
                log.warning("inbox: %d message(s) were filed under the shared mailbox "
                            "%r by an older version and may have been visible to the "
                            "wrong agent; moved to 'unrouted:%s' for review",
                            count, mailbox, mailbox)
        return moved

    def pm_digest(self, project, payload, to=None, *, window="30 min", self_pane_id=None):
        """Publish ONE digest per project under a stable id.

        The row is updated in place (unread again after an update), so the VP
        reads a single message per project rather than a stream. Returns the
        row plus whether it was created or updated.
        """
        import dunk_pm
        did = dunk_pm.digest_id(project)
        if isinstance(payload, str):
            # The socket carries the payload as a string; render it, do not
            # store it verbatim. JSON object -> structured digest; text -> note.
            text = payload.strip()
            if text.startswith("{"):
                try:
                    payload = json.loads(text)
                except ValueError:
                    payload = {"note": text}
            else:
                payload = {"note": text}
        demo = bool(payload.get("demo")) if isinstance(payload, dict) else False
        if isinstance(payload, dict):
            # Numbers are read at publish time from the checkpoint, never from
            # the caller's template: a stale digest number is worse than none.
            live = dunk_pm.checkpoint_numbers(project)
            numbers = live if live is not None else payload.get("numbers", ())
            body = dunk_pm.render_digest(project, window=window,
                                         changed=payload.get("changed", ()),
                                         blocked=payload.get("blocked", ()),
                                         owner_decisions=payload.get("owner_decisions", ()),
                                         numbers=numbers,
                                         note=payload.get("note"))
        else:
            body = str(payload)
        vp_pane = to or os.environ.get("DUNKINGSHEEP_VP_PANE", dunk_pm.DEFAULT_VP_PANE)
        mailbox, label, pane_id = self.resolve_mailbox(vp_pane, self_pane_id)
        existing = None
        try:
            with self.inbox._lock:
                existing = self.inbox.db.execute(
                    "SELECT 1 FROM messages WHERE digest_id=?", (did,)).fetchone()
        except Exception:  # noqa: BLE001
            existing = None
        subject = f"PM digest (DEMO): {project}" if demo else f"PM digest: {project}"
        message = self.inbox.upsert_digest(mailbox, did, body,
                                           subject=subject,
                                           sender=self_pane_id,
                                           mailbox_label=label, pane_id=pane_id)
        message["digest_id"] = did
        message["action"] = "updated" if existing else "created"
        message["body"] = body
        try:
            if pane_id:
                unread = self.inbox.count(mailbox)["unread"]
                text = _notice_text(mailbox, unread, self_pane_id,
                                    f"PM digest: {project}")
                self.send_now(pane_id, text, wait_s=0)
                message["notified"] = True
        except Exception:  # noqa: BLE001 - the digest is stored even if the nudge fails
            message["notified"] = False
        return message

    def resolve_mailbox(self, spec, self_pane_id=None, strict=False):
        """Turn a target into (mailbox key, label, pane id).

        `strict` is the SEND-time contract (VP ruling 2026-10-24): a target must
        resolve to a live pane or to a lane the registry knows, and a registered
        lane marked dead is refused with its date. Reads stay permissive on
        purpose - inspecting a stranded mailbox is how the backlog gets drained.

        The key has to identify one pane and keep identifying it. The pane's
        own label comes first, since that is a name someone chose; then the tab
        label; then the pane id, which is always unique but changes if the pane
        moves. Each candidate is taken only if exactly one pane carries it.

        The agent *type* is never a key. Keying on it was a bug: every Codex
        pane shared one mailbox, so a message meant for the Poolrad PM was
        readable from the website and worker panes too.

        A spec matching no pane is still a valid mailbox name, so mail can be
        left for an agent that has not started. A spec matching several panes
        is refused rather than pooled."""
        spec = str(spec or "").strip()
        if not spec:
            raise DunkError("a target mailbox is required")
        panes = self.herdr.list_panes_grouped()
        # 1. An exact pane id (or terminal id) is unambiguous and always wins.
        for pane in panes:
            if pane.get("pane_id") == spec or pane.get("terminal_id") == spec:
                return self._mailbox_for_pane(pane)
        if _PANE_ID_RE.match(spec) or _TERM_ID_RE.match(spec) or _HERDR_ID_RE.match(spec):
            if not strict:
                # READS STAY PERMISSIVE. A dead pane id is a name mail can still be
                # filed under - that is how 78 stranded messages exist, and they
                # have to be readable and archivable or the triage cannot run.
                # Only a SEND refuses here (measured regression, 2026-10-24).
                return spec, spec, None
            raise DunkError(
                f"no live pane {spec!r}; a dead pane id is not a mailbox, and a "
                "pane id is not a name. If that lane still exists, address it by "
                "mailbox name; if it does not, the send fails here instead of "
                "queueing into nothing. reason=dead_lane.")
        # 1b. A DECORATED address - 'name (wR:p5)' - carries its own pane id, and
        # that id decides. Without this the whole string fell through to step 3 and
        # became a mailbox NAME with no pane: mail addressed to
        # 'codex-poolrad2 (wR:p5)' was then answered by wR:p4, whose conversation
        # name was the same one. A name is a conversation; a pane id is a process.
        embedded = _EMBEDDED_PANE_RE.search(spec)
        if embedded:
            hinted = embedded.group(1)
            for pane in panes:
                if pane.get("pane_id") == hinted or pane.get("terminal_id") == hinted:
                    return self._mailbox_for_pane(pane)
            if not strict:
                # A decorated address is a mailbox NAME too ('PM (par-a) (wW:p1)'
                # is exactly what the triage list shows). Reading it must work.
                return spec, spec, None
            raise DunkError(
                f"target {spec!r} names pane {hinted}, which is not live. A decorated "
                f"address must name a live pane: use a current pane id, or a bare "
                f"mailbox name without the parenthetical. reason=dead_lane.")

        # 1c. The LANE REGISTRY, read backwards. A mailbox recorded against a
        # live pane wins over any name: labels and titles are chosen by the agent
        # and change (claude rewrites its title every turn), while the registry
        # row was written by the daemon when the pane was made.
        recorded, recorded_name = self._lane_pane(spec)
        if recorded is not None:
            # The key is the name the daemon recorded, not the pane's current
            # title: a title the agent overwrote is what lost the lane.
            return (str(recorded_name), f"{recorded_name} ({recorded['pane_id']})",
                    recorded["pane_id"])

        # 2. An exact *name* must identify exactly one pane across every name
        #    field. Two same-type panes can share a label (or one pane's label
        #    can equal another's tab label); picking by field priority delivered
        #    mail to the wrong lane on 2026-10-19. Refuse with the candidates.
        if spec.lower() not in ("self", "me", "here"):
            needle = spec.lower()
            exact = [p for p in panes
                     if any(str(p.get(key) or "").lower() == needle
                            for key in ("label", "tab_label", "workspace_label",
                                        "agent"))]
            if len(exact) > 1:
                raise AmbiguousTarget(
                    f"target mailbox {spec!r} is ambiguous: {_describe(exact)}")
            if len(exact) == 1:
                return self._mailbox_for_pane(exact[0])
        try:
            pane = self.resolve_target(spec, self_pane_id)
        except AmbiguousTarget:
            raise
        except DunkError:
            if spec.lower() in ("self", "me", "here"):
                raise
            if self._is_agent_type(spec):
                raise DunkError(
                    f"{spec!r} is an agent type, not one agent: it would be a mailbox "
                    "several panes share. Use a pane id, a pane or tab label, or a "
                    "name of your own.")
            # FAIL CLOSED (VP ruling 2026-10-24, owner #1556). Creating a mailbox as
            # a fallback is what let 'w9:pH', 'w9:pM' and 'nav-fixer2' each become a
            # brand new mailbox that swallowed the message and reported success. A
            # target either resolves to a live pane or names a lane that the registry
            # knows; anything else is refused here, with the reason, and the caller
            # sees it.
            if not strict:
                return spec, spec, None
            registered, alive, dead_since = self._registered_lane(spec)
            if registered and not alive:
                when = ""
                if dead_since:
                    when = f" (dead since {dead_since})"
                raise DunkError(
                    f"lane {spec!r} is registered as DEAD{when}: nothing can be "
                    "delivered to it. reason=dead_lane. Restore the lane first, or "
                    "send to a lane that is live.")
            if not registered:
                raise DunkError(
                    f"target {spec!r} resolves to no live pane and to no registered "
                    "lane; refusing rather than creating a mailbox that would queue in "
                    "silence. reason=unknown_name. Use a live pane id, a pane/tab "
                    "label, or a name recorded in the lane registry.")
            # Registered, and no pane on the backend we deliver through. That is a
            # lane between restarts OR a lane alive on the other backend, and in both
            # cases a message put in the queue is a message nobody can read - which is
            # the failure the VP outlawed, only slower. Refuse and say which it is.
            raise DunkError(
                f"lane {spec!r} is registered but has no live pane on this backend; "
                "nothing can be delivered to it right now. reason=unhoused_lane. "
                "Existing mail for it can still be read and archived; start or restore "
                "the lane, or send to a lane that is live.")
        return self._mailbox_for_pane(pane)

    def _is_agent_type(self, spec):
        """True if `spec` names a kind of agent (claude, codex) rather than
        one of them, so it must never become a mailbox."""
        low = str(spec).strip().lower()
        return any(str(p.get("agent") or "").lower() == low
                   for p in self.herdr.list_panes())

    def _mailbox_for_pane(self, pane):
        panes = self.herdr.list_panes_grouped()

        def unique(field):
            value = pane.get(field)
            if not value:
                return None
            matches = [p for p in panes if p.get(field) == value]
            return value if len(matches) == 1 else None

        # A DECLARED identity wins over every name the pane carries. The title is
        # written by the agent (claude sets it to its session summary) and the
        # label is a display string; neither may decide who the lane is.
        declared_name, declared = self._row_identity(
            self._lane_registry().get(pane["pane_id"]))
        if declared and declared_name:
            return (declared_name, f"{declared_name} ({pane['pane_id']})",
                    pane["pane_id"])
        key = unique("label") or unique("tab_label") or pane["pane_id"]
        label = pane.get("label") or pane.get("tab_label") or pane["pane_id"]
        if label != pane["pane_id"]:
            label = f"{label} ({pane['pane_id']})"
        return str(key), label, pane["pane_id"]

    def view(self, self_pane_id=None, fresh=False):
        """Cheap view for the poll-shaped commands: status + dunks + inbox count.

        Derived from poll_status, the in-memory dunk list and Inbox.count. A
        mutation (add/update/remove/start/stop, inbox send/ack/read, task
        change) bumps `_view_cache["at"]=0`, so the next read rebuilds; the TTL
        bounds staleness for external writers. `fresh=True` rebuilds now.
        """
        now = time.time()
        cached = self._view_cache
        if (not fresh and cached["data"] is not None
                and cached["pane"] == self_pane_id
                and now - cached["at"] < VIEW_CACHE_S):
            return cached["data"]
        status = self.poll_status()
        status["self_pane_id"] = self_pane_id
        data = {"status": status, "dunks": self.list()}
        if self_pane_id:
            try:
                data["inbox_count"] = self.inbox_count(None,
                                                       self_pane_id=self_pane_id)
            except Exception as error:  # noqa: BLE001 - a view never breaks
                data["inbox_error"] = str(error)
        self._view_cache = {"at": now, "pane": self_pane_id, "data": data}
        return data

    def _invalidate_view(self):
        self._view_cache["at"] = 0.0
        self._view_cache["data"] = None

    def _own_mailbox(self, self_pane_id):
        """The caller's mailbox key, briefly cached.

        Resolving it walks the pane list; on a busy herdr that is 2-4 s per
        poll, while the inbox query itself is sub-millisecond. Only the
        self-default is cached; an explicit target still resolves fresh.
        """
        now = time.time()
        cached = self._own_mailbox_cache.get(self_pane_id)
        if cached and now - cached[1] < MAILBOX_CACHE_S:
            return cached[0]
        resolved = False
        try:
            key, _label, _pane = self.resolve_mailbox("self", self_pane_id)
            resolved = True
        except DunkError:
            # A LANE'S MAILBOX MUST NOT DEPEND ON THE BACKEND'S PANE IDS. After the
            # tmux flip every herdr pane id became unknown to the daemon, and every
            # command that resolved "self" failed - including reading one's own mail.
            # The inbox already supports a mailbox named after the pane id, so that is
            # the identity of last resort, and it is explicit rather than guessed.
            key = (os.environ.get("DUNKINGSHEEP_MAILBOX") or "").strip() or None
            if not key and self_pane_id:
                key = self._lane_registry().get(self_pane_id)
            if not key and self_pane_id and self._mailbox_exists(self_pane_id):
                key = self_pane_id
            if not key:
                raise DunkError(
                    f"cannot resolve this lane's mailbox: {self_pane_id!r} is not a pane "
                    f"on the current backend ({self.herdr.name if hasattr(self.herdr, 'name') else 'unknown'}) "
                    f"and no mailbox is named after it; set DUNKINGSHEEP_MAILBOX to the "
                    f"lane's mailbox name")
        self._own_mailbox_cache[self_pane_id] = (key, now)
        # Only a REAL resolution is worth remembering: recording a fallback would
        # poison the registry with 'pane id -> pane id' and lose the lane's name.
        if self_pane_id and resolved and self._plain_lane_name(key):
            self._remember_lane(self_pane_id, key)
        return key

    # -- lane registry: an identity that survives a backend change -------------
    def lane_registry_path(self):
        return os.path.join(CONFIG_DIR, "lanes.json")

    def _lane_registry(self):
        try:
            with open(self.lane_registry_path(), encoding="utf-8") as handle:
                data = json.load(handle)
            return data if isinstance(data, dict) else {}
        except (OSError, ValueError):
            return {}

    # A NAME A TITLE MAY BECOME. Auto-declaration reads whatever a pane carries, and a
    # tmux title is written by anything that talks to the pane - including an inbox
    # nudge: pane %28 was auto-declared as 'OC | OpenCode inbox message from %15'
    # (VP, t115 side effect). A title is only allowed to name a lane when it looks
    # like a name: one token, no spaces, no pipes, no notice text.
    _PLAIN_LANE_NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:-]*$")
    _NOT_A_NAME_MARKERS = ("[inbox]", "read it with", "|", "\u2502", "\u203a")

    @classmethod
    def _plain_lane_name(cls, name):
        """True when this string may become a lane's identity BY INFERENCE.

        Strict on purpose: a title is written by anything that talks to the pane, so
        inference only accepts one token (or a pane id). Declared names are a separate
        question - 'dunky pi' and 'PM (par-a)' are declared, never inferred.
        """
        text = str(name or "").strip()
        if not text or len(text) > 64:
            return False
        lowered = text.lower()
        if any(marker in lowered for marker in cls._NOT_A_NAME_MARKERS):
            return False
        if _PANE_ID_RE.match(text) or _HERDR_ID_RE.match(text):
            return True
        return bool(cls._PLAIN_LANE_NAME.match(text))

    @classmethod
    def _usable_declared_name(cls, name):
        """True when a DECLARED row holds something a lane could be called.

        Looser than inference - a declared name may contain a space or parentheses -
        but a row carrying notice text or a pipe is corruption, not a name.
        """
        text = str(name or "").strip()
        if not text or len(text) > 64:
            return False
        lowered = text.lower()
        return not any(marker in lowered for marker in cls._NOT_A_NAME_MARKERS)

    @staticmethod
    def _row_identity(value):
        """(mailbox, declared) for a registry row, however it is spelled.

        A row is either a plain mailbox name (older rows, and every inferred one)
        or a dict that can carry `declared` plus liveness. Two questions only:
        who is this lane, and did somebody SAY that or did we guess it.
        """
        if isinstance(value, dict):
            name = str(value.get("mailbox") or value.get("name") or "").strip()
            declared = bool(value.get("declared"))
            if declared and not Flock._usable_declared_name(name):
                # A DECLARED ROW THAT IS NOT A NAME is corruption, not an identity:
                # fail closed and say so, rather than hand a lane a mailbox called
                # 'OC | OpenCode inbox message from %15'.
                log.warning("lane registry row is not a usable name (%r); ignoring it. "
                            "Repair with: dunkingsheep lane set <pane> <name>", name)
                return None, False
            return (name or None), declared
        name = str(value or "").strip()
        return (name or None), False

    # Which harnesses drain a push queue of their own. A pi lane runs the
    # dunkingsheep extension, which polls owner_take about once a second; claude
    # and codex lanes have no such hook, so a push to them is a queue nobody
    # reads. That is a CAPABILITY, not a preference: the desk routes on it.
    DRAIN_HOOKS = {"pi": "owner_push"}
    # A drain observed this recently is evidence a poller is running now. Older
    # than this and the lane is treated as not draining, because silence is the
    # only safe reading of a stale take.
    DRAIN_FRESH_S = 300

    def delivery_capability(self, target, self_pane_id=None):
        """How the desk may deliver to this target, and what it may show.

        Returns, for a live pane: pane_id, mailbox (pinned name if declared),
        agent, alive, drains (does its harness poll a push queue), send
        ('owner_push' or 'guarded'), and the latency expectation. A target that
        does not resolve raises the same tokenised refusals as a send, so the UI
        renders one vocabulary for every way a delivery can fail.
        """
        pane = self.delivery_target(target, self_pane_id)
        pane_id = pane["pane_id"]
        agent = (pane.get("agent") or "").strip().lower()
        name, declared = self._row_identity(self._lane_registry().get(pane_id))
        drain = self.DRAIN_HOOKS.get(agent)
        # OBSERVED, not inferred: only a take proves a drainer is running. A pi
        # lane whose session predates the extension still polls nothing, and the
        # desk would offer a route that silently waits (measured on %2).
        evidence, last = "no poll observed", None
        import dunk_owner
        store = dunk_owner.shared()
        try:
            # The POLL heartbeat, not the last take: a manual take must not confer
            # capability, and an idle poller draining empty queues must not lose it.
            last = store.last_poll(pane_id)
            if last:
                age = int(time.time() - last)
                evidence = (f"harness polled {age}s ago" if age < 86400
                            else "harness last polled over a day ago")
            elif drain:
                evidence = ("harness has a poller but has never polled this pane "
                            "(a manual owner take does not count)")
        except Exception:                       # noqa: BLE001 - capability never crashes
            evidence = "drain evidence unavailable"
        # No close here: the store is shared by the daemon (dunk_owner.shared()),
        # and closing it per probe is what produced the handle churn in the first
        # place. One connection for the process, not one per capability query.
        observed = bool(last and time.time() - last < self.DRAIN_FRESH_S)
        return {
            "pane_id": pane_id,
            "mailbox": name or (pane.get("label") or pane_id),
            "declared": declared,
            "agent": agent or None,
            "alive": True,
            "drains": observed,
            "drain_evidence": evidence,
            # push only when a drain has ACTUALLY been seen recently; otherwise the
            # immediate guarded send, because an unread queue is the failure mode
            # this whole day was about.
            "send": ("owner_push" if observed else "guarded"),
            # The promise follows the OBSERVED route, not the harness's name: a pi
            # lane whose poller is not running is a guarded send like any other.
            "expect": ("push drained by the lane's own poller, no inbox row"
                       if observed else
                       "immediate guarded send (Enter, then C-m); 'sent' means it "
                       "left the box"),
            # WHETHER THE DESK CAN ACTUALLY TYPE HERE. A token existing is not the
            # same as this pane being on its list: the credential names panes, so a
            # view that only knows "a token is configured" offers a button that the
            # daemon will refuse (found reviewing the desk's can_type, 2026-10-24).
            **self._desk_send_rights(pane_id),
        }

    def _desk_send_rights(self, pane_id):
        """Does the desk credential exist, and does it cover THIS pane?"""
        try:
            import dunk_desk
            desk = dunk_desk.DeskCredential()
            panes = set(desk.panes())
            return {"desk_credential": desk.has_token(),
                    "desk_can_type": bool(desk.has_token() and pane_id in panes)}
        except Exception:                        # noqa: BLE001 - capability never crashes
            return {"desk_credential": False, "desk_can_type": False}

    def delivery_target(self, target, self_pane_id=None):
        """Resolve a DIRECT-DELIVERY target (owner push, keystrokes) to one pane.

        Same fail-closed rules as the inbox send path, because it is the same
        question asked by a different caller: who is this and are they alive. The
        refusal carries a token the UI can render - dead_lane, unhoused_lane,
        unknown_name - instead of a generic "no pane". Addressing is the pane id
        (%9) or the lane's PINNED name ("PM (par-a)"); the pinned name is what
        survives a title change, so prefer it in anything a human reads.
        """
        spec = str(target or "").strip()
        if not spec:
            raise DunkError("a delivery target is required (a pane id like %9 "
                            "or a pinned lane name)")
        if _PANE_ID_RE.match(spec) or _HERDR_ID_RE.match(spec) or _TERM_ID_RE.match(spec):
            pane = self.herdr.get_pane(spec)
            if pane is not None:
                return pane
            raise DunkError(
                f"no live pane {spec!r}: this terminal is gone. reason=dead_lane.")
        pane = self.herdr.get_pane(spec)          # a bare '%4'
        if pane is not None:
            return pane
        key, _label, pane_id = self.resolve_mailbox(spec, self_pane_id, strict=True)
        if not pane_id:
            raise DunkError(
                f"lane {key!r} resolved but has no live pane to deliver to. "
                "reason=unhoused_lane.")
        return self.herdr.get_pane(pane_id)

    def lanes(self):
        """The lane registry as rows: pane id, mailbox, declared?, alive?"""
        rows = []
        for pane_id, value in sorted(self._lane_registry().items()):
            name, declared = self._row_identity(value)
            alive = bool(value.get("alive", True)) if isinstance(value, dict) else None
            rows.append({"pane_id": pane_id, "mailbox": name,
                         "declared": declared, "alive": alive,
                         "dead_since": (value.get("dead_since")
                                        if isinstance(value, dict) else None)})
        return rows

    def lane_set(self, pane_id, mailbox, self_pane_id=None):
        """Declare who a pane IS, after the fact. Superuser-gated by the caller.

        The repair path for a lane whose declared name was overwritten by a title
        (or never declared at all). Refuses a pane that does not exist, so a
        typo cannot mint a lane.
        """
        pane_id = str(pane_id or "").strip()
        mailbox = str(mailbox or "").strip()
        if not pane_id or not mailbox:
            raise DunkError("lane set must be given a pane id and a mailbox name")
        if self.herdr.get_pane(pane_id) is None:
            raise DunkError(f"no live pane {pane_id!r}: a lane row for a dead pane "
                            "would be a guess. Create the pane first.")
        self._remember_lane(pane_id, mailbox, declared=True)
        self._invalidate_view()
        name, _ = self._row_identity(self._lane_registry().get(pane_id))
        return {"pane_id": pane_id, "mailbox": name, "declared": True}

    def _remember_lane(self, pane_id, mailbox, declared=False):
        """Record pane id -> mailbox, so a backend flip cannot orphan a lane's mail.

        A DECLARED identity (new-workspace --as, lane set) is immutable from the
        inside: an agent renames its pane title every turn, and those names are
        inferences, so they may fill an empty row and must never overwrite a
        declared one. Measured 2026-10-24: `--as 'PM (par-a)'` was replaced by
        claude's own conversation title minutes later, and the lane became
        unaddressable by the name it had been given.
        """
        data = self._lane_registry()
        existing = data.get(pane_id)
        existing_name, existing_declared = self._row_identity(existing)
        if existing_declared and not declared:
            return False
        if not declared and existing_name:
            return False
        if declared:
            row = dict(existing) if isinstance(existing, dict) else {}
            row["mailbox"] = str(mailbox).strip()
            row["declared"] = True
            row.setdefault("declared_at", time.time())
            row.setdefault("alive", True)
            data[pane_id] = row
        else:
            if existing_name == mailbox:
                return False
            data[pane_id] = str(mailbox).strip()
        self._write_lane_registry(data)
        return True

    def _write_lane_registry(self, data):
        """Atomic replace of the lane registry. One writer, one rename."""
        try:
            os.makedirs(CONFIG_DIR, exist_ok=True)
            temporary = self.lane_registry_path() + ".tmp"
            with open(temporary, "w", encoding="utf-8") as handle:
                json.dump(data, handle, indent=2, sort_keys=True)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.lane_registry_path())
            return True
        except OSError as error:
            log.warning("could not write the lane registry: %s", error)
            return False

    def _title_pane(self, pane_id, mailbox):
        """Best effort: put the lane name in the pane title too.

        A title is a courtesy (the agent may replace it); the registry is the
        record. Never let a failed rename undo a successful one.
        """
        rename = getattr(self.herdr, "rename", None)
        if not callable(rename):
            return False
        try:
            ok, _ = rename("pane", pane_id, str(mailbox))
            return bool(ok)
        except Exception:                        # noqa: BLE001 - cosmetic only
            return False

    def _lane_pane(self, mailbox):
        """(live pane, recorded mailbox) for a mailbox in the lane registry, else (None, None).

        The registry is pane -> mailbox, written on every genuine resolution and
        on every create/restore that knows the lane. Reading it backwards is what
        makes a mailbox addressable after a backend change, when the pane id the
        message was written to no longer exists.
        """
        needle = str(mailbox or "").strip().lower()
        if not needle:
            return None, None
        recorded = [pane_id for pane_id, row in self._lane_registry().items()
                    if (self._row_identity(row)[0] or "").lower() == needle]
        if not recorded:
            return None, None
        live = {pane.get("pane_id"): pane
                for pane in self.herdr.list_panes_grouped()}
        candidates = [(live[pane_id], self._row_identity(self._lane_registry()[pane_id])[0])
                      for pane_id in recorded if pane_id in live]
        if len(candidates) == 1:
            return candidates[0]
        if len(candidates) > 1:
            # Two live panes declaring one name is not a tie to break: it is a lane
            # duplicated by a restore, and choosing one would silently split its
            # mail. Refuse, and say both ids.
            ids = ", ".join(pane["pane_id"] for pane, _ in candidates)
            raise AmbiguousTarget(
                f"lane {mailbox!r} is DECLARED by more than one live pane ({ids}); "
                "delivery would pick one and split the lane's mail. Reset the "
                "duplicate with `lane set` before sending.")
        return None, None              # none live

    def _registered_lane(self, name):
        """(found, alive, dead_since) for a mailbox name in the lane registry.

        The registry is the only place that can say "this lane exists and is not
        live" (a lane between restarts, or one that died in a flip). A string
        that is in NO registry row is not a lane at all: mail to it must fail at
        send time, not create a mailbox and queue into nothing.

        Rows are read tolerantly because the registry is being extended with
        liveness: a row may be a plain mailbox name (pane -> name) or a dict
        carrying name/alive/dead_since.
        """
        needle = str(name or "").strip().lower()
        if not needle:
            return False, True, None
        for value in self._lane_registry().values():
            name, _declared = self._row_identity(value)
            if (name or "").lower() != needle:
                continue
            if isinstance(value, dict):
                return True, bool(value.get("alive", True)), value.get("dead_since")
            return True, True, None        # a plain name row says nothing about life
        return False, True, None

    def _mailbox_exists(self, name):
        """True when the inbox already holds mail addressed to this name."""
        where, params = __import__("dunk_inbox").mailbox_where(name)
        try:
            with self.inbox._lock:
                row = self.inbox.db.execute(
                    f"SELECT COUNT(*) FROM messages WHERE {where}", params).fetchone()
            return bool(row and row[0])
        except Exception:                       # noqa: BLE001 - a fallback must not raise
            return False

    def _mailbox_or_self(self, spec, self_pane_id, strict=False):
        if spec:
            return self.resolve_mailbox(spec, self_pane_id, strict=strict)[0]
        if not self_pane_id:
            raise DunkError("no mailbox given and no caller pane to default to")
        return self._own_mailbox(self_pane_id)

    @_as_dunk_error
    def inbox_send(self, to, body, subject=None, attach=None, notify=True,
                   self_pane_id=None, system_sender=None):
        """Put a message in another agent's inbox. Nothing is typed into their
        terminal except, when `notify`, a short line telling them they have
        mail - which is the one send short enough to always submit.

        `system_sender` names a non-agent sender (the watchdog). It is accepted
        only from SYSTEM_SENDERS and only from a caller with no pane (a systemd
        unit); an agent always runs in a pane, so it cannot claim the name."""
        if system_sender is not None:
            if system_sender not in SYSTEM_SENDERS:
                raise DunkError(f"unknown system sender {system_sender!r}")
            if self_pane_id:
                raise DunkError("a system sender cannot be claimed from a pane")
        undeliverable_reason = None
        try:
            mailbox, label, pane_id = self.resolve_mailbox(to, self_pane_id, strict=True)
        except DunkError as refusal:
            # A SYSTEM SENDER MUST NEVER BE KILLED BY A FAIL-CLOSED REFUSAL. The
            # watchdog escalates to a lane that may have been handed over minutes
            # ago; raising here took the exception watchdog down twice on 2026-10-24
            # and the fleet ran blind for nine minutes. The message is STORED under
            # the name it was addressed to, marked undeliverable with the refusal as
            # its reason. Agents still get the hard refusal: a person typing into a
            # dead lane should be stopped, a watchdog should not be.
            if not system_sender:
                raise
            mailbox = label = str(to).strip()
            pane_id = None
            undeliverable_reason = str(refusal)
            log.warning("system sender %s: %r refused (%s); stored as undeliverable",
                        system_sender, to, refusal)
        # The sender is the raw pane id, never a mailbox label. On 2026-10-19
        # wS:p1's mail arrived "from codex poolrad" because the label resolved
        # through a sibling pane; ids are unique and stable.
        sender = system_sender or self_pane_id or None
        # PM routing: a lane that messages the VP out of habit is redirected
        # to its project PM (or flagged). The VP reads PM digests, not lane
        # reports; this is the "routing is wrong" count's other half.
        redirected = None
        try:
            import dunk_pm
            routing = dunk_pm.load_routing()
            project = None
            for name, entry in routing.items():
                if pane_id and entry["vp_pane"] == pane_id and self_pane_id in entry["members"]:
                    project = name
                    break
            if project:
                pm_pane, _, mode = dunk_pm.redirect(self_pane_id, routing)
                if pm_pane:
                    if mode == "redirect":
                        mailbox, label, pane_id = self.resolve_mailbox(pm_pane, self_pane_id)
                        body = f"[redirected from the VP to the {project} PM]\n\n{body}"
                        redirected = {"from": "vp", "to": pm_pane, "project": project}
                    else:
                        subject = f"[lane mail to VP: {project}] {subject or ''}".strip()
                        redirected = {"flag": project}
        except Exception:  # noqa: BLE001 - routing must never block a send
            redirected = None
        attachments = [attach] if isinstance(attach, str) else list(attach or ())
        message = self.inbox.send(mailbox, body, subject=subject, sender=sender,
                                  mailbox_label=label, pane_id=pane_id,
                                  attachments=attachments)
        if redirected:
            message["redirected"] = redirected
        message["notified"] = False
        if undeliverable_reason:
            message["outcome"] = "undeliverable"
            message["reason"] = undeliverable_reason
        if notify and pane_id:
            unread = self.inbox.count(mailbox)["unread"]
            text = _notice_text(mailbox, unread, sender, subject)
            try:
                existing = self._refresh_queued_notice(pane_id, text)
                if existing is not None:
                    # A notice for this pane is already waiting. Say the new
                    # count rather than stacking a second one: an agent that
                    # reads its inbox once has handled both messages. This
                    # message's notify state follows that notice from here.
                    with self._lock:
                        existing.notify_for.append(message["message_id"])
                    self.inbox.record_notify(message["message_id"], "queued",
                                             f"folded into {existing.id}")
                    message["notified"] = True
                    raise _NoticeFolded
                nudge = self.send_now(pane_id, text, wait_s=0,
                                      notify_for=message["message_id"])
                self.inbox.record_notify(message["message_id"],
                                         "queued" if nudge.get("queued") else "sent",
                                         nudge.get("message"))
                message["notified"] = True
            except _NoticeFolded:
                pass
            except DunkError as error:
                self.inbox.record_notify(message["message_id"], "failed", str(error))
        self.metrics.record("inbox.sent", pane_id=pane_id, mailbox=mailbox,
                            sender=sender, source="inbox", outcome="ok",
                            bytes=len(str(body or "").encode("utf-8")),
                            attempts=len(message.get("attachments") or []) or None)
        with self._lock:
            self.last_message = f"Inbox message to {label}"
        log.info("inbox %s -> %s (%s)", message["message_id"], mailbox,
                 "notified" if message["notified"] else "no nudge")
        return message

    @_as_dunk_error
    @_as_dunk_error
    def inbox_archive(self, mailbox, before=None, states=None, self_pane_id=None):
        """Bulk-retire a mailbox: mark messages read and archived, never delete.
        ``before`` is a unix timestamp; ``states`` defaults to queued+acked."""
        return self.inbox.archive(mailbox, before=before, states=states)

    def _notice_for(self, mailbox, unread, sender, subject):
        return _notice_text(mailbox, unread, sender, subject)

    def _refresh_queued_notice(self, pane_id, text):
        """If an inbox notice for this pane is still waiting, update it to the
        new unread count and return it. Otherwise None.

        Stacking one notice per message means an agent that was away comes
        back to a pile of identical lines, each of which it has to submit.
        One notice carrying the current count is the whole of the information.
        """
        with self._lock:
            for queued in self.outbox.get(pane_id) or []:
                if queued.status == "queued" and queued.is_notice:
                    queued.text = text
                    queued.deliver = lambda: self.submit_text(pane_id, queued.text)
                    return queued
        return None

    def inbox_count(self, mailbox=None, self_pane_id=None):
        key = self._mailbox_or_self(mailbox, self_pane_id)
        lane_panes = []
        try:
            import dunk_pm
            for entry in dunk_pm.load_routing().values():
                lane_panes.extend(entry["members"])
        except Exception:  # noqa: BLE001 - a bad routing file must not break counts
            lane_panes = []
        return self.inbox.count(key, lane_panes=sorted(set(lane_panes)))

    @_as_dunk_error
    def inbox_list(self, mailbox=None, unread_only=True, limit=50, self_pane_id=None):
        key = self._mailbox_or_self(mailbox, self_pane_id)
        return {"mailbox": key,
                "messages": self.inbox.list(key, unread_only=unread_only, limit=limit)}

    @_as_dunk_error
    def inbox_read(self, mailbox=None, limit=10, peek=False, self_pane_id=None):
        """Read a batch WITHOUT consuming it, build the whole result, then commit.

        The order matters and it is the whole point of this method: marking read first
        meant a reader whose rendering or task lookup failed had already consumed the
        batch, and the messages were gone on the next read - pi-review-1 issued a
        verdict on a head whose withdrawal message it never saw (VP, t93). So:

          1. fetch with mark=False,
          2. build the FULL result, including the caller's task queues (which is where
             the failure actually happened: the row_factory drift crashed for_pane),
          3. only then mark those ids read.
        A failure in 1 or 2 leaves the batch unread, which is the safe direction: the
        reader sees the same mail again rather than losing it.
        """
        key = self._mailbox_or_self(mailbox, self_pane_id)
        messages = self.inbox.read(key, limit=limit, peek=True)
        result = {"mailbox": key, "messages": messages}
        if self_pane_id:
            # The owner's directive: mail and the reader's task queues travel
            # together, so one inbox read is the whole handoff. This runs BEFORE the
            # mark for the reason above - it is the step that used to crash.
            result["tasks"] = self.tasks.for_pane(self_pane_id)
        if peek or not messages:
            return result
        marked = self.inbox.mark_read([message["message_id"] for message in messages])
        now = time.time()
        for message in messages:
            message["state"] = "read"
            message["read_at"] = now
            self.metrics.record(
                "inbox.read", pane_id=message.get("pane_id"), mailbox=key,
                sender=message.get("sender"), source="inbox", outcome="ok",
                duration_ms=int((now - message["created_at"]) * 1000),
                bytes=message.get("body_bytes"))
        result["marked_read"] = marked
        return result

    @_as_dunk_error
    def inbox_status(self, message_id):
        return self.inbox.status(message_id)

    @_as_dunk_error
    def inbox_ack(self, message_ids, mailbox=None, self_pane_id=None):
        """Ack only messages in the caller's own mailbox. Sender is irrelevant;
        an id from another mailbox is refused, never marked read there. The
        internal no-mailbox call (tests, in-process) keeps the old behaviour."""
        key = self._mailbox_or_self(mailbox, self_pane_id) if (mailbox or self_pane_id) else None
        before = {}
        for message_id in ([message_ids] if isinstance(message_ids, str)
                           else list(message_ids)):
            try:
                before[message_id] = self.inbox.status(message_id)
            except InboxError:
                pass
        result = self.inbox.ack(message_ids, mailbox=key)
        result["acked_in"] = key
        for message_id in result.get("changed", []):
            was = before.get(message_id) or {}
            started = was.get("read_at") or was.get("created_at")
            self.metrics.record("inbox.acked", pane_id=was.get("pane_id"), mailbox=key,
                                sender=was.get("sender"), source="inbox", outcome="ok",
                                duration_ms=int((self.inbox.get(message_id)["read_at"] - started) * 1000)
                                if started else None)
        return result

    @_as_dunk_error
    def inbox_cancel(self, message_ids):
        return self.inbox.cancel(message_ids)

    @_as_dunk_error
    def inbox_delete(self, message_ids):
        return self.inbox.delete(message_ids)

    @_as_dunk_error
    def inbox_stats(self):
        return {"mailboxes": self.inbox.stats()}

    @_as_dunk_error
    def inbox_fetch(self, attachment_id, dest=None):
        return self.inbox.fetch(attachment_id, dest or os.getcwd())

    # -- global and per-pane tasks ------------------------------------------

    def _task_scope(self, scope, self_pane_id, default=TASK_GLOBAL):
        """Resolve a caller's scope and enforce per-pane privacy.

        Global is fleet-wide. A pane id is private to that pane; an agent may
        only name its own pane (or 'self'). A caller with no pane, such as the
        human's CLI outside herdr, may name any pane for inspection.
        """
        if scope is None or str(scope).strip() in ("", "all"):
            return default
        value = str(scope).strip()
        if value == "self":
            if not self_pane_id:
                raise DunkError("scope 'self' requires a known caller pane")
            return self_pane_id
        try:
            value = validate_task_scope(value)
        except TaskError as error:
            raise DunkError(str(error)) from None
        if self_pane_id and value != TASK_GLOBAL and value != self_pane_id:
            raise DunkError(
                f"per-pane tasks are private to {value}; caller runs in {self_pane_id}")
        return value

    def _task_assignees(self, assignees, self_pane_id):
        """Expand 'self'/'me' to the caller's pane id."""
        if not assignees:
            return []
        values = [assignees] if isinstance(assignees, str) else list(assignees)
        resolved = []
        for value in values:
            value = str(value).strip()
            if not value:
                continue
            if value in ("self", "me"):
                if not self_pane_id:
                    raise DunkError("self-assign requires a known caller pane")
                resolved.append(self_pane_id)
            else:
                resolved.append(value)
        return resolved

    def _require_task_visible(self, task, self_pane_id):
        if not self_pane_id:
            return task
        scope = task.get("scope")
        if scope != TASK_GLOBAL and scope != self_pane_id:
            raise DunkError(
                f"task {task['id']} belongs to another pane's private queue ({scope})")
        return task

    @_as_dunk_error
    def task_add(self, title, scope=None, body=None, status=None, priority=None,
                 assignees=None, parent_id=None, depends_on=None,
                 multi_assignee=False, self_pane_id=None):
        target = self._task_scope(scope, self_pane_id, default=TASK_GLOBAL)
        # t278: a caller with NO pane (human CLI, a unit) could silently create a
        # task in another pane's PRIVATE queue - the task showed in `task list`, but
        # `task read`/`complete` from anywhere else answered "belongs to another
        # pane's private queue". A pane caller is already refused by _task_scope
        # (per-pane queues are private), so the hole is anonymous creation only.
        # Refuse it, naming the workaround, instead of silently landing work where
        # nobody can see who put it there.
        if target != TASK_GLOBAL and not self_pane_id:
            raise DunkError(
                f"refusing to create a task in {target}'s private queue from a "
                "caller with no pane id: per-pane queues belong to that pane. "
                "Pass --scope global to create a fleet-visible task (add --assign "
                "to assign a lane), or run the add from that pane itself.")
        return self.tasks.add(
            target, title, body=body or "", status=status or "open",
            priority=priority or "medium",
            assignees=self._task_assignees(assignees, self_pane_id),
            parent_id=parent_id, depends_on=depends_on,
            created_by=self_pane_id, multi_assignee=_truthy(multi_assignee, False))

    @_as_dunk_error
    def task_list(self, scope=None, status=None, assignee=None, include_done=True,
                  limit=200, self_pane_id=None):
        explicit = None
        if scope is not None and str(scope).strip() not in ("", "all"):
            explicit = self._task_scope(scope, self_pane_id, default=TASK_GLOBAL)
        tasks = self.tasks.list(
            scope=explicit, status=status, assignee=assignee,
            include_done=_truthy(include_done, True), limit=int(limit or 200),
            self_pane_id=self_pane_id)
        return {"scope": explicit or "visible", "pane_id": self_pane_id,
                "tasks": tasks}

    @_as_dunk_error
    def task_read(self, task_id, self_pane_id=None):
        return self._require_task_visible(
            self.tasks.get(task_id, pane_id=self_pane_id), self_pane_id)

    @_as_dunk_error
    def task_update(self, task_id, title=None, body=None, status=None,
                    priority=None, parent_id=..., assignees=None,
                    depends_on=None, block_reason=..., multi_assignee=None,
                    self_pane_id=None, updated_by=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        resolved = (self._task_assignees(assignees, self_pane_id)
                    if assignees is not None else None)
        return self.tasks.update(
            task_id, title=title, body=body, status=status, priority=priority,
            parent_id=parent_id, assignees=resolved, depends_on=depends_on,
            block_reason=block_reason, multi_assignee=multi_assignee,
            updated_by=updated_by or self_pane_id)

    @_as_dunk_error
    def task_assign(self, task_id, assignees, mode="add", self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.assign(
            task_id, self._task_assignees(assignees, self_pane_id), mode=mode)

    @_as_dunk_error
    def task_complete(self, task_id, by=None, self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        # Automatic work belongs to the actual caller, never a caller-supplied
        # completion label. Anonymous administrative completions do not feed lanes.
        if not self_pane_id:
            return self.tasks.complete(task_id, by=by)
        plan = self.next_jobs.advance(self_pane_id, "completion", task_id)
        self._next_job_wake.set()
        return dict(plan["task"], next_job={k: v for k, v in plan.items() if k != "task"})

    @_as_dunk_error
    def task_block(self, task_id, reason=None, self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.block(task_id, reason=reason)

    @_as_dunk_error
    def task_unblock(self, task_id, self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.unblock(task_id)

    @_as_dunk_error
    def task_cancel(self, task_id, reason=None, self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.cancel(task_id, reason=reason)

    @_as_dunk_error
    def task_delete(self, task_id, self_pane_id=None):
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.delete(task_id)

    @_as_dunk_error
    def task_note(self, task_id, event="note", reason=None, self_pane_id=None):
        """Append an explanatory event (e.g. why a task was removed)."""
        return self.tasks.note(task_id, event=event, actor=self_pane_id,
                               meta={"reason": reason} if reason else {})

    def require_turn_caller(self, pane_id, self_pane_id):
        """Authenticate socket turn events; request pane labels alone prove nothing."""
        from dunk_callers import caller_pane_auth_failure
        if not self_pane_id or (pane_id and pane_id != self_pane_id):
            log.warning("turn authentication refused reason=target_mismatch pane=%r self_pane=%r",
                        pane_id, self_pane_id)
            raise DunkError("turn event must target the caller's own pane")
        pane = self.herdr.get_pane(self_pane_id) or {}
        caller = self.current_caller()
        reason = caller_pane_auth_failure(caller, pane.get("pane_pid"))
        if reason is not None:
            log.warning("turn authentication refused reason=%s pane=%r caller_pid=%r "
                        "caller_source=%r pane_pid=%r", reason, self_pane_id,
                        (caller or {}).get("pid"), (caller or {}).get("source"),
                        pane.get("pane_pid"))
            raise DunkError("turn event caller is not authenticated to this pane")

    def turn_start(self, pane_id=None, ttl_s=None, source=None,
                   self_pane_id=None, handoff_id=None):
        """Declare a busy turn for `pane_id` (pi/claude hook or extension).

        The turn holds sends until turn_end, but carries a TTL so a crashed or
        force-quit agent cannot hold mail forever. The expiry is recorded and
        reconciled by turn_expire().
        """
        pane_id = pane_id or self_pane_id
        if not pane_id:
            raise DunkError("turn start requires a pane")
        if handoff_id is not None and not re.fullmatch(r"nj-[0-9a-f]{32}", handoff_id):
            raise DunkError("invalid next-job handoff id")
        ttl = float(ttl_s or TURN_TTL_S)
        if ttl <= 0:
            raise DunkError("ttl_s must be positive")
        now = time.time()
        import secrets
        state = {"started_at": now, "expires_at": now + ttl, "ttl_s": ttl,
                 "turn_id": secrets.token_hex(16),
                 "source": source, "caller": self.current_caller(), "handoff_id": handoff_id}
        with self._lock:
            self.turns[pane_id] = state
        self.metrics.record("turn.start", pane_id=pane_id, source=source,
                            outcome="ok", at=now)
        self.next_jobs.observe_turn(pane_id, state)
        return {"pane_id": pane_id, "started_at": now,
                "expires_at": now + ttl, "ttl_s": ttl, "source": source,
                "turn_id": state["turn_id"]}

    def turn_end(self, pane_id=None, reason="turn_end", self_pane_id=None,
                 turn_id=None, authenticated=False):
        pane_id = pane_id or self_pane_id
        if not pane_id:
            raise DunkError("turn end requires a pane")
        now = time.time()
        with self._lock:
            # Keep the ENDED turn instead of popping it: its end time is the only
            # evidence of IDLENESS the pi extension will ever give us. Popping it
            # made every pi lane read 'unknown' between turns.
            state = self.turns.get(pane_id)
            invalid = ("no_active_turn" if state is None else
                       "already_ended" if state.get("ended_at") is not None else
                       "expired_turn" if state["expires_at"] <= now else None)
            if authenticated and not invalid:
                caller, starter = self.current_caller() or {}, state.get("caller") or {}
                if not turn_id or turn_id != state.get("turn_id"):
                    invalid = "turn_mismatch"
                elif not caller.get("pid") or any(caller.get(k) != starter.get(k)
                                                   for k in ("pid", "start")):
                    invalid = "foreign_turn"
            if invalid:
                return {"pane_id": pane_id, "ended": False, "reason": invalid,
                        "dispatch": {"trigger": "turn.end", "action": "skip",
                                     "reason": invalid, "delivered": False}}
            state["ended_at"] = now
            state["expires_at"] = 0.0
        duration = int((now - state["started_at"]) * 1000) if state else None
        self.metrics.record("turn.end", pane_id=pane_id, reason=reason,
                            outcome="ok" if state else "unknown", at=now,
                            duration_ms=duration)
        result = {"pane_id": pane_id, "ended": bool(state), "reason": reason}
        # THE TRIGGER. The turn just ended and the lane is idle: if it still holds open
        # work, this is the moment to prompt it - seconds, not the next 60-120 minute
        # dunk. The send wait is bounded; backend/SQLite lookups have their own timeouts.
        if authenticated:
            result["dispatch"] = self._dispatch_on_turn_end(pane_id)
        else:
            result["dispatch"] = {"trigger": "turn.end", "action": "skip",
                                  "reason": "unauthenticated", "delivered": False}
        return result

    def _dispatch_on_turn_end(self, pane_id):
        plan = self.next_jobs.advance(pane_id, "turn.end")
        self._next_job_wake.set()
        if plan.get("reason") in ("disabled", "not_allowed", "unavailable_lane", "readiness_unknown"):
            return dict(plan, delivered=False)
        handoffs = self.tasks.next_job_handoffs(pane_id)
        if plan["action"] == "claim" or any(
                self.tasks.get(h["task_id"])["status"] not in ("done", "cancelled")
                for h in handoffs):
            return dict(plan, delivered=False)
        # Check-and-send spans backend calls: do not hold the global lock, but
        # reserve this pane so concurrent end notifications cannot enqueue twice.
        with self._lock:
            if pane_id in self.turn_end_dispatching:
                return {"trigger": "turn.end", "pane_id": pane_id, "action": "skip",
                        "reason": "dispatch_in_progress", "delivered": False}
            self.turn_end_dispatching.add(pane_id)
        try:
            return self._dispatch_claimed_turn_end(pane_id)
        finally:
            with self._lock:
                self.turn_end_dispatching.discard(pane_id)

    def _dispatch_claimed_turn_end(self, pane_id):
        """Prompt a lane that ended its turn still holding work. Returns its record.

        Every guardrail the task names, in order, and each one is a code the caller and
        the log can read:

          disabled                the trigger is off (DUNKINGSHEEP_TURN_END_DISPATCH=0)
          unread_unknown          the mailbox could not be read
          unread_mail             the lane is already fed (unread mail waiting)
          no_open_work            no claimed task: an idle lane is left alone
          not_allowed             the lane has not opted in to dispatch
          blocked/dead/working    the shared lane_plan gate refuses delivery
          cooldown/task_cooldown  the same pane or task was just prompted
          held_delivery_pending   a prompt for this pane is already queued, not delivered

        And the delivery distinction the task insists on: a send that is HELD (box would
        not clear, dead session, unconfirmed submit) is recorded as held/failed and does
        NOT count as delivered, and does not start the cooldown clock.
        """
        import dunk_dispatch
        now = time.time()
        record = {"trigger": "turn.end", "pane_id": pane_id, "action": "skip",
                  "reason": None, "delivered": False}
        if not TURN_END_DISPATCH:
            record["reason"] = "disabled"
            return record
        with self._lock:
            last_nudge = self.turn_end_nudges.get(pane_id)
        try:
            # `None` + the pane as the reader: that path uses the briefly cached
            # self-mailbox resolution, so a turn end does not cost an extra pane listing.
            unread = (self.inbox_count(None, self_pane_id=pane_id) or {}).get("unread")
        except Exception as error:                  # noqa: BLE001 - unknown is a skip
            unread = None
            record["error"] = f"inbox count failed: {error}"
        try:
            # Read this lane's live claims directly: a fleet-wide limited list can
            # omit its task, and the decorated record carries resolved blocked_by.
            tasks = [self.task_read(lease["task_id"], self_pane_id=pane_id)
                     for lease in self.tasks.leases_for_pane(pane_id)]
        except Exception as error:                  # noqa: BLE001 - unknown is a skip
            tasks = []
            record["error"] = f"task list failed: {error}"
        with self._lock:
            # task id -> when this pane was last prompted about it, for the tasks it holds
            nudges = {str(task.get("id")): self.turn_end_task_nudges[(pane_id, str(task.get("id")))]
                      for task in tasks
                      if (pane_id, str(task.get("id"))) in self.turn_end_task_nudges}
        plan = dunk_dispatch.turn_end_plan(
            pane_id=pane_id, tasks=tasks, unread=unread, last_nudge_at=last_nudge,
            task_nudges=nudges, now=now)
        if plan["action"] != "nudge":
            record["reason"] = plan["reason"]
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="held", reason=plan["reason"])
            log.info("turn.end %s: no nudge (trigger=turn.end reason=%s)",
                     pane_id, plan["reason"])
            return record
        # One prompt at a time per pane: something we typed is still waiting for the box
        # to clear, and stacking a second one behind it is how a lane gets three
        # contradictory prompts after one turn.
        with self._lock:
            pending = [m for m in (self.outbox.get(pane_id) or []) if m.status == "queued"]
        if pending:
            record["reason"] = "held_delivery_pending"
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="held", reason=record["reason"])
            log.info("turn.end %s: no nudge (trigger=turn.end reason=held_delivery_pending "
                     "message=%s)", pane_id, pending[0].id)
            return record
        # PR49 bypassed the opt-in allowlist. Claiming a task is necessary but does
        # not opt a reviewer/leader lane into automatic prompts. Reuse the SAME
        # configuration resolver and readiness gate as the timer dispatcher.
        import dunk_api
        import dunk_panes
        try:
            lanes = self.resolve_lanes()
            allowed = dunk_api._dispatch_allowlist(self, lanes=lanes)
            candidates = [p for p in lanes if p["pane_id"] == pane_id
                          and p.get("agent_status") not in (None, "unknown")
                          and not dunk_panes.is_retired(pane_id)]
            gate = dunk_dispatch.lane_plan(
                candidates, lanes=allowed, unread=lambda pane: unread,
                delivery=lambda pane: self.delivery_state(
                    pane_id, pane.get("agent_status"), pane=pane))
            if pane_id not in allowed:
                record["reason"] = "not_allowed"
            elif not candidates:
                record["reason"] = "unavailable_lane"
            elif not gate["ready"]:
                record["reason"] = next(name for name in
                                        ("has_mail", "blocked", "dead", "working")
                                        if gate[name])
        except Exception as error:                  # unknown readiness fails closed
            record.update(reason="readiness_unknown", error=str(error))
        if record["reason"]:
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="held", reason=record["reason"])
            log.info("turn.end %s: no nudge (trigger=turn.end reason=%s)",
                     pane_id, record["reason"])
            return record
        task = plan["item"]
        record.update(action="nudge", task_id=str(task.get("id")),
                      item=f"board:{task.get('id')}")
        text = dunk_dispatch.turn_end_text(task)
        try:
            message = self.send_now(pane_id, text, wait_s=TURN_END_WAIT_S,
                                    source="turn_end")
        except Exception as error:                  # noqa: BLE001 - never break turn_end
            record.update(delivered=False, outcome="refused", error=str(error))
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="failed", reason="refused")
            log.warning("turn.end %s: trigger=turn.end nudge REFUSED for task %s (%s); "
                        "not counted as delivered", pane_id, record["task_id"], error)
            return record
        outcome = message.get("outcome") or "unknown"
        delivered = outcome == "ok"
        record.update(message_id=message.get("message_id"), delivered=delivered,
                      outcome=outcome, message=message.get("message"))
        if delivered:
            with self._lock:
                self.turn_end_nudges[pane_id] = now
                self.turn_end_task_nudges[(pane_id, record["task_id"])] = now
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="ok", bytes=len(text.encode("utf-8")))
            log.info("turn.end %s: trigger=turn.end nudge DELIVERED task=%s item=%s",
                     pane_id, record["task_id"], record["item"])
        else:
            # HELD, NOT DELIVERED. The message stays in the outbox and the worker retries
            # when the box clears; saying "delivered" here would be the exact claim the
            # task forbids ("never count a held delivery as delivered").
            self.metrics.record("turn.end.dispatch", pane_id=pane_id, source="turn.end",
                                outcome="held", reason=outcome,
                                bytes=len(text.encode("utf-8")))
            log.warning("turn.end %s: trigger=turn.end nudge NOT delivered for task %s "
                        "(outcome=%s message=%s); held, not counted as delivered",
                        pane_id, record["task_id"], outcome,
                        record.get("message_id"))
        return record

    def turn_active(self, pane_id):
        """True only for an unexpired declared turn. Expiry is the safety."""
        with self._lock:
            state = self.turns.get(pane_id)
            if not state:
                return False
            return state["expires_at"] > time.time()

    def turn_expire(self, now=None):
        """Release expired turns: record the reason, reconcile the caller pid.

        This is the crashed-turn path: no turn_end arrived, so the TTL fires,
        mail is released, and the event says why. Reconciliation is recorded,
        not used to extend the hold: a live pid does not prove the turn is
        still running after the declared deadline.
        """
        now = now or time.time()
        released = []
        with self._lock:
            for pane_id, state in list(self.turns.items()):
                if state["expires_at"] > now:
                    continue
                self.turns.pop(pane_id, None)
                released.append((pane_id, state))
        for pane_id, state in released:
            caller = state.get("caller") or {}
            alive = bool(caller.get("pid") and process_alive(
                caller.get("pid"), caller.get("start")))
            log.warning(
                "turn expired on %s after %.0fs (ttl %.0fs); releasing hold "
                "(caller pid %s alive=%s)",
                pane_id, now - state["started_at"], state["ttl_s"],
                caller.get("pid"), alive)
            self.metrics.record(
                "turn.expired", pane_id=pane_id, reason="ttl",
                outcome="released", at=now,
                duration_ms=int((now - state["started_at"]) * 1000))
        return [{"pane_id": pane_id, "ttl_s": state["ttl_s"],
                 "reason": "ttl"} for pane_id, state in released]

    def set_caller(self, caller):
        self._caller_state.value = caller

    def current_caller(self):
        return getattr(self._caller_state, "value", None)

    def clear_caller(self):
        self._caller_state.value = None

    @_as_dunk_error
    def task_next(self, scope=None, lease_seconds=None, self_pane_id=None):
        """Atomically claim the next task; subscribe if the queue is empty.

        Auto-following on an empty queue is what makes the pull cheap: the
        daemon pushes the next task when this pane next goes idle instead of
        the agent polling.
        """
        caller = self.current_caller()
        value = "" if scope is None else str(scope).strip()
        follow_scope = TASK_GLOBAL
        if value in ("", "global"):
            target = TASK_GLOBAL
        elif value == "self":
            if not self_pane_id:
                raise DunkError("scope 'self' requires a known caller pane")
            target = self_pane_id
            follow_scope = self_pane_id
        elif value == "auto":
            target = TASK_GLOBAL
            if self_pane_id:
                local = self.tasks.claim_next(scope=self_pane_id,
                                              pane_id=self_pane_id,
                                              lease_seconds=lease_seconds,
                                              caller=caller)
                if local.get("task"):
                    return local
        else:
            target = self._task_scope(value, self_pane_id, default=TASK_GLOBAL)
            follow_scope = target

        result = self.tasks.claim_next(scope=target, pane_id=self_pane_id,
                                       lease_seconds=lease_seconds,
                                       caller=caller)
        if not result.get("task") and self_pane_id:
            with self._lock:
                self.task_waiters[self_pane_id] = follow_scope
            result["following"] = True
        return result

    @_as_dunk_error
    def task_follow(self, scope=None, self_pane_id=None):
        """Ask the daemon to push the next task when this pane is idle."""
        if not self_pane_id:
            raise DunkError("task follow requires a known caller pane")
        target = self._task_scope(scope, self_pane_id, default=TASK_GLOBAL)
        with self._lock:
            self.task_waiters[self_pane_id] = target
            followers = sorted(self.task_waiters)
        return {"pane_id": self_pane_id, "scope": target, "following": True,
                "followers": followers}

    @_as_dunk_error
    def task_unfollow(self, self_pane_id=None):
        if not self_pane_id:
            raise DunkError("task unfollow requires a known caller pane")
        with self._lock:
            existed = self.task_waiters.pop(self_pane_id, None) is not None
            followers = sorted(self.task_waiters)
        return {"pane_id": self_pane_id, "following": False,
                "was_following": existed, "followers": followers}

    @_as_dunk_error
    def task_followers(self):
        with self._lock:
            items = [{"pane_id": pane_id, "scope": scope}
                     for pane_id, scope in sorted(self.task_waiters.items())]
        return {"followers": items}

    @_as_dunk_error
    def task_heartbeat(self, task_id, lease_seconds=None, self_pane_id=None):
        if not self_pane_id:
            raise DunkError("task heartbeat requires a known caller pane")
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.heartbeat(task_id, self_pane_id,
                                    lease_seconds=lease_seconds,
                                    caller=self.current_caller())

    @_as_dunk_error
    def task_release(self, task_id, self_pane_id=None):
        if not self_pane_id:
            raise DunkError("task release requires a known caller pane")
        self._require_task_visible(self.tasks.get(task_id), self_pane_id)
        return self.tasks.release(task_id, self_pane_id)

    @_as_dunk_error
    def task_reap(self, self_pane_id=None):
        try:
            panes = {p["pane_id"] for p in self.herdr.list_panes()}
        except Exception:
            panes = None
        source = "live" if getattr(self.herdr, "live", False) else "fixture"
        return {"requeued": self.tasks.reap(alive_panes=panes,
                                            alive_source=source,
                                            caller_alive=process_alive)}

    @_as_dunk_error
    def task_stats(self, scope=None, self_pane_id=None):
        explicit = None
        if scope is not None and str(scope).strip() not in ("", "all"):
            explicit = self._task_scope(scope, self_pane_id, default=TASK_GLOBAL)
        return self.tasks.stats(scope=explicit, self_pane_id=self_pane_id)

    @_as_dunk_error
    def task_metrics(self, window_s=3600, since=None, until=None, scope=None,
                     bucket="hour", self_pane_id=None):
        """Deterministic, timeseries-friendly task metrics for the CTO.

        Complements the delivery `metrics` endpoint. Current state, throughput
        buckets and latencies all come from SQL; no model is involved. An
        agent can see global plus its own pane; a caller with no pane (the
        human/CTO CLI outside herdr) can see all scopes.
        """
        value = "" if scope is None else str(scope).strip()
        if value in ("", "all"):
            if self_pane_id:
                scopes = [TASK_GLOBAL, self_pane_id]
                label = "visible"
            else:
                scopes = None
                label = "all"
        elif value == "global":
            scopes = [TASK_GLOBAL]
            label = "global"
        else:
            target = self._task_scope(value, self_pane_id, default=TASK_GLOBAL)
            scopes = [target]
            label = target
        return self.tasks.metrics(window_s=window_s, since=since, until=until,
                                  scopes=scopes, bucket=bucket,
                                  scope_label=label)

    # -- memory and recall service -------------------------------------------

    def _memory_required(self):
        if self.memory is None:
            raise DunkError("memory service is not enabled; start the daemon "
                            "with persistence")
        return self.memory

    @_as_dunk_error
    def memory_recall(self, query, limit=10, harness=None, project=None,
                      since=None):
        return self._memory_required().recall(
            query, limit=limit, harness=harness, project=project, since=since)

    @_as_dunk_error
    def memory_context(self, query, sessions=5):
        return self._memory_required().context(query, sessions=sessions)

    @_as_dunk_error
    def memory_search(self, query, limit=10, harness=None, project=None,
                      since=None):
        return self.memory_recall(query, limit=limit, harness=harness,
                                  project=project, since=since)

    @_as_dunk_error
    def memory_stats(self):
        return self._memory_required().stats()

    @_as_dunk_error
    def memory_status(self):
        status = self._memory_required().status()
        if self.memory_ingestor is not None:
            status["ingestor"] = self.memory_ingestor.status()
        return status

    @_as_dunk_error
    def memory_backfill(self, action="start", limit=None):
        """Discover every known session source and enqueue it (throttled,
        resumable, pausable). Actions: start, pause, resume, status."""
        ingestor = self.memory_ingestor
        if ingestor is None or self.memory is None:
            raise DunkError("memory service is not enabled")
        action = (action or "start").lower()
        if action == "pause":
            ingestor.pause()
            return {"paused": True, "status": self.memory_status()}
        if action == "resume":
            ingestor.resume()
            return {"paused": False, "status": self.memory_status()}
        if action == "status":
            return self.memory_status()
        sources = memory_sources()
        enqueued = 0
        for path in sources:
            if limit and enqueued >= int(limit):
                break
            try:
                self.memory.enqueue(path)
                enqueued += 1
            except ValueError:
                continue
        with self._lock:
            self.last_message = f"Queued {enqueued} memory source(s)"
        return {"enqueued": enqueued, "discovered": len(sources),
                "limit": limit, "paused": ingestor.paused(),
                "status": self.memory_status()}

    @_as_dunk_error
    def memory_ingest(self, path, harness=None, project=None):
        store = self._memory_required()
        try:
            result = store.enqueue(path, harness=harness, project=project)
        except ValueError as error:
            raise DunkError(str(error)) from None
        with self._lock:
            self.last_message = f"Queued memory ingest {os.path.basename(path)}"
        return result

    @_as_dunk_error
    def health(self, repair=False, deep=False):
        """Deterministic self-check; see dunk_health.run_health."""
        from dunk_health import run_health
        return run_health(self, repair=bool(repair), deep=bool(deep))

    # -- deterministic task push/reap workers -------------------------------

    def _start_task_workers(self):
        if self.memory_ingestor is not None:
            self.memory_ingestor.start()
        self._task_threads = [
            threading.Thread(target=self._task_reaper_loop,
                             name="task-reaper", daemon=True),
            threading.Thread(target=self._task_push_loop,
                             name="task-push", daemon=True),
        ]
        for thread in self._task_threads:
            thread.start()

    def _clear_historical_errors(self, panes):
        """Clear a stale 'target pane not found' once the pane is back.

        A dunk that was skipping on a missing pane can keep the old error for
        ever if it never reaches a send. The reaper already has the live pane
        set, so clear it there rather than making an operator edit the state.
        """
        if not panes:
            return
        changed = False
        with self._lock:
            for dunker in self.dunkers:
                if (dunker.running and dunker.target_pane_id in panes
                        and dunker.last_error
                        and "target pane not found" in dunker.last_error):
                    dunker.last_error = None
                    changed = True
        if changed:
            self._save()

    def _task_reaper_loop(self):
        while not self._task_stop.wait(TASK_REAP_S):
            try:
                pane_list = self.herdr.list_panes()
                self._snapshot_launches(pane_list)
                panes = {p["pane_id"] for p in pane_list}
                self._clear_historical_errors(panes)
            except Exception:
                panes = None
            if panes is not None:
                try:
                    marked = self.inbox.mark_undeliverable(panes)
                    if marked["undeliverable"]:
                        log.warning(
                            "inbox: %d queued message(s) marked undeliverable "
                            "for dead panes %s", marked["undeliverable"],
                            ", ".join(str(p) for p in marked["panes"][:5]))
                except Exception:  # noqa: BLE001 - maintenance must not die
                    log.exception("inbox undeliverable sweep failed")
                try:
                    archived = self.inbox.auto_archive_undeliverable()
                    # A retired pane's mailbox is not undeliverable YET - it is
                    # retained and quiet. Same 24h age, same retention, no alerts.
                    try:
                        import dunk_panes
                        retired_boxes = {box for record in dunk_panes.all_retired()
                                         for box in (record.get("mailboxes") or [])}
                        archived += self.inbox.archive_retired(retired_boxes)
                    except Exception as error:      # noqa: BLE001 - never break the tick
                        log.warning("retired-mailbox archive failed: %s", error)
                    if archived:
                        self.metrics.record("inbox.archived", pane_id=None,
                                            source="inbox", outcome="ok",
                                            reason="undeliverable_24h")
                        log.info("inbox: auto-archived %d undeliverable message(s) "
                                 "older than 24h", archived)
                except Exception:  # noqa: BLE001
                    log.exception("inbox auto-archive failed")
            if not getattr(self.herdr, "live", False):
                # A FIXTURE BACKEND CANNOT REAP, and it must not try (t247). reap() refuses a
                # pane list that is not tagged live - correctly - so every fixture-backed Flock
                # used to log a TaskError traceback from this thread on every pass. That is what
                # made a green suite look like it aborted, and what sent a reviewer chasing a
                # state bug: the traceback is a REFUSAL WORKING, printed as if it were a fault.
                # Skipping here keeps the guard intact and stops the noise.
                log.debug("task reaper: %r backend is not live; not reaping",
                          getattr(self.herdr, "name", "fixture"))
                continue
            try:
                requeued = self.tasks.reap(alive_panes=panes, alive_source="live",
                                           caller_alive=process_alive)
            except Exception:
                log.exception("task reaper failed")
                continue
            if requeued:
                with self._lock:
                    self.last_message = (
                        f"Requeued {len(requeued)} task(s) "
                        f"({', '.join(item['task_id'] for item in requeued[:5])})")
                log.info("requeued task leases: %s", requeued)
            if self.memory is not None:
                # Deterministic self-repair: a crashed ingest worker's lease is
                # returned to the queue on every tick. No model, no human.
                try:
                    self.memory.recover_leases()
                    self.memory.requeue_partial()
                except Exception:  # noqa: BLE001 - maintenance must not die
                    log.exception("memory maintenance failed")
            if panes is not None:
                with self._lock:
                    for pane_id in list(self.task_waiters):
                        if pane_id not in panes:
                            self.task_waiters.pop(pane_id, None)

    def _task_push_loop(self):
        try:
            self.next_jobs.recover()
        except Exception:
            log.exception("next-job recovery failed")
        while not self._task_stop.is_set():
            self._next_job_wake.wait(TASK_PUSH_S)
            self._next_job_wake.clear()
            if self._task_stop.is_set():
                break
            try:
                self.next_jobs.tick()
                self._push_tasks()
            except Exception:
                log.exception("task push failed")

    def _push_tasks(self):
        """Feed idle followers one task each, using the same idle signal as
        `only_when="idle"` and the same guarded inbox nudge as everything else.

        No model is involved: the daemon selects, claims and delivers
        deterministically. A follower already holding a live lease is skipped.
        """
        with self._lock:
            followers = dict(self.task_waiters)
        if not followers:
            return
        try:
            panes = {p["pane_id"]: p for p in self.herdr.list_panes()}
        except Exception:
            return
        for pane_id in sorted(followers):
            pane = panes.get(pane_id)
            if pane is None:
                continue
            if (pane.get("agent_status") or "unknown") in BUSY_STATUSES:
                continue
            if self.tasks.leases_for_pane(pane_id):
                continue
            try:
                claimed = self.tasks.claim_next(
                    scope=followers[pane_id], pane_id=pane_id)
            except Exception:
                log.exception("task claim failed for %s", pane_id)
                continue
            task = claimed.get("task")
            if not task:
                continue
            self._announce_task(pane_id, task, claimed.get("lease_seconds"))

    def _announce_task(self, pane_id, task, lease_seconds):
        body = [
            f"TASK {task['id']} [{task['priority']}] {task['title']}",
            f"scope {task['scope']} | status {task['status']} | "
            f"lease {int(lease_seconds or 0)}s",
        ]
        if task.get("body"):
            body += ["", task["body"]]
        if task.get("depends_on"):
            body += ["", "depends on: " + ", ".join(task["depends_on"])]
        body += [
            "",
            f"heartbeat: dunkingsheep task heartbeat {task['id']}",
            f"complete:  dunkingsheep task complete {task['id']}",
            "next:      dunkingsheep task next",
        ]
        try:
            self.inbox_send(pane_id, "\n".join(body),
                            subject=f"TASK {task['id']}: {task['title']}",
                            self_pane_id=None)
        except Exception:
            log.exception("could not announce task %s to %s",
                          task["id"], pane_id)
        with self._lock:
            self.last_message = f"Pushed task {task['id']} to {pane_id}"

    # -- operations telemetry -----------------------------------------------

    def metrics_summary(self, window_s=3600, since=None, until=None):
        """A read-only snapshot of how delivery is going, for a status board.

        Carries identifiers and numbers only: mailbox names, pane ids, dunk
        ids, counts, durations and byte sizes. No message body, no subject, no
        filename, no account email, and nothing about token spend, which would
        be a guess dressed as a measurement.

        A duration is null, never zero, when the window holds no event that
        measures it, and `coverage` says when durable recording began, so
        "nothing happened" is never confused with "not recorded"."""
        now = time.time()
        end = float(until) if until else now
        start = float(since) if since else end - max(1.0, float(window_s))
        counts, durations, retries = self.metrics.summary(start, end)

        missing = [name for name, value in durations.items() if value is None]
        earliest = self.metrics.earliest()
        if earliest is None:
            notes = ("no events recorded yet; durable telemetry began with this "
                     "daemon and records as traffic happens")
        elif earliest > start:
            notes = ("the window starts before durable recording did, so it is "
                     "only covered from coverage.events_since onwards")
        else:
            notes = "the window is fully covered by durable recording"

        with self._lock:
            queued = [m for queue in self.outbox.values() for m in queue
                      if m.status == "queued"]
            dunks = [{
                "id": d.id,
                "target_pane_id": d.target_pane_id,
                "running": d.running,
                "interval_minutes": d.interval_minutes,
                "send_count": d.send_count,
                "skip_count": d.skip_count,
                "total_skips": d.total_skips,
                "hold_count": d.hold_count,
                "created_at": d.created_at,
            } for d in self.dunkers]

        inbox_stats = self.inbox.stats()
        return {
            "schema_version": METRICS_SCHEMA_VERSION,
            "generated_at": now,
            "interval": {"start": start, "end": end, "seconds": round(end - start, 3)},
            "units": {"duration": "milliseconds", "size": "bytes",
                      "timestamp": "unix_epoch_seconds", "rate": "per_hour"},
            "event_definitions": dunk_metrics.KINDS,
            "outcome_definitions": dunk_metrics.OUTCOMES,
            "reason_definitions": dunk_metrics.REASONS,
            "counts": counts,
            "durations": durations,
            "retries": retries,
            "queue": {
                "depth_now": len(queued),
                "oldest_wait_s": (int(now - min(m.created_at for m in queued))
                                  if queued else None),
            },
            "inbox": {
                "unread_now": sum(m["unread"] for m in inbox_stats),
                "mailboxes": len(inbox_stats),
            },
            "cumulative": {
                "note": ("counters kept per schedule since it was created; these "
                         "are not windowed and must not be read as a rate"),
                "dunks": dunks,
            },
            "coverage": {
                "events_since": earliest,
                "window_has_events": counts["events"] > 0,
                "missing": missing,
                "retention_s": self.metrics.retention_s,
                "notes": notes,
            },
        }

    # -- herdr layout control -----------------------------------------------
    # Workspaces, tabs and panes can be created, renamed, focused and closed
    # through the same surfaces that schedule dunks, so an admin agent can
    # spin up a team (a workspace per project, a tab per agent) and then dunk
    # on it. Everything here is a thin, resolved call into HerdrClient.

    LAYOUT_KINDS = ("workspace", "tab", "pane")

    def list_workspaces(self):
        """Workspaces with their tabs nested, sorted by number."""
        tabs = self.herdr.list_tabs()
        for tab in tabs:  # older herdr omits workspace_id; it is the id's prefix
            tab.setdefault("workspace_id", str(tab.get("tab_id", "")).split(":")[0])
        workspaces = self.herdr.list_workspaces()
        for workspace in workspaces:
            workspace["tabs"] = sorted(
                (t for t in tabs if t.get("workspace_id") == workspace.get("workspace_id")),
                key=lambda t: t.get("number", 0),
            )
        workspaces.sort(key=lambda w: w.get("number", 0))
        return workspaces

    def _self_pane(self, self_pane_id):
        if not self_pane_id:
            raise DunkError("'self' needs HERDR_PANE_ID (run from inside a herdr pane)")
        pane = self.herdr.get_pane(self_pane_id)
        if pane is None:
            raise DunkError(f"own pane {self_pane_id} not found in herdr")
        return pane

    @staticmethod
    def _pick(items, spec, kind, id_key, label_key="label"):
        """Resolve `spec` against a list: exact id, exact label (case-
        insensitive), then unique substring of the label."""
        needle = str(spec).strip()
        if not needle:
            raise DunkError(f"{kind} is required")
        for item in items:
            if item.get(id_key) == needle:
                return item
        low = needle.lower()
        exact = [i for i in items if str(i.get(label_key) or "").lower() == low]
        if len(exact) == 1:
            return exact[0]
        if len(exact) > 1:
            raise DunkError(f"{kind} {spec!r} is ambiguous: "
                            + ", ".join(i[id_key] for i in exact))
        partial = [i for i in items if low in str(i.get(label_key) or "").lower()]
        if len(partial) == 1:
            return partial[0]
        if partial:
            raise DunkError(f"{kind} {spec!r} is ambiguous: "
                            + ", ".join(f"{i[id_key]} ({i.get(label_key)})" for i in partial))
        raise DunkError(f"no herdr {kind} matches {spec!r}")

    def resolve_workspace(self, spec, self_pane_id=None):
        """Workspace dict from an id ('w9'), a label, a unique label substring
        or 'self' (the caller's own workspace)."""
        spec = str(spec or "").strip()
        if spec.lower() in ("self", "me", "here"):
            spec = self._self_pane(self_pane_id).get("workspace_id")
        workspaces = self.herdr.list_workspaces()
        if not workspaces:
            raise DunkError(self.herdr.server_error_hint())
        return self._pick(workspaces, spec, "workspace", "workspace_id")

    def resolve_tab(self, spec, self_pane_id=None):
        """Tab dict from an id ('w9:t2'), a label, a unique label substring or
        'self' (the caller's own tab)."""
        spec = str(spec or "").strip()
        if spec.lower() in ("self", "me", "here"):
            spec = self._self_pane(self_pane_id).get("tab_id")
        tabs = self.herdr.list_tabs()
        if not tabs:
            raise DunkError(self.herdr.server_error_hint())
        return self._pick(tabs, spec, "tab", "tab_id")

    # -- superuser allowlist -------------------------------------------------

    def _pane_info(self, pane_id):
        if not pane_id:
            return None
        # list_panes_grouped annotates each pane with its tab/workspace labels;
        # get_pane alone does not, and the name-based allowlist needs them.
        try:
            for pane in self.herdr.list_panes_grouped():
                if pane.get("pane_id") == pane_id:
                    return pane
        except Exception:  # noqa: BLE001 - identity check must not crash
            pass
        try:
            return self.herdr.get_pane(pane_id)
        except Exception:  # noqa: BLE001 - identity check must not crash
            return None

    def is_superuser(self, self_pane_id):
        self.superusers.refresh()          # the file can change mid-turn
        return self.superusers.is_superuser(self_pane_id,
                                            self._pane_info(self_pane_id))

    def require_superuser(self, self_pane_id, action):
        """Fail closed: no non-empty allowlist means no destructive action."""
        if self.superusers.empty():
            raise DunkError(
                "superuser allowlist is empty; run `dunkingsheep superuser "
                "recover` from the VP pane, or add a pane/name as a superuser")
        if not self.is_superuser(self_pane_id):
            raise DunkError(
                f"{action} requires a superuser; {self_pane_id or 'this caller'} "
                "is not on the allowlist. Non-superusers can use the inbox and "
                "task queue.")

    def superuser_status(self, self_pane_id=None):
        return {"superuser": self.is_superuser(self_pane_id),
                "pane_id": self_pane_id,
                **self.superusers.describe()}

    def superuser_add(self, entry, self_pane_id=None):
        if not self.superusers.empty() and not self.is_superuser(self_pane_id):
            raise DunkError("superuser add requires a superuser")
        try:
            value = self.superusers.add(entry)
        except SuperuserError as error:
            raise DunkError(str(error)) from None
        with self._lock:
            self.last_message = f"Added superuser {value}"
        return {"added": value, **self.superuser_status(self_pane_id)}

    def superuser_remove(self, entry, self_pane_id=None):
        if not self.is_superuser(self_pane_id):
            raise DunkError("superuser remove requires a superuser")
        entry = str(entry or "").strip()
        remaining = (len(self.superusers.panes) - (1 if ":" in entry else 0)
                     + len(self.superusers.names) - (0 if ":" in entry else 1))
        if remaining <= 0:
            raise DunkError("refusing to remove the last superuser")
        try:
            removed = self.superusers.remove(entry)
        except SuperuserError as error:
            raise DunkError(str(error)) from None
        with self._lock:
            self.last_message = f"Removed superuser {entry}"
        return {"removed": removed, "entry": entry,
                **self.superuser_status(self_pane_id)}

    def superuser_recover(self, self_pane_id=None, name=None):
        """Re-assert the caller's pane and the configured VP name.

        Bootstrap (empty allowlist) is allowed; after that only an existing
        superuser can recover. This is the setup/takeover step: the VP pane
        gets its current HERDR_PANE_ID and name written into the allowlist.
        """
        if not self_pane_id:
            raise DunkError("superuser recover requires a known caller pane")
        if not self.superusers.empty() and not self.is_superuser(self_pane_id):
            raise DunkError(
                "superuser recover requires an existing superuser; ask one to "
                "run `dunkingsheep superuser add <your-pane>`")
        name = str(name or DEFAULT_VP_NAME).strip()
        try:
            self.superusers.add(self_pane_id)
            if name:
                self.superusers.add(name)
        except SuperuserError as error:
            raise DunkError(str(error)) from None
        with self._lock:
            self.last_message = f"Recovered superuser {self_pane_id} ({name})"
        return {"recovered": True, "pane_id": self_pane_id, "name": name,
                **self.superuser_status(self_pane_id)}

    def require_send_self(self, target, self_pane_id, action):
        """Non-superusers may only type into their own pane.

        A DESK CREDENTIAL is the one exception, and only because the caller has
        already been narrowed to send_text/send_keys on an explicit pane set
        (dunk_desk.DeskCredential). It is not a superuser entry and grants nothing
        else; the flag is set per request by _authorise_send.
        """
        if self.is_superuser(self_pane_id):
            return
        if getattr(getattr(self, "_caller_state", None), "desk_send", False):
            return
        if not self_pane_id:
            raise DunkError(
                f"{action} requires a superuser; non-superusers can use the "
                "inbox and task queue")
        try:
            resolved = self.resolve_target(target, self_pane_id)["pane_id"]
        except DunkError:
            raise
        if resolved != self_pane_id:
            raise DunkError(
                f"{action} to another pane requires a superuser; use "
                "`dunkingsheep inbox send` instead")

    def _resolve_kind(self, kind, target, self_pane_id=None):
        kind = str(kind or "").strip().lower()
        if kind not in self.LAYOUT_KINDS:
            raise DunkError("kind must be 'workspace', 'tab' or 'pane'")
        if kind == "workspace":
            item = self.resolve_workspace(target, self_pane_id)
            return kind, item["workspace_id"], item
        if kind == "tab":
            item = self.resolve_tab(target, self_pane_id)
            return kind, item["tab_id"], item
        item = self.resolve_target(target, self_pane_id)
        return kind, item["pane_id"], item

    def _after_create(self, ok, result, command, what):
        if not ok:
            raise DunkError(f"herdr could not create the {what}: {result}")
        pane = result.get("root_pane") or result.get("pane") or result.get("agent") or {}
        out = {
            "workspace": result.get("workspace"),
            "tab": result.get("tab"),
            "pane": pane,
            "pane_id": pane.get("pane_id"),
            "ran": None,
        }
        if command and pane.get("pane_id"):
            ran, message = self.submit_text(pane["pane_id"], command)
            out["ran"] = command if ran else None
            if not ran:
                out["error"] = f"created, but running the command failed: {message}"
        with self._lock:
            self.last_message = f"Created {what}"
        log.info("created %s -> %s%s", what, pane.get("pane_id"),
                 f" running {command!r}" if command else "")
        return out

    def create_workspace(self, label=None, cwd=None, command=None, focus=False,
                         mailbox=None):
        """New workspace (with its first tab and pane). `command`, if given, is
        typed + Enter into that pane once it exists.

        `mailbox` names the lane the new pane IS, and is written into the lane
        registry as soon as the pane exists. That is the write-back a backend
        flip needs: without it the only record of the lane is a title the agent
        itself may overwrite, and a restore produces a pane nobody can address.
        """
        ok, result = self.herdr.create_workspace(label=label, cwd=cwd, focus=focus)
        out = self._after_create(ok, result, command, f"workspace {label or ''}".strip())
        if mailbox and out.get("pane_id"):
            self._remember_lane(out["pane_id"], str(mailbox).strip(), declared=True)
            self._title_pane(out["pane_id"], mailbox)
        return out

    def create_tab(self, workspace=None, label=None, cwd=None, command=None, focus=False,
                   self_pane_id=None):
        """New tab. `workspace` is an id, label or 'self'; omitted means the
        caller's own workspace when known, else herdr's focused one."""
        workspace_id = None
        if workspace:
            workspace_id = self.resolve_workspace(workspace, self_pane_id)["workspace_id"]
        elif self_pane_id:
            own = self.herdr.get_pane(self_pane_id)
            workspace_id = own.get("workspace_id") if own else None
        ok, result = self.herdr.create_tab(workspace_id=workspace_id, label=label, cwd=cwd,
                                           focus=focus)
        return self._after_create(ok, result, command, f"tab {label or ''}".strip())

    def split_pane(self, target, direction="right", cwd=None, command=None, focus=False,
                   self_pane_id=None):
        """Split an existing pane (any pane target) right or down."""
        direction = str(direction or "right").lower()
        if direction not in ("right", "down"):
            raise DunkError("direction must be 'right' or 'down'")
        pane = self.resolve_target(target, self_pane_id)
        ok, result = self.herdr.split_pane(pane["pane_id"], direction=direction, cwd=cwd,
                                           focus=focus)
        return self._after_create(ok, result, command, "pane")

    def _sandbox_workspace_id(self):
        """The dedicated workspace where spawned agents go by default.

        The 2026-10-18 incident: agents started without an explicit
        workspace/tab split into the VP's own tab, and a cleanup then closed
        that tab with the VP in it. Never default into the caller's tab.
        """
        label = (os.environ.get("DUNKINGSHEEP_SANDBOX_WORKSPACE")
                 or "sandbox").strip() or "sandbox"
        for workspace in self.herdr.list_workspaces():
            if str(workspace.get("label") or "").strip().lower() == label.lower():
                return workspace.get("workspace_id")
        ok, result = self.herdr.create_workspace(label=label, cwd=None, focus=False)
        if not ok:
            raise DunkError(f"could not create sandbox workspace {label!r}: {result}")
        workspace_id = (result.get("workspace") or {}).get("workspace_id")
        if not workspace_id:
            raise DunkError(f"sandbox workspace {label!r} was created without an id")
        return workspace_id

    def start_agent(self, name, command, workspace=None, tab=None, split=None, cwd=None,
                    focus=False, self_pane_id=None):
        """Launch `command` in a new pane registered with herdr as agent `name`
        (`herdr agent start`). Placement: inside `tab` (splitting it when
        `split` is right/down), else in `workspace` (id, label or 'self';
        herdr picks the tab), else in the dedicated sandbox workspace."""
        name = str(name or "").strip()
        if not name:
            raise DunkError("agent name is required")
        argv = shlex.split(str(command or ""))
        if not argv:
            raise DunkError("command is required (e.g. 'claude' or 'codex --model gpt-5')")
        workspace_id = tab_id = None
        if tab:
            tab_id = self.resolve_tab(tab, self_pane_id)["tab_id"]
        if workspace:
            workspace_id = self.resolve_workspace(workspace, self_pane_id)["workspace_id"]
        if not tab and not workspace:
            workspace_id = self._sandbox_workspace_id()
        if split and split not in ("right", "down"):
            raise DunkError("split must be 'right' or 'down'")
        ok, result = self.herdr.start_agent(name, argv, workspace_id=workspace_id,
                                            tab_id=tab_id, split=split, cwd=cwd, focus=focus)
        if not ok:
            raise DunkError(f"herdr could not start agent {name!r}: {result}")
        agent = result.get("agent") or {}
        # VERIFY BEFORE CLAIMING "STARTED" (t215): eight opencode panes were reported started
        # and had actually died with 'opencode: not found' (rc=127) because the pane's PATH
        # lacked /snap/bin. "The pane exists" is not "the agent runs": read what the pane says
        # before reporting success, and refuse with the captured line if it says the command
        # never started.
        self._verify_agent_started(name, agent.get("pane_id"))
        session = agent.get("agent_session") or {}
        self._record_launch(
            agent.get("pane_id"), agent=name, argv=list(argv),
            command=shlex.join(argv), cwd=cwd or agent.get("cwd"),
            session=session.get("value"), session_kind=session.get("kind"),
            name=name, source="start_agent")
        with self._lock:
            self.last_message = f"Started agent {name}"
        log.info("started agent %s -> %s: %s", name, agent.get("pane_id"), argv)
        return {"agent": agent, "pane_id": agent.get("pane_id"), "name": name,
                "argv": result.get("argv") or argv}

    # How long a freshly started agent gets to show a failure, and the lines that mean it
    # never ran. A shell's "not found" arrives in milliseconds; a working agent prints its
    # banner in the same window.
    AGENT_VERIFY_S = float(os.environ.get("DUNKINGSHEEP_AGENT_VERIFY_S", "2.0"))
    # SHELL-SHAPED ONLY (reviewer, i28041): a bare 'not found' is not evidence of anything -
    # "theme cache not found, rebuilding" is a working agent's own startup line. What a shell
    # says when a command is missing is these shapes, and only these are refusals.
    AGENT_FAILURE_MARKERS = (
        "command not found",        # bash/zsh: "foo: command not found"
        ": not found",              # dash/sh: "/bin/sh: 1: opencode: not found"
        "no such file or directory",
        "permission denied",
        "cannot execute",
    )

    def _verify_agent_started(self, name, pane_id):
        """Refuse to report 'started' for a pane whose command never ran.

        Fail closed: if the pane's first lines name a missing command, or the pane is already
        gone, raise with that line as the evidence. A start line that reports success for a dead
        agent is worse than a refusal, because nobody looks again.

        KNOWN LIMITS, named rather than implied:
        - a bare shell prompt that simply sits idle looks like a working agent's idle UI and
          passes; nothing on the screen distinguishes "agent at rest" from "shell at rest".
        - a refusal leaves the pane up (pane-safety: this code never closes a pane). The refusal
          names its id so a superuser can close it.
        """
        if not pane_id:
            raise DunkError(f"agent {name!r} was launched without a pane id")
        deadline = time.time() + self.AGENT_VERIFY_S
        captured = ""
        while time.time() < deadline:
            pane = self.herdr.get_pane(pane_id)
            if pane is None:
                raise DunkError(
                    f"agent {name!r} did not stay up: pane {pane_id} is already gone"
                    + (f"; captured: {captured.strip()[:200]!r}" if captured.strip() else ""))
            captured = self.herdr.read_pane(pane_id, lines=20) or ""
            lowered = captured.lower()
            for marker in self.AGENT_FAILURE_MARKERS:
                if marker in lowered:
                    line = next((row for row in captured.splitlines()
                                 if marker in row.lower()), captured.strip()[:200])
                    raise DunkError(
                        f"agent {name!r} did not start in pane {pane_id}: {line.strip()!r}. "
                        "The pane is up but the command never ran - check the PATH the pane "
                        "inherits.")
            # An agent that is drawing its own UI has started; a bare shell that has printed
            # nothing yet is neither. Keep waiting for the rest of the window.
            if "[agent exited rc=" in lowered:
                line = next(row for row in captured.splitlines() if "agent exited rc=" in row)
                raise DunkError(
                    f"agent {name!r} exited immediately in pane {pane_id}: {line.strip()!r}")
            time.sleep(0.1)

    # -- post-upgrade recovery ------------------------------------------------

    @staticmethod
    def _is_empty_shell(pane):
        if not pane or pane.get("agent"):
            return False
        if (pane.get("agent_status") or "unknown") not in (None, "unknown"):
            return False
        title = (pane.get("terminal_title_stripped") or "").strip()
        return bool(re.match(r"^[^@\s]+@[^:\s]+:", title))

    @staticmethod
    def _resume_command(agent, session):
        if not agent or not session:
            return None
        if agent == "pi":
            return f"pi --session {shlex.quote(session)}"
        if agent == "codex":
            return f"codex resume {shlex.quote(session)}"
        if agent == "claude":
            return f"claude --resume {shlex.quote(session)}"
        return None  # opencode/unknown: do not invent a command

    def _launch_for(self, pane_id, dunker, entry, cwd):
        entry = entry or {}
        argv = entry.get("argv")
        if argv:
            command = shlex.join(argv)
            if any(flag in argv for flag in ("--session", "--resume", "resume")):
                return command, "recorded-argv"
        agent = entry.get("agent") or (dunker.target_agent if dunker else None)
        session = entry.get("session")
        source = "recorded-session"
        if not session and agent and cwd and self.memory is not None:
            harness = agent if agent in ("pi", "codex", "claude", "opencode") else None
            if harness:
                row = self.memory.latest_session(harness, cwd)
                if row:
                    session = row["id"]
                    source = "memory-latest"
        command = self._resume_command(agent, session)
        if not command and argv:
            return shlex.join(argv), "recorded-argv"
        return (command, source) if command else (None, None)

    @staticmethod
    def _with_cwd(cwd, command):
        if cwd and command and not command.startswith("cd "):
            return f"cd {shlex.quote(cwd)} && {command}"
        return command

    def _doctor_warnings(self, by_id):
        warnings = []
        for dunker in self.dunkers:
            if not dunker.running or not dunker.target_pane_id:
                continue
            pane = by_id.get(dunker.target_pane_id)
            if pane is None:
                warnings.append({"dunk": dunker.id, "pane_id": dunker.target_pane_id,
                                 "kind": "pane-missing",
                                 "detail": "target pane no longer exists"})
                continue
            live_agent = pane.get("agent")
            if dunker.target_agent and live_agent and dunker.target_agent != live_agent:
                warnings.append({
                    "dunk": dunker.id, "pane_id": dunker.target_pane_id,
                    "kind": "agent-mismatch", "target_agent": dunker.target_agent,
                    "pane_agent": live_agent,
                    "detail": f"dunk says {dunker.target_agent}, pane is {live_agent}"})
            live_cwd = pane.get("cwd") or pane.get("foreground_cwd")
            if (dunker.target_cwd and live_cwd
                    and os.path.normpath(dunker.target_cwd) != os.path.normpath(live_cwd)):
                warnings.append({
                    "dunk": dunker.id, "pane_id": dunker.target_pane_id,
                    "kind": "cwd-mismatch", "target_cwd": dunker.target_cwd,
                    "pane_cwd": live_cwd,
                    "detail": f"dunk says {dunker.target_cwd}, pane is {live_cwd}"})
        return warnings

    def _guard_status(self):
        try:
            from dunk_guard import status
            return status()
        except Exception as error:  # noqa: BLE001 - report, never raise
            return {"error": str(error)}

    def recover(self, apply=False, record=False, self_pane_id=None):
        """Plan (and optionally apply) post-upgrade agent recovery.

        Sources: launches.json (exact argv/session), herdr's live agent_session,
        then the memory index's latest session for the dunk's harness+cwd. A
        missing session is reported, never invented. --apply types only into
        panes that are still bare shells.
        """
        panes = self.herdr.list_panes()
        by_id = {pane.get("pane_id"): pane for pane in panes}
        if record:
            self._snapshot_launches(panes)
        plan, seen = [], set()
        for dunker in self.dunkers:
            if not dunker.running or not dunker.target_pane_id:
                continue
            pane_id = dunker.target_pane_id
            seen.add(pane_id)
            pane = by_id.get(pane_id)
            cwd = (pane.get("cwd") if pane else None) or dunker.target_cwd
            item = {"pane_id": pane_id, "dunk": dunker.id,
                    "agent": (pane or {}).get("agent") or dunker.target_agent,
                    "cwd": cwd, "target_agent": dunker.target_agent,
                    "target_cwd": dunker.target_cwd}
            if pane is None:
                item.update(action="pane-missing",
                            reason="target pane no longer exists")
            elif pane.get("agent"):
                item.update(action="already-running",
                            reason=f"{pane['agent']} is live")
            elif not self._is_empty_shell(pane):
                item.update(action="not-empty-shell",
                            reason="pane is not a bare shell")
            else:
                command, source = self._launch_for(
                    pane_id, dunker, self.launches.get(pane_id), cwd)
                if command:
                    item.update(action="launch",
                                command=self._with_cwd(dunker.target_cwd or cwd, command),
                                source=source)
                else:
                    item.update(action="no-session",
                                reason="no recorded launch or unique session; start manually")
            plan.append(item)
        for pane_id, entry in self.launches.items():
            if pane_id in seen:
                continue
            pane = by_id.get(pane_id)
            if not pane or not self._is_empty_shell(pane):
                continue
            command, source = self._launch_for(pane_id, None, entry, entry.get("cwd"))
            if command:
                plan.append({"pane_id": pane_id, "dunk": None,
                             "agent": entry.get("agent"), "cwd": entry.get("cwd"),
                             "action": "launch",
                             "command": self._with_cwd(entry.get("cwd"), command),
                             "source": source,
                             "reason": "recorded pane (no running dunk)"})
        warnings = self._doctor_warnings(by_id)
        applied = []
        if apply:
            for item in plan:
                if item["action"] != "launch":
                    continue
                pane = self.herdr.get_pane(item["pane_id"])
                if pane is None or pane.get("agent") or not self._is_empty_shell(pane):
                    item["applied"] = False
                    item["apply_reason"] = "pane is no longer an empty shell"
                    continue
                ok, detail = self.herdr.send_text_and_enter(
                    item["pane_id"], item["command"])
                item["applied"] = bool(ok)
                item["apply_detail"] = detail
                if ok:
                    self._record_launch(
                        item["pane_id"], agent=item.get("agent"),
                        command=item["command"], cwd=item.get("cwd"),
                        source="recover", applied_at=time.time())
                    applied.append(item["pane_id"])
        return {"ok": bool(not warnings
                           and all(item["action"] in ("launch", "already-running")
                                   for item in plan)),
                "apply": bool(apply), "herdr": self.herdr.version(),
                "plan": plan, "warnings": warnings, "applied": applied,
                "launches": len(self.launches)}

    def doctor(self, self_pane_id=None):
        """Read-only post-upgrade check of the herdr surfaces and dunk metadata."""
        panes = self.herdr.list_panes()
        by_id = {pane.get("pane_id"): pane for pane in panes}
        read_ok = None
        if panes:
            read_ok = self.herdr.read_pane(panes[0]["pane_id"], lines=3,
                                           source="visible") is not None
        guard = self._guard_status()
        warnings = self._doctor_warnings(by_id)
        guard_ok = bool(guard.get("shadowing_herdr") or guard.get("installed"))
        return {"ok": bool(not warnings and read_ok and guard_ok),
                "herdr_version": self.herdr.version(),
                "panes": {"total": len(panes),
                          "with_agent": sum(1 for pane in panes if pane.get("agent")),
                          "with_session": sum(1 for pane in panes
                                              if pane.get("agent_session"))},
                "surfaces": {"pane_list": bool(panes),
                             "pane_get": bool(panes) and self.herdr.get_pane(
                                 panes[0]["pane_id"]) is not None,
                             "read_visible": read_ok, "guard": guard},
                "warnings": warnings, "launches": len(self.launches)}

    def rename(self, kind, target, label, self_pane_id=None):
        label = str(label or "").strip()
        if not label:
            raise DunkError("label is required")
        kind, target_id, _item = self._resolve_kind(kind, target, self_pane_id)
        ok, result = self.herdr.rename(kind, target_id, label)
        if not ok:
            raise DunkError(f"herdr could not rename {kind} {target_id}: {result}")
        log.info("renamed %s %s -> %r", kind, target_id, label)
        return {"kind": kind, "id": target_id, "label": label,
                kind: (result or {}).get(kind)}

    def focus(self, kind, target, self_pane_id=None):
        kind, target_id, _item = self._resolve_kind(kind, target, self_pane_id)
        ok, result = self.herdr.focus(kind, target_id)
        if not ok:
            raise DunkError(f"herdr could not focus {kind} {target_id}: {result}")
        return {"kind": kind, "id": target_id, "focused": True}

    def close_target(self, kind, target, self_pane_id=None, force=False):
        """Close a workspace, tab or pane, killing whatever runs inside.
        Refuses to close the caller's own pane or its containers unless
        `force` is set, since a process closing its own terminal is almost
        always a mistake; do it from another pane. Dunks aimed into the
        closed area are stopped."""
        kind, target_id, item = self._resolve_kind(kind, target, self_pane_id)
        own = self.herdr.get_pane(self_pane_id) if self_pane_id else None
        if own and own.get({"workspace": "workspace_id", "tab": "tab_id",
                            "pane": "pane_id"}[kind]) == target_id and not force:
            raise DunkError(
                f"refusing to close the {kind} this caller runs in "
                f"({target_id}); pass --force only if you intend to kill your "
                "own pane/tab/workspace")
        doomed = [p["pane_id"] for p in self.herdr.list_panes()
                  if p.get({"workspace": "workspace_id", "tab": "tab_id",
                            "pane": "pane_id"}[kind]) == target_id]
        ok, result = self.herdr.close(kind, target_id)
        if not ok:
            raise DunkError(f"herdr could not close {kind} {target_id}: {result}")
        stopped = []
        with self._lock:
            for dunker in self.dunkers:
                if dunker.running and dunker.target_pane_id in doomed:
                    self._halt(dunker)
                    dunker.running = False
                    dunker.next_send_at = None
                    dunker.status = "Target closed"
                    stopped.append(dunker.id)
            if stopped:
                self._save()
            self.last_message = f"Closed {kind} {item.get('label') or target_id}"
        log.info("closed %s %s (panes %s); stopped dunks %s", kind, target_id, doomed, stopped)
        return {"kind": kind, "id": target_id, "closed_panes": doomed,
                "stopped_dunks": stopped}

    # Passthrough refusals: commands that would take herdr itself down or need
    # an interactive terminal. Everything else herdr offers is fair game.
    HERDR_BLOCKED = (
        (), ("server", "stop"), ("update",), ("channel", "set"), ("session",),
        ("completion",), ("agent", "attach"), ("integration",),
    )
    # Typing into a pane must go through send_text, which waits for the human
    # to stop typing. Reaching these through the passthrough would bypass every
    # protection in this module; `pane send-keys ... Enter` is the worst of
    # them, because it submits whatever half-finished sentence is in the box.
    HERDR_TYPING = {
        ("pane", "send-text"): "send_text(target=..., text=...)",
        ("pane", "send-keys"): "send_text(target=..., text=...)",
        ("pane", "run"): "send_text(target=..., text=...)",
        ("agent", "send"): "send_text(target=..., text=...)",
    }

    def herdr_command(self, command, self_pane_id=None):
        """Run any `herdr <subcommand ...>` and return its parsed result. This
        is the escape hatch for everything without a dedicated command:
        `herdr_command("tab focus w9:t2")`, `herdr_command("api schema --json")`.
        The word `self` as an argument becomes the caller's pane id (its tab
        or workspace id for `tab ...` / `workspace ...` commands). herdr wants
        ids here, not labels; the typed commands above resolve labels. Returns
        {"ok", "result" (parsed JSON when herdr printed JSON, else None),
        "text" (raw stdout), "error" (herdr's message, if any)}."""
        if getattr(self.herdr, "name", "herdr") != "herdr":
            raise DunkError(
                "the herdr passthrough is not available with the "
                f"{getattr(self.herdr, 'name', 'unknown')} terminal backend; "
                "use the dedicated workspace/tab/pane commands instead")
        argv = shlex.split(str(command or "")) if not isinstance(command, list) \
            else [str(a) for a in command]
        if argv and argv[0] == "herdr":
            argv = argv[1:]
        if not argv:
            raise DunkError("command is required, e.g. 'workspace list' (see `herdr --help`)")
        for blocked in self.HERDR_BLOCKED:
            if blocked and tuple(argv[:len(blocked)]) == blocked:
                raise DunkError(f"'herdr {' '.join(blocked)}' is not allowed from here "
                                "(it would stop, replace or attach herdr itself)")
        instead = self.HERDR_TYPING.get(tuple(argv[:2]))
        if instead:
            raise DunkError(
                f"'herdr {' '.join(argv[:2])}' types into a pane, which would land on top of "
                f"whatever a human is writing there. Use {instead} instead: it waits until "
                "the input box is clear, keeps ordering, and reports back. Pass "
                "hold_while_typing=false if you really must type over a draft.")
        if any(a.startswith("-") and a in ("--remote", "--session") for a in argv):
            raise DunkError("launching or attaching a herdr session is not allowed from here")
        if self_pane_id and "self" in argv:
            # `self` means the caller's tab for tab commands, its workspace
            # for workspace commands, and its pane otherwise.
            own_id = self_pane_id
            if argv[0] in ("tab", "workspace"):
                own = self.herdr.get_pane(self_pane_id) or {}
                own_id = own.get(f"{argv[0]}_id") or self_pane_id
            argv = [own_id if a == "self" else a for a in argv]
        code, out, err = self.herdr._run(argv, timeout=30)
        payload = None
        for stream in (out, err):  # herdr prints JSON errors on stderr
            if stream.strip():
                try:
                    payload = json.loads(stream)
                    break
                except ValueError:
                    payload = None
        error = None
        text = out if out.strip() else err
        if isinstance(payload, dict) and isinstance(payload.get("error"), dict):
            error = payload["error"].get("message") or payload["error"].get("code")
        elif code != 0 and not _looks_like_usage(text):
            error = err.strip() or out.strip() or f"herdr exited {code}"
        result = payload.get("result") if isinstance(payload, dict) and "result" in payload \
            else payload
        log.info("herdr %s -> %s", " ".join(argv), "ok" if not error else f"error: {error}")
        return {"ok": error is None, "argv": argv, "result": result if error is None else None,
                "text": text or "", "error": error, "exit_code": code}

    def herdr_help(self, topic=None):
        """herdr's own usage text: the command groups (`topic` empty) or one
        group's subcommands (`topic` = 'pane', 'workspace', 'tab', 'agent',
        'wait', 'notification', ...). This is how an agent discovers what the
        `herdr` passthrough can do; `herdr api schema --json` has the full
        machine-readable schema."""
        if getattr(self.herdr, "name", "herdr") != "herdr":
            raise DunkError(
                "herdr help is not available with the "
                f"{getattr(self.herdr, 'name', 'unknown')} terminal backend")
        topic = str(topic or "").strip().split()
        argv = topic[:1] if topic else ["--help"]
        code, out, err = self.herdr._run(argv, timeout=10)
        text = out if out.strip() else err
        if not text.strip():
            raise DunkError(f"herdr printed no help for {' '.join(argv)!r} (exit {code})")
        return {"topic": " ".join(topic) or None, "text": text}

    def resolve_target(self, spec, self_pane_id=None):
        """Turn a target spec into a pane dict.

        `spec` may be a pane dict, a pane id (`w8:p3`), the word `self` (the
        pane of the calling process), a terminal id, or a case-insensitive
        substring of a tab label, workspace label, agent name, directory or
        terminal title. Exactly one pane must match.
        """
        if isinstance(spec, dict):
            if not spec.get("pane_id"):
                raise DunkError("target dict has no pane_id")
            panes = self.herdr.list_panes_grouped()
            for pane in panes:
                if pane.get("pane_id") == spec["pane_id"]:
                    return pane
            return spec
        spec = str(spec or "").strip()
        if not spec:
            raise DunkError("target is required")
        if spec.lower() in ("self", "me", "here"):
            if not self_pane_id:
                raise DunkError(
                    "target 'self' needs HERDR_PANE_ID (run from inside a herdr pane)"
                )
            spec = self_pane_id
        panes = self.herdr.list_panes_grouped()
        if not panes:
            raise DunkError(self.herdr.server_error_hint())
        for pane in panes:
            if pane.get("pane_id") == spec or pane.get("terminal_id") == spec:
                return pane
        needle = spec.lower()
        # Exact label matches beat substring matches, so a tab named
        # "Dunking Sheep" wins over a pane whose title merely contains it.
        for key in ("tab_label", "label", "workspace_label", "agent"):
            exact = [p for p in panes if str(p.get(key) or "").lower() == needle]
            if len(exact) == 1:
                return exact[0]
            if len(exact) > 1:
                agents = [p for p in exact if p.get("agent")]
                if len(agents) == 1:
                    return agents[0]
                raise AmbiguousTarget(f"target {spec!r} is ambiguous: {_describe(exact)}")
        matches = []
        for pane in panes:
            haystack = " ".join(
                str(pane.get(key) or "")
                for key in ("tab_label", "workspace_label", "agent", "cwd",
                            "terminal_title_stripped", "label")
            ).lower()
            if needle in haystack:
                matches.append(pane)
        if len(matches) == 1:
            return matches[0]
        if not matches:
            raise DunkError(f"no herdr pane matches {spec!r}")
        # Prefer a single agent pane among several matches (a tab label often
        # matches both the agent pane and a shell beside it).
        agents = [p for p in matches if p.get("agent")]
        if len(agents) == 1:
            return agents[0]
        raise AmbiguousTarget(f"target {spec!r} is ambiguous: {_describe(matches)}")

    # -- mutation -----------------------------------------------------------

    def add(self, target=None, text=DEFAULT_TEXT, interval_minutes=DEFAULT_INTERVAL_MINUTES,
            name=None, start=False, only_when=None, max_sends=None,
            skip_if_unconsumed=False, hold_while_typing=True, self_pane_id=None):
        interval = parse_interval(interval_minutes)
        gate_given = only_when is not None
        only_when = _check_only_when(only_when)
        max_sends = _check_max_sends(max_sends)
        pane = self.resolve_target(target, self_pane_id) if target else None
        with self._lock:
            dunker = Dunker(
                f"d{self.next_id}",
                name=name,
                interval_minutes=interval,
                text=text if text is not None else DEFAULT_TEXT,
                only_when=only_when,
                max_sends=max_sends,
                skip_if_unconsumed=_check_bool(skip_if_unconsumed, "skip_if_unconsumed"),
                hold_while_typing=_check_bool(hold_while_typing, "hold_while_typing"),
            )
            self.next_id += 1
            if pane:
                dunker.set_target(pane)
                dunker.status = "Target set"
                if not gate_given and pane.get("agent"):
                    # Typing into an agent mid-turn can abort that turn, which
                    # is silent and costs the whole turn's work. A schedule
                    # aimed at an agent therefore waits for it to be idle
                    # unless the caller says otherwise (only_when="any").
                    dunker.only_when = "idle"
            self.dunkers.append(dunker)
            if start and pane:
                self._start(dunker)
            elif start:
                dunker.status = "No target - press c"
            self._save()
            self.last_message = f"Added {dunker.id}"
            return dunker.to_dict()

    def update(self, dunk_id, target=None, text=None, interval_minutes=None,
               name=None, only_when=..., max_sends=..., skip_if_unconsumed=None,
               hold_while_typing=None, self_pane_id=None):
        """Change fields on a dunk. Pass `only_when=None` / `max_sends=None`
        explicitly to clear them; omit them to leave them alone."""
        pane = self.resolve_target(target, self_pane_id) if target else None
        interval = parse_interval(interval_minutes) if interval_minutes is not None else None
        if only_when is not ...:
            only_when = _check_only_when(only_when)
        if max_sends is not ...:
            max_sends = _check_max_sends(max_sends)
        with self._lock:
            dunker = self._find(dunk_id)
            changed = []
            if pane:
                dunker.set_target(pane)
                changed.append("target")
            if text is not None:
                dunker.text = str(text)
                changed.append("text")
            if interval is not None and interval != dunker.interval_minutes:
                dunker.interval_minutes = interval
                if dunker.running:
                    anchor = dunker.last_sent_at or dunker.started_at or time.time()
                    dunker.next_send_at = max(time.time() + 1, anchor + interval * 60)
                changed.append("interval")
            if name is not None:
                dunker.name = str(name)
                changed.append("name")
            if only_when is not ...:
                dunker.only_when = only_when
                changed.append("only_when")
            if max_sends is not ...:
                dunker.max_sends = max_sends
                changed.append("max_sends")
            if skip_if_unconsumed is not None:
                dunker.skip_if_unconsumed = _check_bool(skip_if_unconsumed, "skip_if_unconsumed")
                changed.append("skip_if_unconsumed")
            if hold_while_typing is not None:
                dunker.hold_while_typing = _check_bool(hold_while_typing, "hold_while_typing")
                changed.append("hold_while_typing")
            if changed:
                dunker.status = f"{changed[-1].replace('_', ' ').capitalize()} set"
                self._save()
            return dunker.to_dict()

    def remove(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._halt(dunker)
            dunker.running = False
            self.dunkers.remove(dunker)
            self._save()
            self.last_message = f"Removed {dunker.id}"
            return dunker.to_dict()

    def start(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._start(dunker)
            self._save()
            return dunker.to_dict()

    def stop(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            self._halt(dunker)
            dunker.running = False
            dunker.next_send_at = None
            dunker.status = "Stopped"
            self._save()
            return dunker.to_dict()

    def toggle(self, dunk_id):
        with self._lock:
            dunker = self._find(dunk_id)
            return self.stop(dunk_id) if dunker.running else self.start(dunk_id)

    def stop_all(self):
        with self._lock:
            for dunker in self.dunkers:
                if dunker.running:
                    self._halt(dunker)
                    dunker.running = False
                    dunker.next_send_at = None
                    dunker.status = "Stopped"
            self._save()
            return [d.to_dict() for d in self.dunkers]

    def _start(self, dunker):
        if not dunker.target_pane_id:
            dunker.status = "No target - press c"
            raise DunkError(f"{dunker.id} has no target")
        if dunker.running and dunker.thread and dunker.thread.is_alive():
            return
        now = time.time()
        dunker.running = True
        dunker.started_at = now
        dunker.next_send_at = now + dunker.interval_minutes * 60
        dunker.last_error = None
        self._spawn_timer(dunker)

    def _spawn_timer(self, dunker):
        dunker.stop_event = threading.Event()
        dunker.thread = threading.Thread(
            target=self._timer_loop,
            args=(dunker, dunker.stop_event),
            name=f"dunk-{dunker.id}",
            daemon=True,
        )
        dunker.thread.start()

    def _halt(self, dunker):
        """Stop the timer thread without touching the persisted running flag."""
        if dunker.stop_event:
            dunker.stop_event.set()
        dunker.stop_event = None
        dunker.thread = None

    # -- sending ------------------------------------------------------------

    def fire(self, dunk_id, countdown_s=0, wait=True):
        """Send a dunk's text right now (the classic 't' test send). Does not
        disturb the running countdown."""
        with self._lock:
            dunker = self._find(dunk_id)
            if not dunker.target_pane_id:
                dunker.status = "No target - press c"
                raise DunkError(f"{dunker.id} has no target")
        if wait:
            return self._fire_worker(dunker, countdown_s)
        threading.Thread(
            target=self._fire_worker, args=(dunker, countdown_s), daemon=True
        ).start()
        return {"id": dunker.id, "ok": None, "message": "test send scheduled"}

    def _fire_worker(self, dunker, countdown_s):
        try:
            for remaining in range(int(countdown_s), 0, -1):
                dunker.status = f"Test in {remaining}..."
                time.sleep(1)
            if dunker.hold_while_typing and not self._clear_to_send(
                    dunker.target_pane_id, dunker.last_sent_text, dunker.awaiting_consumption):
                # Same rule as a scheduled send: never type over a human. The
                # send waits in the pane's outbox and lands when the box clears.
                message = self._new_message(
                    dunker.target_pane_id, dunker.target_label(), dunker.text,
                    source=dunker.id, deliver=lambda: self._do_send(dunker))
                dunker.status = "Queued (typing)"
                self._enqueue(message)
                return {"id": dunker.id, "ok": None, "queued": True, "message_id": message.id,
                        "message": f"queued: the box in {dunker.target_label()} would not clear"}
            dunker.status = "Sending..."
            sent, ok, message = self.send_guarded(
                dunker.target_pane_id, lambda: self._do_send(dunker),
                dunker.last_sent_text, dunker.awaiting_consumption,
                hold_while_typing=dunker.hold_while_typing)
            if not sent:
                queued = self._new_message(
                    dunker.target_pane_id, dunker.target_label(), dunker.text,
                    source=dunker.id, deliver=lambda: self._do_send(dunker))
                dunker.status = "Queued (typing)"
                self._enqueue(queued)
                return {"id": dunker.id, "ok": None, "queued": True,
                        "message_id": queued.id,
                        "message": f"queued: the box in {dunker.target_label()} would not clear"}
            stamp = time.strftime("%H:%M:%S")
            dunker.status = f"Tested {stamp}" if ok else "Test failed"
            return {"id": dunker.id, "ok": ok, "queued": False, "message": message}
        except Exception as error:  # noqa: BLE001 - keep the thread alive
            dunker.status = "Test failed"
            dunker.last_error = str(error)
            log.exception("test send failed for %s", dunker.id)
            return {"id": dunker.id, "ok": False, "message": str(error)}

    def send_now(self, target, text, self_pane_id=None, hold_while_typing=True, wait_s=0,
                 press_enter=True, is_notice=False, notify_for=None, source="send_text"):
        """One-off send to any pane, no dunk involved. Like a dunk, it never
        types over a human: if someone is composing in the target's box (or
        earlier messages to that pane are still waiting), the message is queued
        in the daemon and delivered, in order, once the box has been clear for
        TYPING_SETTLE_S. `wait_s` > 0 blocks up to that long for delivery
        before returning, so a short hold still comes back as sent."""
        pane = self.resolve_target(target, self_pane_id)
        if not str(text or "").strip():
            raise DunkError("text is required")
        label = pane.get("tab_label") or pane["pane_id"]
        message = self._new_message(
            pane["pane_id"], label, str(text), source=source,
            deliver=lambda: self.submit_text(pane["pane_id"], message.text,
                                             press_enter=press_enter))
        message.is_notice = is_notice or str(text).startswith(NOTICE_PREFIX)
        if notify_for:
            message.notify_for.append(notify_for)
        if not _check_bool(hold_while_typing, "hold_while_typing"):
            message.hold_while_typing = False
            self._deliver(message)
            return message.to_dict()
        self._enqueue(message)
        if wait_s:
            message.done.wait(min(float(wait_s), 3600))
        return message.to_dict()

    def send_keys_now(self, target, keys, self_pane_id=None, hold_while_typing=True,
                      wait_s=0):
        """Press keys in a pane, queued behind a human who is typing exactly
        like a message. `pane send-keys <pane> Enter` is how a half-written
        sentence gets submitted by accident, so it takes the same path."""
        pane = self.resolve_target(target, self_pane_id)
        keys = [keys] if isinstance(keys, str) else [str(k) for k in keys]
        keys = [k for k in keys if str(k).strip()]
        if not keys:
            raise DunkError("keys are required, e.g. 'Enter' or 'ctrl+c'")
        label = pane.get("tab_label") or pane["pane_id"]
        message = self._new_message(pane["pane_id"], label, " ".join(keys), source="send_keys",
                                    deliver=lambda: self.press_keys(pane["pane_id"], keys))
        message.kind = "keys"
        if not _check_bool(hold_while_typing, "hold_while_typing"):
            message.hold_while_typing = False
            self._deliver(message)
            return message.to_dict()
        self._enqueue(message)
        if wait_s:
            message.done.wait(min(float(wait_s), 3600))
        return message.to_dict()

    # -- direct-message outbox ---------------------------------------------

    def _new_message(self, pane_id, label, text, source, deliver):
        with self._lock:
            message = Message(f"m{self.next_message_id}", pane_id, label, text, source, deliver)
            self.next_message_id += 1
        return message

    def _deliver(self, message):
        """Send one message, re-checking the box under the send lock. Returns
        False when a draft appeared at the last moment and the message must
        stay queued."""
        with self._lock:
            if message.status == "cancelled":
                return True
        sent, ok, detail = self.send_guarded(
            message.pane_id, message.deliver,
            hold_while_typing=message.hold_while_typing,
            allow_modal=(message.kind == "keys"),
            preserve_draft=getattr(message, "preserve_draft", False),
            permission=getattr(message, "permission", None))
        if not sent:
            with self._lock:
                message.holds += 1
                message.detail = detail
            self.metrics.record("message.held", pane_id=message.pane_id,
                                mailbox=message.target_label, source=message.source,
                                outcome="held", reason=self._last_block_reason.get(
                                    message.pane_id))
            return False
        with self._lock:
            message.status = "sent" if ok else "failed"
            message.detail = detail
            message.finished_at = time.time()
            self.last_message = (f"Sent to {message.target_label}" if ok
                                 else f"Send failed: {detail}")
            self._remember(message)
        # PERSIST ON THE WAY OUT TOO, not only on the way in (t249). _enqueue() saves when a
        # message becomes held; nothing saved when it stopped being held, so dunks.json kept the
        # last snapshot and a restart re-queued messages that had already been SENT - typed a
        # second time under their old id, which is exactly what _restore_outbox's docstring
        # forbids. Measured 2026-10-26: m6361 had been delivered and was still in the file.
        self._save()
        self.metrics.record(
            "message.sent" if ok else "message.failed",
            pane_id=message.pane_id, mailbox=message.target_label,
            source=message.source, outcome="ok" if ok else "failed",
            reason=None if ok else "unconfirmed",
            duration_ms=int((message.finished_at - message.created_at) * 1000),
            attempts=message.holds or None,
            bytes=len((message.text or "").encode("utf-8")))
        for inbox_id in list(message.notify_for):
            # The inbox record said "queued" when the notice was created; say
            # what actually happened, so a status query is not stale. After
            # shutdown the store may already be closed; delivery bookkeeping
            # must never crash a worker or race a tempdir cleanup.
            try:
                if not self.closed:
                    self.inbox.record_notify(inbox_id, "sent" if ok else "failed", detail)
            except Exception:  # noqa: BLE001 - bookkeeping only
                pass
        log.info("message %s -> %s (%s): %s", message.id, message.pane_id, message.source,
                 "sent" if ok else f"FAILED {detail}")
        message.done.set()
        return True

    def _remember(self, message):
        self.message_history.append(message)
        del self.message_history[:-MESSAGE_HISTORY]
        # Keep the verdict where the NEXT process can find it, not only where this one can - and
        # keep the message's CREATED_AT with it (reviewer, PR #43): message ids are reused across
        # daemons (the live log has m6379 sent on 2026-10-15 to wN:p1 and again on 2026-10-26 to
        # %4), so an id alone is a claim about a NUMBER, not about a message. A verdict that
        # refused a brand-new message on an id match would be a lost message that looks like a
        # clean restore - the opposite of the defect this fixes.
        self.decided[message.id] = {"status": getattr(message, "status", None) or "unknown",
                                    "created_at": getattr(message, "created_at", None)}
        while len(self.decided) > DECIDED_KEPT:
            self.decided.pop(next(iter(self.decided)), None)

    def _enqueue(self, message):
        """Queue a message for its pane and make sure a delivery thread runs.
        Delivery is immediate when nothing is queued and nobody is typing."""
        with self._lock:
            queue = self.outbox.setdefault(message.pane_id, [])
            queue.append(message)
            thread = self.outbox_threads.get(message.pane_id)
            if thread is None or not thread.is_alive():
                thread = threading.Thread(target=self._outbox_worker, args=(message.pane_id,),
                                          name=f"outbox-{message.pane_id}", daemon=True)
                self.outbox_threads[message.pane_id] = thread
                thread.start()
        # Persist on enqueue, not only on dunk changes: a restart between a hold
        # and its delivery used to discard the message silently.
        self._save()

    def _unwedge(self, pane_id, content, since):
        """Last resort when a box has held the same text, unchanged, for
        STALE_BOX_S while messages wait behind it.

        Only text we typed is ever submitted this way. Unchanged text we do
        not recognise is far more likely to be a person who started a sentence
        and walked away, and submitting half a thought on their behalf is
        worse than a queue that waits: the wedge is visible in `messages` and
        `flush` resolves it deliberately. Our own stranded text is a different
        matter - it is ours to clean up, and recognising it is what the
        remembered-sends list exists for."""
        if not content or time.time() - since < STALE_BOX_S:
            return False
        if not self._looks_like_ours(pane_id, content):
            if not self._warned_wedged.get(pane_id):
                self._warned_wedged[pane_id] = True
                log.warning("%s: input box has held text we did not type for %ds with "
                            "messages waiting. Not submitting someone's draft; run "
                            "`dunkingsheep flush %s` to release it: %r",
                            pane_id, int(time.time() - since), pane_id, content[:80])
            return False
        self._warned_wedged.pop(pane_id, None)
        log.warning("%s: our own text has sat unsubmitted for %ds with messages "
                    "waiting; submitting it to unwedge the queue: %r",
                    pane_id, int(time.time() - since), content[:80])
        ok, detail = self._press_until_submitted(pane_id, content)
        if not ok:
            log.error("%s: could not unwedge the input box: %s", pane_id, detail)
        return ok

    def _outbox_worker(self, pane_id):
        """Deliver a pane's queued messages in order, each only once the box
        has been clear for TYPING_SETTLE_S; while someone types, wait and
        re-check every MESSAGE_RECHECK_S. A box that never clears at all is
        unwedged rather than waited on for ever."""
        held_since = None
        seen, seen_since = None, time.time()
        try:
            while not self.closed:
                with self._lock:
                    queue = self.outbox.get(pane_id) or []
                    queue[:] = [m for m in queue if m.status == "queued"]
                    message = queue[0] if queue else None
                    if message is None:
                        self.outbox.pop(pane_id, None)
                        return
                # A message sent with the hold bypassed must not be gated here
                # either: `--ignore-typing` means this send does not ask the pane.
                if not message.hold_while_typing or self._clear_to_send(pane_id):
                    if held_since is not None:
                        log.info("message %s -> %s: box clear after %ds, delivering",
                                 message.id, pane_id, int(time.time() - held_since))
                        held_since = None
                    if self._deliver(message):
                        with self._lock:
                            if message in queue:
                                queue.remove(message)
                        seen, seen_since = None, time.time()
                        continue
                    # Refused under the lock. This used to loop straight back,
                    # and when the two checks disagreed - the box holding our
                    # own text that would not submit, which `_clear_to_send`
                    # forgives and `_ensure_box_clear` does not - the worker
                    # span for 27 minutes without delivering or ever reaching
                    # the recovery below. Fall through to it instead.
                    held_since = held_since or time.time()
                elif held_since is None:
                    held_since = time.time()
                    with self._lock:
                        message.holds += 1
                        self.last_message = (f"Holding message for {message.target_label}: "
                                             f"the box would not clear")
                    log.info("message %s -> %s: holding, the box would not clear",
                             message.id, pane_id)
                # Watch what is in the box. While it changes someone is
                # writing and we wait; while it does not, we are wedged.
                signature = self._box_signature(pane_id)
                if signature != seen:
                    seen, seen_since = signature, time.time()
                elif (self._note_stale_panel(pane_id, seen, seen_since)
                      or (seen[0] == "box" and self._unwedge(pane_id, seen[1], seen_since))):
                    seen, seen_since = None, time.time()
                    held_since = None
                    continue
                time.sleep(MESSAGE_RECHECK_S)
        except Exception:  # noqa: BLE001 - never kill the daemon
            if not self.closed:
                log.exception("outbox worker for %s crashed", pane_id)
        finally:
            with self._lock:
                if self.outbox_threads.get(pane_id) is threading.current_thread():
                    self.outbox_threads.pop(pane_id, None)

    def list_messages(self, why_blocked=True):
        """Queued messages (oldest first) and the recent delivered/failed ones.
        A queued message also says what its pane's box is holding, so a wedge
        is visible instead of looking like an idle queue."""
        with self._lock:
            queued = [m.to_dict() for queue in self.outbox.values() for m in queue
                      if m.status == "queued"]
            recent = [m.to_dict() for m in self.message_history]
        queued.sort(key=lambda m: m["created_at"])
        if why_blocked:
            boxes = {}
            for message in queued:
                pane = message["pane_id"]
                if pane not in boxes:
                    content = self._box_text(pane)
                    boxes[pane] = ("" if content is None else content)
                content = boxes[pane]
                message["blocked_by"] = (
                    None if not content
                    else "guarded send pending" if self._looks_like_ours(pane, content)
                    else "input box contains a draft")
        return {"queued": queued, "recent": recent[::-1]}

    def get_message(self, message_id):
        with self._lock:
            for queue in self.outbox.values():
                for message in queue:
                    if message.id == message_id:
                        return message.to_dict()
            for message in self.message_history:
                if message.id == message_id:
                    return message.to_dict()
        raise DunkError(f"no message with id {message_id!r}")

    def cancel_message(self, message_id):
        """Drop a queued message before it is delivered."""
        with self._lock:
            for pane_id, queue in self.outbox.items():
                for message in queue:
                    if message.id == message_id:
                        if message.status != "queued":
                            raise DunkError(f"message {message_id} already {message.status}")
                        message.status = "cancelled"
                        message.finished_at = time.time()
                        queue.remove(message)
                        self._remember(message)
                        message.done.set()
                        log.info("message %s -> %s cancelled", message.id, pane_id)
                        # PERSIST THE CANCELLATION (t255, third instance of the same class): the
                        # outbox is written from memory on enqueue, so a cancelled message stayed in
                        # dunks.json and a restart would restore it - the cancel looked done and was
                        # not durable. Measured 2026-10-27: `cancel-message m862` reported "cancelled"
                        # and the file still held m862.
                        self._save()
                        return message.to_dict()
            for message in self.message_history:
                if message.id == message_id:
                    raise DunkError(f"message {message_id} already {message.status}")
        raise DunkError(f"no queued message with id {message_id!r}")

    def read_pane(self, target, lines=40, self_pane_id=None):
        pane = self.resolve_target(target, self_pane_id)
        text = self.herdr.read_pane(pane["pane_id"], lines=lines)
        if text is None:
            raise DunkError(f"could not read pane {pane['pane_id']}")
        return {"pane_id": pane["pane_id"], "agent": pane.get("agent"),
                "agent_status": pane.get("agent_status"),
                "tab_label": pane.get("tab_label"), "text": text}

    def _do_send(self, dunker):
        text = expand_template(dunker.text, dunker).strip()
        if not text:
            return True, "empty text, nothing sent"
        ok, message = self.submit_text(dunker.target_pane_id, text)
        with self._lock:
            if ok:
                dunker.send_count += 1
                # A delivered send ends the run: the skip counter belongs to the
                # run, not to the dunk's lifetime, or a threshold eventually fires
                # on accumulated history rather than on a current condition.
                dunker.skip_count = 0
                dunker.last_sent_at = time.time()
                dunker.last_error = None
                dunker.last_sent_text = text
                # Backpressure looks at this text in the pane on the next send.
                dunker.awaiting_consumption = True
                dunker.unknown_skips = 0
                if dunker.detector_fallback:
                    dunker.last_error = (
                        "input box not recognised on this pane; sent anyway after "
                        "repeated skips (detector fallback)")
                    dunker.detector_fallback = False
                self.last_message = f"Sent to {dunker.target_label()}"
            else:
                dunker.last_error = message
                self.last_message = f"Send failed: {message}"
        self.metrics.record("dunk.sent", pane_id=dunker.target_pane_id,
                            mailbox=dunker.target_label(), source=dunker.id,
                            outcome="ok" if ok else "failed",
                            bytes=len((text or "").encode("utf-8")))
        log.info("%s -> %s: %s (%s)", dunker.id, dunker.target_pane_id,
                 "sent" if ok else "FAILED", message)
        return ok, message

    def _previous_send_consumed(self, dunker):
        """Decide, at send time, whether the previous send has left the target's
        input box. Returns True (consumed, go ahead) or False (still pending or
        we could not tell, so hold).

        The signal is the pane's visible input box, not the agent's busy state:
        a pane idling at a prompt with unread text reports idle, and a pane busy
        on an earlier turn tells us nothing about the newest send. We read the
        box and treat the send as still pending if it shows a collapsed-input
        placeholder (how Claude Code renders a long queued paste) or the sent
        text verbatim. An unreadable or unrecognisable box is inconclusive, and
        for this opt-in feature the safe answer to "not sure" is to hold: a
        missed nudge is recoverable next interval, a stacked pile-up is not.

        Blind spots, stated honestly: we detect our own queued send (verbatim, or
        the collapse placeholder a long one becomes) but not arbitrary unrelated
        short text a human left in the box, because agents show rotating idle
        hints there that cannot be told apart from typed text - so a short
        unrelated line reads as "clear" and the send proceeds. A short prompt
        whose echo lingers in the box region can hold one extra interval. This
        assumes an agent pane that draws an input box (Claude/Codex), not a bare
        shell."""
        if dunker.send_count == 0 or not dunker.last_sent_text:
            return True  # nothing sent yet; the first send always proceeds
        tail = self.herdr.read_pane(
            dunker.target_pane_id, lines=CONSUMPTION_READ_LINES, source="visible"
        )
        pending = _input_pending(tail, dunker.last_sent_text)
        if pending is None:
            # The pane is unreadable, or this harness's input box is not one we
            # can parse. Skipping for ever silently kills the nudge, so hold a
            # bounded number of times, then send once and surface the fallback.
            with self._lock:
                dunker.unknown_skips += 1
                count = dunker.unknown_skips
                dunker.last_skip_reason = "unrecognised"
            if count < MAX_UNKNOWN_SKIPS:
                log.info("%s: could not read %s input box (%d/%d); holding",
                         dunker.id, dunker.target_pane_id, count, MAX_UNKNOWN_SKIPS)
                return False
            log.warning("%s: input box on %s still unrecognised after %d skips; "
                        "sending once (detector fallback)",
                        dunker.id, dunker.target_pane_id, count)
            with self._lock:
                dunker.unknown_skips = 0
                dunker.detector_fallback = True
            return True
        with self._lock:
            dunker.unknown_skips = 0
            dunker.last_skip_reason = "unconsumed" if pending else None
            if not pending:
                dunker.awaiting_consumption = False
        return not pending

    def read_screen(self, pane_id, lines=None, ansi=True):
        """The one way a component asks the pane what it looks like.

        Three components disagreed with the pane in one day (send believed a claim,
        delivery and the queue asked different questions, flush misread a bordered
        row). The only one that never lied was the one reading the pane, so every
        question about the pane goes through here: one read, one parser, one fixture
        can prove all of them.
        """
        try:
            return self.herdr.read_pane(pane_id, lines=lines or CONSUMPTION_READ_LINES,
                                        source="visible", ansi=ansi)
        except Exception:                   # noqa: BLE001 - unreadable is None, not an error
            return None

    def read_box(self, pane_id):
        """(screen, text, detail) for a pane's input box, from one read."""
        screen = self.read_screen(pane_id)
        if screen is None:
            return None, "", "the pane could not be read"
        text, detail = box_text(screen)
        return screen, text, detail

    def _typing_in(self, pane_id, last_sent_text=None, awaiting_consumption=False):
        """True if someone is composing text in `pane_id`'s input box. See
        `_human_typing` for the reasoning; this is the pane-level form used by
        dunks and by queued direct messages alike."""
        screen = self.read_screen(pane_id)
        if screen is None:
            return False
        if _modal_open(screen):
            # A panel or dialog, not a half-written sentence. Its selected row
            # carries the same sigil as the prompt, which is what made a
            # queued Escape wait for ever behind an open /usage panel.
            return False
        typed = _typed_text(screen)
        if not typed:
            return False
        if _is_our_send(typed, last_sent_text, awaiting_consumption):
            return False
        # Text we typed and failed to submit is ours to clean up, not a draft
        # to wait behind; `_ensure_box_clear` flushes it before the next send.
        return not self._looks_like_ours(pane_id, typed)

    def send_blocker(self, pane_id, last_sent_text=None, awaiting_consumption=False):
        """Why a send cannot go now, as (state, reason), or None if it can.

        One function, because two checks gave two answers: `delivery` said ready
        while the queue held the mail, and there was no way to see which was right.
        Every gate - delivery_state, the queued-message worker, send_text - asks
        this, so they cannot disagree again.
        """
        # POLICY (owner, 2026-10-23): a busy agent is NOT a hold reason. Every
        # harness we use queues a message typed while it is working, so holding it
        # here was solving a problem that did not exist and costing the owner
        # minutes. The only two reasons to refuse are a DEAD session (read, not
        # claimed) and a HUMAN mid-sentence in this box.
        marker = self._dead_session(pane_id)
        if marker:
            return ("dead", f"dead session ({marker})")
        if self._session_process_gone(pane_id):
            return ("dead", "the lane's recorded process is gone")
        screen, text, detail = self.read_box(pane_id)
        if screen is not None and _modal_open(screen):
            # A panel is not a draft. Its selected row sits where a draft would, and
            # keys/Enter are how a panel is dismissed - holding them behind it is how
            # a queued Escape waited for ever.
            return None
        if detail.startswith(AMBIGUOUS_COMPOSER):
            return ("unknown", detail)
        # NO 'A HUMAN IS TYPING' HOLD. Every message the owner sends goes through this
        # daemon (web, phone, Telegram) - none of those paths type into a pane - so a
        # non-empty box is not evidence of a person. Foreign text is PRESERVED at send
        # time (submitted first, or left untouched), never a reason to stop the queue.
        # A harness that does not queue what we type while it works would LOSE the
        # message (codex: measured). Hold for that tool only, and say why.
        harness = self._pane_harness(pane_id)
        if not HARNESS_QUEUES_WHILE_BUSY.get(harness, False):
            # A working indicator sits a few lines above the composer (codex prints
            # "• Working (54m · esc to interrupt)" above its status row), so this one
            # check looks a little wider than the 3-line status window.
            if screen and _status_marker(screen, WORKING_MARKERS, lines=8,
                                         placeholder_safe=True, dim_ok=True):
                return ("blocked", f"{harness or 'this harness'} does not queue text typed "
                                   f"during a turn (measured: notices are lost), so this waits "
                                   f"for the turn to end")
        return None

    def _pane_harness(self, pane_id):
        try:
            pane = self.herdr.get_pane(pane_id) or {}
        except Exception:                       # noqa: BLE001
            return None
        return str(pane.get("agent") or "").lower() or None

    def _clear_to_send(self, pane_id, last_sent_text=None,
                       awaiting_consumption=False, stop_event=None):
        """True once the box has read clear on two checks TYPING_SETTLE_S
        apart, so a pause between thoughts is not mistaken for being done.
        False as soon as either check sees typing, a declared turn is active,
        or the wait is stopped."""
        if self.send_blocker(pane_id, last_sent_text, awaiting_consumption):
            return False
        if stop_event is not None:
            if stop_event.wait(TYPING_SETTLE_S):
                return False
        else:
            time.sleep(TYPING_SETTLE_S)
        # The second read is for REDRAW safety (a pane that repaints mid-send), not for
        # somebody's text: text in the box is preserved at send time now, so it is no
        # longer a reason to return False. A screen that cannot be read is inconclusive
        # and does not hold - the send's own confirmation decides.
        screen = self.read_screen(pane_id)
        if screen is None:
            return True
        return not self._dead_session(pane_id)

    def _box_holds(self, pane_id, text):
        """True if `text` still sits in the pane's input box, False if nothing
        of ours is waiting there, None if the pane could not be read.

        A pane that reads fine but draws no input box is a bare shell: there
        is no box for text to be stranded in, so there is nothing to confirm
        and the send stands. Only a pane we cannot read at all is inconclusive,
        and that is the case the strict rule is for. Conflating the two made
        every send to a shell pane report failure."""
        screen = self.read_screen(pane_id)
        if screen is None:
            return None
        if _modal_open(screen):
            # A panel or dialog is open, so our text left the box and opened
            # it. Pressing Enter again would act on the panel instead.
            return False
        shown = _typed_text(screen)
        if not shown:            # empty box, or no box at all
            # "EMPTY" IS NOT ALWAYS PROOF. When the reader decided the last prompt
            # row is TRANSCRIPT (the footer-adjacency rule), the box may be empty -
            # or it may hold a multi-line draft whose blank lines look like the gap
            # an echo leaves. That distinction is invisible from here, and reporting
            # "sent" on it typed three messages into one composer without submitting
            # them for twenty minutes (VP i24194: 'sent', lane idle since 07:21).
            # So: unknown, which the submit ladder treats as "wait for a turn".
            text_seen, detail = box_text(screen)
            if not text_seen and "transcript" in (detail or ""):
                return None
            return False
        return _is_our_send(shown, text, True)

    def _session_process_gone(self, pane_id):
        """True when the lane's own recorded process is no longer running.

        A second READING for the dead refusal, alongside the screen marker: a status
        field can stick (a pane reported 'ready' all night while its session was
        gone), and a process that is not there cannot be argued with.
        """
        try:
            leases = self.tasks.leases_for_pane(pane_id)
        except Exception:                   # noqa: BLE001
            return False
        for lease in leases:
            pid = lease.get("caller_pid")
            if not pid:
                continue
            from dunk_callers import process_alive
            if not process_alive(int(pid), lease.get("caller_start")):
                return True
        return False

    def _dead_session(self, pane_id):
        """Return the dead-session marker showing at the BOTTOM of the pane, or None.

        Only the tail counts: a live pane whose transcript quotes the phrase is
        not dead, and refusing it would make the guard the outage.
        """
        # A plain read: a banner needs no styling, and the styled read is the one
        # the send path already pays for. Same reader, cheaper form.
        screen = self.read_screen(pane_id, lines=40, ansi=False)
        return _banner_marker(screen, DEAD_SESSION_MARKERS)

    def _flush_once(self, pane_id, text):
        """One Enter, one short check. Used INSIDE the typing lock, so it must be short.

        The full retry loop (four presses, seconds apart) is right for a confirmed send
        but wrong here: held under the global typing lock it delays every other sender,
        which is the stall class the lock exists to prevent. If one press does not clear
        the box, the send is deferred and the outbox worker tries again.
        """
        ok, detail = self.herdr.send_keys(pane_id, "Enter")
        if not ok:
            return False
        time.sleep(SUBMIT_POLL_S[0])
        return self._box_holds(pane_id, text) is False

    def _press_until_submitted(self, pane_id, text, first_pressed=False):
        """Press Enter (each press under the typing mutex) until `text` leaves the box.

        Press Enter until `text` has left the input box. Returns
        (ok, detail). Only ever presses again while our own text is still
        there, so it can never submit something a human typed afterwards."""
        dead = self._dead_session(pane_id)
        if dead:
            # One honest refusal naming the pane's own state, instead of four
            # Enter presses into a dead composer and a misleading "did not submit".
            return False, f"agent session is dead ({dead}); not submitting into a dead composer"
        # t363: the turn evidence must belong to THIS send. A lane that was already
        # working before we sent is running SOMEONE's turn - possibly an earlier
        # message's - so its current "working" proves nothing about ours. Baseline
        # first; only a transition to working during the ladder may confirm.
        pre_status = self._read_status(pane_id)
        key = "Enter"          # what the press inside the mutex used, if any
        for attempt, gap in enumerate(SUBMIT_GAPS, start=1):
            # Let the pane fall quiet first; a burst of Enters does not submit.
            if gap:
                time.sleep(gap)
            if first_pressed and attempt == 1:
                # The first Enter went out with the text, inside the mutex.
                pass
            else:
                # The key that last submitted for this pane wins from then on. Until
                # one has worked: Enter, Enter, then C-m, Enter, C-m ... - so a TUI
                # that ignores Enter is not pressed four times for nothing, and a
                # TUI that wants two Enters still gets them.
                key = (self._submit_key.get(pane_id)
                       or ("C-m" if attempt > SUBMIT_KEY_AFTER
                           and attempt % 2 == 1 else "Enter"))
                with self._typing(pane_id):      # one keystroke burst per attempt
                    ok, detail = self.herdr.send_keys(pane_id, key)
                if not ok:
                    return False, detail
            deadline = time.time() + SUBMIT_CONFIRM_S
            last_hold = None
            for wait in SUBMIT_POLL_S:
                if time.time() >= deadline:
                    break
                time.sleep(wait)
                last_hold = self._box_holds(pane_id, text)
                if last_hold is False:
                    for verify_gap in SUBMIT_VERIFY_GAPS:
                        time.sleep(verify_gap)
                        last_hold = self._box_holds(pane_id, text)
                        if last_hold is not False:
                            break    # it came back, or the pane became unreadable
                    else:
                        # Only repeated readable checks prove submission. A
                        # missing input box during redraw proves nothing.
                        if attempt > 1:
                            self._submit_key[pane_id] = key
                            log.info("%s: submitted after %d presses (%s)",
                                     pane_id, attempt, key)
                        self.metrics.record("submit.attempt", pane_id=pane_id,
                                            outcome="ok", attempts=attempt,
                                            bytes=len((text or "").encode("utf-8")))
                        return True, ("sent" if attempt == 1
                                      else f"sent ({key} x{attempt})")
                    if last_hold is True:
                        # A BUSY LANE'S BOX IS NOT THE RECORD (measured 2026-10-24:
                        # two sends to a claude lane reported "did not submit" and
                        # the receiving lane showed both as turns). If OUR send
                        # STARTED the turn - working now, not working at send start
                        # (t363) - the submit landed and the reader is looking at our
                        # own echo in the transcript.
                        if self._turn_started(pane_id, pre_status):
                            if attempt > 1:
                                self._submit_key[pane_id] = key
                            self.metrics.record("submit.attempt", pane_id=pane_id,
                                                outcome="ok-turn", attempts=attempt,
                                                bytes=len((text or "").encode("utf-8")))
                            return True, "sent (confirmed by the turn it started)"
                        break        # our text returned; another Enter is safe
            if last_hold is not True:
                # UNKNOWN IS NOT FAILURE IF THE LANE STARTED WORKING: the turn is the
                # other witness, and it is the one that survives an ambiguous box read.
                if self._turn_started(pane_id, pre_status):
                    if attempt > 1:
                        self._submit_key[pane_id] = key
                    self.metrics.record("submit.attempt", pane_id=pane_id,
                                        outcome="ok-turn", attempts=attempt,
                                        bytes=len((text or "").encode("utf-8")))
                    return True, "sent (confirmed by the turn it started)"
                # ...AND UNKNOWN IS NOT A REASON TO STOP PRESSING. Returning here
                # after the first attempt is what left nudges TYPED BUT UNSUBMITTED in
                # six idle codex lanes (t130): their composer reads as "transcript" by
                # the footer-adjacency rule, so the box is unknown, the first Enter does
                # nothing, and the ladder gave up before ever trying the carriage
                # return that submits. The retry is the whole point of the ladder.
                log.info("%s: box read is inconclusive after press #%d (%s); trying the "
                         "next key", pane_id, attempt, key)
                continue
            # The box still shows our text. That is only evidence of failure if the
            # lane is NOT working: a busy pane keeps an echo in the transcript.
            if self._turn_started(pane_id, pre_status):
                if attempt > 1:
                    self._submit_key[pane_id] = "Enter"
                self.metrics.record("submit.attempt", pane_id=pane_id,
                                    outcome="ok-turn", attempts=attempt,
                                    bytes=len((text or "").encode("utf-8")))
                return True, "sent (confirmed by the turn it started)"
            log.info("%s: still in the input box after Enter #%d; pressing again",
                     pane_id, attempt)
        if last_hold is None:
            # We pressed every key and the pane never gave a readable answer. Say what
            # we know: the box could not be read, so this is UNCONFIRMED - not "the text
            # is still there", which would be a claim we cannot support.
            return False, (f"submission unconfirmed: input box unreadable or changing "
                           f"after {SUBMIT_ATTEMPTS} presses; the lane may or may not "
                           "have taken it - check the pane before resending")
        if pre_status in ("working", "busy", "running", "processing"):
            # t363: the lane was ALREADY working when this send began, so the turn
            # we can see is not necessarily ours and the busy echo proves nothing
            # either way. UNCONFIRMED - not "did not submit" (a claim the echo
            # cannot support) and not "sent" (a claim the stale turn cannot support).
            return False, (f"submission unconfirmed: the lane was already working "
                           f"before this send ({SUBMIT_ATTEMPTS} presses); status "
                           "alone cannot confirm a new message - check the pane")
        return False, (f"text did not submit after {SUBMIT_ATTEMPTS} Enter presses; "
                       "it is still in the pane's input box")

    def _read_status(self, pane_id):
        """Quiet status read: None on any probe failure (never fails a send)."""
        try:
            return (self.herdr.agent_status(pane_id) or "").strip().lower() or None
        except Exception:                        # noqa: BLE001 - never fail a send on a probe
            return None

    def _turn_started(self, pane_id, pre_status=None):
        """True when a turn STARTED during this send: working now, not working before.

        t363: the second confirmation, for the case the box read cannot decide, is
        only valid as a TRANSITION. A lane that was already working when the send
        began is running some earlier message's turn; confirming ours with it is
        the false delivery that the PR63 review reproduced (status "working",
        composer still holding the full text, sends=[]). A readable pre-send
        baseline that was NOT working is therefore required; an unreadable
        baseline (probe failure, missing pane) cannot confirm either - silence is
        not a baseline."""
        status = self._read_status(pane_id)
        if status not in ("working", "busy", "running", "processing"):
            return False
        if pre_status is None or pre_status in ("working", "busy", "running", "processing"):
            # No readable baseline, or the turn predates this send: stale evidence.
            return False
        return True

    def _remember_typed(self, pane_id, text):
        with self._lock:
            recent = self.sent_text_by_pane.setdefault(pane_id, [])
            if text in recent:
                recent.remove(text)
            recent.append(text)
            del recent[:-REMEMBERED_SENDS]

    def _looks_like_ours(self, pane_id, shown):
        """True if what is in the box is text we typed."""
        if shown and _collapse(shown).startswith(NOTICE_PREFIX.strip()):
            # An inbox notice is unmistakably ours whatever we remember: no
            # person writes this line. Recognising it by shape rather than by
            # memory is what makes it survive a daemon restart, which is how
            # one sat in a PM's composer for ten hours - the restart wiped the
            # record of having typed it, so it became "someone's draft" and
            # the backstop rightly refused to submit a draft.
            return True
        if not shown:
            return False
        # THE DAEMON TRACKS WHAT IT TYPED. Two sources, both exact:
        #  - what it has typed to this pane recently (in memory), and
        #  - what it is TRYING to deliver (the queued outbox text for this pane),
        # so a nudge that was typed but not yet submitted is recognised as ours.
        # A long notice WRAPS, so the box may hold only its first line: the test is
        # prefix-in-either-direction, not equality.
        with self._lock:
            candidates = list(self.sent_text_by_pane.get(pane_id, []))
            candidates += [m.text for m in self.outbox.get(pane_id) or []
                           if m.status == "queued" and m.text]
        shown_c = _collapse(shown)
        for text in candidates:
            full = _collapse(text)
            if not full:
                continue
            if full.startswith(shown_c) or shown_c.startswith(full[:CONSUMPTION_PREFIX_CHARS]):
                return True
        return any(_is_our_send(shown, text, True) for text in candidates)

    def _region_is_ours(self, pane_id, region):
        """t130: STRICT ownership for the ambiguous-frame flush. The complete
        normalized visible composer must EQUAL the complete owned text - a
        human deletion or a short draft (candidate "F", "FEDERATE NOW") is not
        a prefix exemption (CoS i31447): truncation needs explicit proof, not an
        arbitrary prefix. A human continuation or edit below our body breaks the
        equality and the frame is refused."""
        candidate = _collapse(region)
        if not candidate:
            return False
        with self._lock:
            candidates = list(self.sent_text_by_pane.get(pane_id, []))
            candidates += [m.text for m in self.outbox.get(pane_id) or []
                           if m.status == "queued" and m.text]
        for text in candidates:
            if _collapse(text) == candidate:
                return True
        return False

    def _composer_ownership(self, pane_id, screen):
        """(owned, region) for an ambiguous frame - THE shared decision path.

        Used by BOTH _ensure_box_clear and the public composer_region_is_owned
        seam (CoS i31512: the watchdog must not bypass the guards). The complete
        fail-closed rule: the composer region is delimited only when the status
        footer is the LAST non-blank row and EXACTLY ONE prompt-sigil row exists
        above it; ownership is STRICT EQUALITY of the whole region against what
        we typed or are queuing (no prefix, no shape rule, no brightness
        assumption). Any other sigil row - adjacent or blank-separated, a
        transcript echo, a human continuation or edit, an inserted copy of our
        text - makes the boundary undelimitable and the frame is refused with
        zero keys. The sigil-free-transcript wedge positive is the one recovery
        case; sigil'd transcript echoes fail closed (DocumentedLimitTests)."""
        region = _composer_region_text(screen)
        if not region:
            return False, ""
        return self._region_is_ours(pane_id, region), region

    def composer_region_is_owned(self, pane_id, screen):
        """PUBLIC (t377 seam, Owner2282): the exact-owned full-composer check.

        Returns True only when the complete visible composer - extracted with
        the same geometry rules the delivery guard uses - EQUALS, after
        _collapse normalisation of wrapping, text we typed or are queuing for
        `pane_id`. Appended human lines, edits, truncations, foreign drafts and
        nested/adjacent sigil rows are refused. Stable name: the watchdog
        depends on this, not on the private helpers."""
        owned, _region = self._composer_ownership(pane_id, screen)
        return owned

    def _box_text(self, pane_id):
        """Whatever is sitting in the pane's input box: '' when empty, None
        when there is no box to read."""
        screen = self.read_screen(pane_id)
        if screen is None:
            return None
        return _typed_text(screen)

    def press_keys(self, pane_id, keys):
        """Press a sequence, under the global typing mutex (one burst)."""
        with self._typing(pane_id):
            return self._press_keys_unlocked(pane_id, keys)

    def _press_keys_unlocked(self, pane_id, keys):
        """Press keys in a pane. When the last key is Enter and the box holds
        text, the press is confirmed the same way a text send is: Enter does
        not reliably submit, so pressing it once and reporting success was how
        agent-to-agent messages were ending up stranded in the box."""
        keys = [keys] if isinstance(keys, str) else [str(k) for k in keys]
        if not keys or keys[-1] not in ENTER_KEYS:
            return self.herdr.send_keys(pane_id, keys)
        if len(keys) > 1:
            ok, detail = self.herdr.send_keys(pane_id, keys[:-1])
            if not ok:
                return ok, detail
        pending = self._box_text(pane_id)
        if not pending:
            # Nothing waiting: a bare Enter is just a key press.
            return self.herdr.send_keys(pane_id, [keys[-1]])
        return self._press_until_submitted(pane_id, pending)

    def submit_text(self, pane_id, text, press_enter=True, check_box=False):
        """Type `text` into a pane, press Enter, and confirm it left the box.

        The box check, the typing and the first Enter happen inside ONE hold of the
        global typing mutex, so a human who starts typing in that gap cannot have our
        text spliced onto their sentence. Everything that WAITS happens outside the
        mutex: under the guard the confirmation is a single short check (the caller
        re-attempts), and an unguarded call keeps the full careful loop.
        """
        if not hasattr(self, "_box_ctx"):
            self._box_ctx = threading.local()
        guarded = getattr(self._box_ctx, "enabled", False)
        # The guard already checked the box under the same lock; checking again would
        # be a second read of a decision just made.
        check_box = (check_box or guarded) and not getattr(self._box_ctx, "checked", False)
        allow_modal = getattr(self._box_ctx, "allow_modal", False)
        if not getattr(self._box_ctx, "checked", False):
            # A direct caller has not been through the guard: refuse a dead session
            # BEFORE pressing anything (the guard does this itself for its own sends).
            marker = self._dead_session(pane_id)
            if marker:
                return False, (f"agent session is dead ({marker}); "
                               "not submitting into a dead composer")
        pane = next((item for item in self.herdr.list_panes()
                     if item.get("pane_id") == pane_id), None)
        shell = pane is not None and not pane.get("agent")
        with self._typing(pane_id):
            if check_box and not self._ensure_box_clear(pane_id, allow_modal=allow_modal):
                why = self._last_block_reason.get(pane_id) or "draft"
                return False, f"{BOX_HOLD} ({why})"
            ok, detail = self.herdr.send_text(pane_id, text)
            if not ok:
                return False, detail
            self._remember_typed(pane_id, text)
            if not press_enter:
                return True, "sent"
            ok, detail = self.herdr.send_keys(pane_id, "Enter")
            if not ok:
                return False, detail
            if shell:
                # A bare shell has no chat input box to read back.
                return True, "sent"
        if guarded:
            # THE NUDGE PATH IS THIS ONE. Two rounds of "press, wait, check", not one:
            # measured on an idle codex lane (%31) right after 2.70.37 shipped - the text
            # stayed in the composer and the log showed send_text then nothing, because a
            # single extra press landed while the TUI was mid-redraw and was swallowed.
            # The VP's hand-submitted Enter worked because it came seconds later, so the
            # missing ingredient is TIME, not the key name.
            #
            # Still bounded: at most GUARDED_PRESS_ATTEMPTS presses with short waits, then
            # the honest refusal. The guard's contract is "no long blocking"; the queue
            # re-attempts.
            # t363: baseline before any press, so "working" can only confirm a
            # transition - a lane already working is running an earlier turn.
            pre_status = self._read_status(pane_id)
            for attempt in range(GUARDED_PRESS_ATTEMPTS):
                time.sleep(SUBMIT_POLL_S[min(attempt, len(SUBMIT_POLL_S) - 1)])
                held = self._box_holds(pane_id, text)
                if held is False:
                    for verify_gap in GUARDED_VERIFY_GAPS:
                        time.sleep(verify_gap)
                        held = self._box_holds(pane_id, text)
                        if held is not False:
                            break
                if held is False:
                    # ONE LINE PER GUARDED DELIVERY, naming how it went (VP i25315): a
                    # clear box used to leave no trace at all, so the only evidence of a
                    # guarded nudge was the generic "message sent" line.
                    if attempt == 0:
                        log.info("guarded nudge: %s submitted, box stayed clear", pane_id)
                    else:
                        log.info("guarded nudge: %s submitted, %s took at round %d",
                                 pane_id, key, attempt)
                    return True, ("sent" if attempt == 0 else
                                  f"sent (confirmed after {attempt + 1} checks)")
                if self._turn_started(pane_id, pre_status):
                    log.info("guarded nudge: %s submitted, the lane started a turn%s",
                             pane_id, f" after {key} at round {attempt}" if attempt else "")
                    return True, "sent (confirmed by the turn it started)"
                key = self._submit_key.get(pane_id) or ("Enter" if attempt == 0 else "C-m")
                with self._typing(pane_id):
                    ok, _detail = self.herdr.send_keys(pane_id, key)
                if not ok:
                    continue
            # The failure must be VISIBLE from the sender's side (VP i25123/i25315): a
            # recurrence then shows up in the journal as this line, not as an idle lane
            # somebody infers hours later.
            log.warning("guarded nudge: typed, not submitted on %s after %d rounds; the "
                        "text is in the composer and the lane has not started a turn",
                        pane_id, GUARDED_PRESS_ATTEMPTS)
            if pre_status in ("working", "busy", "running", "processing"):
                # t363: stale working state cannot confirm OR condemn this send.
                return False, ("submission unconfirmed: the lane was already working "
                               "before this send; status alone cannot confirm a new "
                               "message - check the pane")
            return False, "text did not submit; it is still in the pane's input box"
        return self._press_until_submitted(pane_id, text, first_pressed=True)

    def flush_box(self, target, self_pane_id=None):
        """Submit whatever is sitting unsent in a pane's input box.

        The daemon flushes its own stranded text automatically, but text left
        by something else (an agent typing through the herdr CLI, an older
        daemon, a person who walked away) looks exactly like a draft, so every
        message to that pane queues behind it for ever. This is the explicit
        "yes, send that" for those cases."""
        pane = self.resolve_target(target, self_pane_id)
        pane_id = pane["pane_id"]
        screen, shown, detail = self.read_box(pane_id)
        if screen is None:
            raise DunkError(f"could not read the input box of {pane_id}")
        label = pane.get("tab_label") or pane_id
        if detail.startswith(AMBIGUOUS_COMPOSER):
            return {"pane_id": pane_id, "target_label": label, "flushed": False,
                    "state": "unknown", "text": None, "detail": detail,
                    "message": f"refused to submit: {detail}"}
        if not shown:
            # Say WHAT was read, not just 'nothing to do': the box that survived two
            # hours did so behind a bare 'already clear'.
            return {"pane_id": pane_id, "target_label": label, "flushed": False,
                    "text": None, "detail": detail,
                    "message": f"nothing to submit: {detail}"}
        ok, detail = self._press_until_submitted(pane_id, shown)
        log.info("flush %s: %s (%s)", pane_id, "submitted" if ok else "FAILED", detail)
        return {"pane_id": pane_id, "target_label": label, "flushed": ok,
                "text": shown, "message": detail}

    def _box_signature(self, pane_id):
        """What the pane is showing, as ("modal"|"box", text). A panel and a
        draft both stop a send, but they are cleared in different ways, so the
        stale-state tracking has to tell them apart."""
        screen = self.read_screen(pane_id)
        if screen is None:
            return ("gone", "")
        if _modal_open(screen):
            return ("modal", _collapse(_strip_ansi(screen))[-120:])
        return ("box", _typed_text(screen) or "")

    def _note_stale_panel(self, pane_id, signature, since):
        """Say once that a dialog is holding up this pane. Never answer it.

        2.9.0 pressed Escape after MODAL_WAIT_S to get a blocked pane moving
        again. That was wrong twice over: a Codex approval prompt asks a person
        to choose, so Escape answers "no" on their behalf, and Escape in a
        working pane interrupts the turn. Combined with a modal check that read
        the transcript as well as the live screen, it fired 1042 times. A
        dialog means the agent is waiting for a person, which is a thing to
        report, not to resolve."""
        kind, _text = signature
        if kind != "modal" or time.time() - since < MODAL_WAIT_S:
            return False
        if not self._warned_modal.get(pane_id):
            self._warned_modal[pane_id] = True
            self.metrics.record("panel.blocking", pane_id=pane_id, outcome="held",
                                reason="dialog",
                                duration_ms=int((time.time() - since) * 1000))
            log.warning("%s: a dialog has been open for %ds with a send waiting. "
                        "It is waiting for a person to answer it; nothing will be "
                        "typed or pressed there until it is.",
                        pane_id, int(time.time() - since))
        return False

    def _draft_persists(self, pane_id):
        """Read the box again after a moment and return what is still there.

        One reading is not enough to refuse a send. An agent redrawing its
        pane can put status text on the prompt line for a frame, and a single
        unlucky sample then looks exactly like a half-written sentence. A
        person's draft is still there a moment later; a redraw is not. The
        settle check has always taken two readings for the same reason, and
        this is the other half of that pair - without it a Codex pane that was
        merely working kept refusing a notification, 14 times in 82 seconds."""
        time.sleep(DRAFT_CONFIRM_S)
        screen = self.read_screen(pane_id)
        if screen is None or _modal_open(screen):
            return None
        return _typed_text(screen) or None

    def resolve_lanes(self):
        """Every pane that has an agent in it: the set the dispatcher may feed.

        A bare shell has no agent_status and is not a lane. `list_panes_grouped`
        carries the labels, so a dispatch line can name the lane it fed.
        """
        try:
            panes = self.herdr.list_panes_grouped()
        except Exception as exc:                # noqa: BLE001 - a backend outage is a refusal
            raise DunkError(f"cannot list panes: {exc}")
        lanes = []
        for pane in panes:
            if not pane.get("pane_id"):
                continue
            lanes.append({"pane_id": pane["pane_id"],
                          # pane_pid lets capture read the session from the PROCESS
                          # instead of guessing from a cwd or trusting a stale launch row.
                          "pane_pid": pane.get("pane_pid"),
                          "agent": pane.get("agent"),
                          "cwd": pane.get("cwd"),
                          "agent_name": (pane.get("agent_name") or pane.get("tab_label")
                                         or pane.get("label") or pane["pane_id"]),
                          "tab_label": pane.get("tab_label"),
                          # The DERIVED status, not the backend's raw field: resolve_lanes feeds
                          # the roster and the dispatch tick, and both must read the same answer
                          # as the CLI (reviewer on t227 - the desk and the dispatcher may not
                          # disagree about what the same turn means).
                          "agent_status": self.lane_status(pane)})
        return lanes

    def delivery_state(self, pane_id, status=None, pane=None, screen=None):
        """Classify whether this pane can receive a message, and why not.

        `status` comes from the pane list (working/idle). The screen decides the
        blocked states: a dead session, a paused goal, a full context, or a draft
        sitting unsubmitted in the box. None of those are "working", and all of
        them starve a lane silently if a dispatcher treats them as idle.
        """
        if status is None and pane is not None:
            status = pane.get("agent_status")
        if status is None:
            # The pane's own report beats our guess: a lane that says it is
            # working is working even if its screen has no composer to read.
            try:
                status = self.herdr.agent_status(pane_id)
            except Exception:                   # noqa: BLE001 - status is optional
                status = None
        if screen is None:
            screen = self.read_screen(pane_id)
        if screen is None:                      # an unreadable pane is unknown, not ready
            return {"state": "unknown", "reason": "pane unreadable"}
        typed = None if _modal_open(screen) else _typed_text(screen)
        state, reason = classify_delivery(screen, status, typed=typed)
        _text, detail = box_text(screen)
        if state in ("ready", "working") and detail.startswith(AMBIGUOUS_COMPOSER):
            state, reason = "unknown", detail
        foreign = bool(typed) and not self._looks_like_ours(pane_id, typed)
        # A hold that the classifier cannot see (a declared turn, a stale busy
        # report) is still a hold: report it as blocked with that reason, so
        # `delivery` and the queue never disagree.
        # `working` is INFORMATION now, not a gate: the harness queues a message
        # typed while it works. `deliverable` is the field a caller must consult, so
        # a state can never be mistaken for permission again.
        deliverable = state in ("ready", "working")
        return {"state": state, "reason": reason, "deliverable": deliverable,
                "foreign_text": foreign,
                "foreign_excerpt": (" ".join(typed.split())[:60] if foreign else None)}

    def clear_box(self, target, self_pane_id=None, reason=None):
        """CLEAR an unsubmitted draft (Ctrl-U, Escape). The destructive pair to
        `flush_box`, which submits what is there.

        Use this when submit has failed - the nudge that would not take - because
        submitting is what we wanted and it did not work. Use `flush_box` when the
        text simply never went in.

        Ctrl-U clears the input line, then Escape leaves any open modal without
        submitting anything. Every flush is logged: clearing someone's draft is a
        destructive act even when the draft is our own text.
        """
        self.require_superuser(self_pane_id, "clear_box")
        pane = self.resolve_target(target, self_pane_id)
        pane_id = pane["pane_id"]
        # A pane record may carry tab_label, label, name or none of them; two
        # spellings of the same lane have already bitten the ack path, so the
        # label is looked up rather than assumed.
        label = (pane.get("tab_label") or pane.get("label") or pane.get("name")
                 or pane.get("agent_name") or pane_id)
        before = self.delivery_state(pane_id)
        if before["state"] == "ready":
            return {"pane_id": pane_id, "target_label": label, "flushed": False,
                    "before": before,
                    "reason": "nothing to clear: the input box is already clear"}
        if before["state"] == "working":
            return {"pane_id": pane_id, "target_label": label, "flushed": False,
                    "before": before,
                    "reason": "the lane is working; a clear would interrupt it"}
        try:
            with self._typing(pane_id):
                self.herdr.send_keys(pane_id, "C-u")
                self.herdr.send_keys(pane_id, "Escape")
        except Exception as exc:
            raise DunkError(f"flush failed for {pane_id}: {exc}")
        after = self.delivery_state(pane_id)
        log.warning("flushed pane %s by %s (%s): %s -> %s", pane_id, self_pane_id,
                    reason or "no reason given", before["state"], after["state"])
        return {"pane_id": pane_id, "target_label": label, "flushed": True,
                "before": before, "after": after, "by": self_pane_id, "reason": reason}

    def _ensure_box_clear(self, pane_id, allow_modal=False, preserve_draft=False):
        """Make the input box ready to type into. Our own text stranded by a
        failed submit is flushed with Enter; a human's draft is left alone; a
        declared active turn holds the send."""
        # Busy is not a reason to wait (owner policy, 2026-10-23): the harness queues
        # what we type while it works. Only a human mid-sentence holds a send.
        screen = self.read_screen(pane_id)
        if screen is None:
            return True                      # unreadable: nothing to go on
        if _modal_open(screen):
            # Typing into an open panel does nothing useful and its keys mean
            # something else, so text waits for it to go. Key presses are how
            # a panel is dismissed, so they go through.
            self._last_block_reason[pane_id] = "dialog"
            return allow_modal
        _text, detail = box_text(screen)
        if detail.startswith(AMBIGUOUS_COMPOSER):
            # t130 (Owner2277): the geometry rule cannot tell pending input from
            # transcript - but CONTENT can, when the pending text is OURS. The
            # decision lives in ONE shared path (_composer_ownership) so the
            # delivery guard and the public watchdog seam cannot drift (CoS
            # i31512): strict full-composer equality, footer = last non-blank
            # row, adjacent/nested sigil rows refuse, duplicate matched echoes
            # refuse.
            owned, region = self._composer_ownership(pane_id, screen)
            if owned:
                self._last_block_reason[pane_id] = "stranded"
                log.info("%s: ambiguous composer holds OUR queued text; submitting it",
                         pane_id)
                if self._flush_once(pane_id, region):
                    return True
                self._last_block_reason[pane_id] = "stranded-will-not-submit"
                return False
            self._last_block_reason[pane_id] = "ambiguous-composer"
            return False
        shown = _typed_text(screen)
        if shown is None or not shown:
            return True
        if self._looks_like_ours(pane_id, shown):
            self._last_block_reason[pane_id] = "stranded"
            log.info("%s: our previous send is stranded in the box; submitting it",
                     pane_id)
            if self._flush_once(pane_id, shown):
                return True
            self._last_block_reason[pane_id] = "stranded-will-not-submit"
            return False
        if preserve_draft:
            self._last_block_reason[pane_id] = "human_draft"
            return False
        # The box holds text we did not type. PRESERVE it: submit it (so it reaches the
        # agent rather than being clobbered), and only then type ours. If it will not
        # submit, leave it exactly as it is and deliver on a later attempt - never
        # overwrite somebody's text and never stall the whole queue behind it.
        log.info("%s: box holds text we did not type (%r); submitting it to preserve it",
                 pane_id, shown[:60])
        if self._flush_once(pane_id, shown):
            return True
        self._last_block_reason[pane_id] = "foreign-text-will-not-submit"
        return False

    def _lock_for(self, pane_id):
        with self._lock:
            lock = self._pane_locks.get(pane_id)
            if lock is None:
                lock = self._pane_locks[pane_id] = threading.Lock()
            return lock

    def send_guarded(self, pane_id, send, last_sent_text=None, awaiting_consumption=False,
                     hold_while_typing=True, allow_modal=False, preserve_draft=False,
                     permission=None):
        """Run `send` (a callable that types into `pane_id`) with the send lock
        held, after one final look at the input box under that same lock.

        The settle check alone is not enough: between `_clear_to_send` returning
        and the keystrokes actually going out, this send has to wait for the
        global `send_lock`, which every other pane's send also holds. On a busy
        flock that gap ran into seconds, and a human who started typing inside
        it got our text spliced onto the end of their sentence (reproduced
        2026-09-23). The final check closes the gap to one pane read.

        Returns (sent, ok, detail): sent=False means a draft appeared and
        nothing was typed."""
        # No lock here: the callable that types (submit_text, press_keys) takes the
        # mutex around its own keystroke burst, with the box check inside it. A lock
        # held out here would cover the submit confirmation, which waits and retries -
        # the first version of this lock held it for 20s on one pane and every other
        # sender timed out.
        # Two locks, two jobs:
        #  - _lock_for(pane_id) serialises WHOLE sends to one pane, so two messages
        #    cannot interleave their bursts into the same box (the per-pane test).
        #  - the global typing mutex inside submit_text serialises each keystroke
        #    burst across the whole fleet (the owner's rule).
        # The dead check here is read-free (a plain read, not the styled one) so the
        # styled-read budget is unchanged; the box check happens inside the burst.
        with self._lock_for(pane_id), self._typing(pane_id):
            if permission is not None:
                refusal = permission()
                if refusal:
                    return False, None, refusal
            if hold_while_typing:
                marker = self._dead_session(pane_id)
                if marker:
                    return False, None, f"dead session ({marker})"
                if self._session_process_gone(pane_id):
                    return False, None, "the lane's recorded process is gone"
                if not self._ensure_box_clear(pane_id, allow_modal=allow_modal,
                                             preserve_draft=preserve_draft):
                    why = self._last_block_reason.get(pane_id) or "draft"
                    return False, None, f"{BOX_HOLD} ({why})"
            try:
                with self._box_check(hold_while_typing, allow_modal, already_checked=True):
                    ok, detail = send()
            except Exception as error:  # noqa: BLE001 - report, never crash a caller
                return True, False, str(error)
            if not ok and str(detail or "").startswith(BOX_HOLD):
                # The box was not ours to type into: that is a HOLD (the message
                # stays queued), not a failed send.
                return False, None, detail
            return True, ok, detail

    @contextlib.contextmanager
    def _box_check(self, enabled, allow_modal=False, already_checked=False):
        """Tell the typing primitive how this send treats the box.

        `enabled` is the hold (a bypassed send does not read the box), `allow_modal`
        is the key-press case: a panel is dismissed by keys, so keys go through it
        while text waits for it.
        """
        if not hasattr(self, "_box_ctx"):
            self._box_ctx = threading.local()
        # A threading.local has no attributes in a thread that never set them: read
        # them with defaults, or the first send in every worker thread raises.
        previous = (getattr(self._box_ctx, "enabled", True),
                    getattr(self._box_ctx, "allow_modal", False))
        self._box_ctx.enabled = enabled
        self._box_ctx.allow_modal = allow_modal
        self._box_ctx.checked = already_checked
        try:
            yield
        finally:
            self._box_ctx.enabled, self._box_ctx.allow_modal = previous
            self._box_ctx.checked = False

    @contextlib.contextmanager
    def _typing(self, pane_id, timeout_s=None):
        """The global typing mutex, held for one keystroke burst."""
        timeout_s = TYPING_LOCK_TIMEOUT_S if timeout_s is None else timeout_s
        started = time.time()
        acquired = self.typing_lock.acquire(timeout=timeout_s)
        waited = time.time() - started
        if not acquired:
            raise DunkError(
                f"the typing lock was held for more than {int(timeout_s)}s by "
                f"{self._typing_owner}; refusing to type concurrently")
        previous, self._typing_owner = self._typing_owner, (pane_id, time.time())
        if waited > TYPING_LOCK_LOUD_S:
            self.typing_contentions += 1
            log.warning("typing lock contended for %dms: holder was %s, waiter %s",
                        int(waited * 1000), previous, pane_id)
        try:
            yield
        finally:
            self._typing_owner = previous
            self.typing_lock.release()

    def _claude_busy(self, pane_id, pane=None):
        """True only when Claude's own session reports status=busy AND that report
        is fresh enough to believe.

        A busy report is a REMEMBERED state from another process: it froze the fleet
        for eight and a half hours on 2026-10-23 while the pane sat with an empty
        prompt and no turn. So the hold is bounded: if the session has said busy for
        longer than CLAUDE_BUSY_TTL_S while this pane's box reads clear and no turn
        is active, the report is stale, the hold is released, and the release is
        logged with both numbers. The pane is the evidence; the status file is a
        claim about it.
        """
        if pane is None:
            try:
                pane = self.herdr.get_pane(pane_id)
            except Exception:  # noqa: BLE001 - never break a send on a status probe
                return False
        if not pane or str(pane.get("agent") or "").lower() != "claude":
            return False
        try:
            busy = self.claude_status.status_for_pane(pane) == "busy"
        except Exception:  # noqa: BLE001
            return False
        if not busy:
            self._claude_busy_since.pop(pane_id, None)
            return False
        # A claim can be WRONG, not merely stale: if it names a session whose
        # process is gone (the dead-pane case), the hold is wrong NOW and waiting
        # for the TTL would be a delay on a known-false claim.
        try:
            session = self.claude_status.session_for_pane(pane)
        except Exception:                   # noqa: BLE001 - a claim we cannot read is not a claim
            session = None
        gone = self._claim_names_a_dead_session(session)
        if gone:
            log.warning("releasing the hold on %s: %s - the claim is wrong, not stale "
                        "(claude reports busy for that cwd)", pane_id, gone)
            self._claude_busy_since.pop(pane_id, None)
            return False
        since = self._claude_busy_since.setdefault(pane_id, time.time())
        held = time.time() - since
        if held <= CLAUDE_BUSY_TTL_S:
            return True
        # Long "busy" report. The TTL is for a claim whose PANE CONTRADICTS it -
        # not for a claim that is merely long. par-e ran a 75-minute byte job with
        # a clear box, and releasing then would have typed into a working agent.
        # So the release needs POSITIVE evidence of idleness, not the absence of a
        # draft: no declared turn, no draft, and the pane must not show that it is
        # offering an interrupt.
        if self.turn_active(pane_id) or self._typing_in(pane_id):
            return True
        if not self._pane_is_quietly_idle(pane_id):
            return True
        log.warning("releasing the hold on %s after %d min of 'busy' (TTL %ds): the "
                    "report is STALE, not wrong - the session is running and still "
                    "says busy, but the pane shows no turn and a clear box",
                    pane_id, held / 60, CLAUDE_BUSY_TTL_S)
        self._claude_busy_since.pop(pane_id, None)
        return False

    def _pane_is_quietly_idle(self, pane_id):
        """True only when the pane itself says nothing is happening.

        The distinction that matters: a claim can be long and TRUE (a 75-minute
        byte job), and releasing a true claim interrupts live work. A draft-free box
        is not evidence of idleness - the pane must report an idle status and must
        not be showing a working indicator or an interrupt hint.
        """
        try:
            pane = self.herdr.get_pane(pane_id) or {}
        except Exception:                   # noqa: BLE001
            return False                    # cannot see it -> do not release
        # 'unknown' is the pane record's way of saying it does not know: ask the
        # status source rather than treating a word as evidence.
        status = str(pane.get("agent_status") or "").lower()
        if status in ("", "unknown"):
            try:
                status = str(self.herdr.agent_status(pane_id) or "").lower()
            except Exception:               # noqa: BLE001
                status = ""
        if status not in ("idle", "done"):
            return False
        screen = self.read_screen(pane_id)
        if screen is None:
            return False
        state, _reason = classify_delivery(screen, "idle",
                                           typed=None if _modal_open(screen) else _typed_text(screen))
        return state == "ready"

    def _claim_names_a_dead_session(self, session):
        """A sentence naming why a busy claim is WRONG rather than stale, or None."""
        if not session:
            return None
        pid = session.get("pid")
        if not pid:
            return None
        try:
            from dunk_callers import process_alive
        except ImportError:                 # the watchdog-style import must never break a send
            return None
        if process_alive(int(pid)):
            return None
        return f"the session named in the claim is gone (pid {pid} is not running)"

    def _human_typing(self, dunker):
        """True if someone is composing text in the target's input box right
        now, so a send would splice our text into the middle of theirs and
        Enter would submit the mangled result.

        Reads the pane *with styling*: both Claude Code and Codex draw their
        empty-box placeholder hints dim (SGR 2) and typed text at full
        intensity, so "non-dim text after the prompt sigil" is a human's draft
        without having to know every rotating hint. Our own leftover send
        (verbatim, or the collapse placeholder a long one becomes while we are
        still awaiting its consumption) is not typing; that is backpressure's
        business. An unreadable pane or one with no input box (a bare shell) is
        inconclusive and does not hold, since this check guards the human's
        draft rather than the agent's queue."""
        return self._typing_in(dunker.target_pane_id, dunker.last_sent_text,
                               dunker.awaiting_consumption)

    def _timer_loop(self, dunker, stop_event):
        panel_seen, panel_since = None, time.time()
        try:
            while not stop_event.is_set():
                with self._lock:
                    total = max(1.0, dunker.interval_minutes * 60)
                    if not dunker.next_send_at:
                        dunker.next_send_at = time.time() + total

                # Countdown, re-reading next_send_at so interval edits apply.
                while True:
                    remaining = dunker.next_send_at - time.time()
                    if remaining <= 0:
                        break
                    minutes, seconds = divmod(int(remaining + 0.999), 60)
                    # Write the countdown under the lock and only while the dunk
                    # is still live. A closed dunk's thread used to write one
                    # more "Next: 60:00" after close_target set "Target closed",
                    # so a stopped dunk could report itself as scheduled again.
                    with self._lock:
                        if stop_event.is_set() or not dunker.running:
                            return
                        dunker.status = f"Next: {minutes:02d}:{seconds:02d}"
                    if stop_event.wait(min(1.0, remaining)):
                        return

                # Gates. First the optional idle gate, then never type over a
                # human. After a typing hold the idle gate runs again, because
                # finishing typing usually means submitting, which makes the
                # agent busy.
                held_since = None
                while True:
                    if dunker.only_when == "idle":
                        while True:
                            agent_status = self.herdr.agent_status(dunker.target_pane_id)
                            if agent_status is None:
                                dunker.status = "Target gone"
                                dunker.last_error = "target pane not found"
                                break
                            if agent_status not in BUSY_STATUSES:
                                break
                            dunker.status = f"Busy ({agent_status})"
                            if idle_poll_wait(stop_event):
                                return
                    if not dunker.hold_while_typing or self._clear_to_send(
                            dunker.target_pane_id, dunker.last_sent_text,
                            dunker.awaiting_consumption, stop_event):
                        break
                    if stop_event.is_set():
                        return
                    if held_since is None:
                        held_since = time.time()
                        with self._lock:
                            dunker.hold_count += 1
                            dunker.last_held_at = held_since
                            self.last_message = (f"Holding {dunker.id}: the box would not clear")
                            self._save()
                        log.info("%s -> %s: holding, the box would not clear",
                                 dunker.id, dunker.target_pane_id)
                    dunker.status = "Held (typing)"
                    if stop_event.wait(TYPING_RECHECK_S):
                        return
                if held_since is not None:
                    log.info("%s -> %s: input box clear after %ds, resuming",
                             dunker.id, dunker.target_pane_id, int(time.time() - held_since))

                # Backpressure: a no-op when the previous send was never taken.
                if dunker.skip_if_unconsumed and not self._previous_send_consumed(dunker):
                    with self._lock:
                        if stop_event.is_set():
                            return
                        dunker.next_send_at = time.time() + max(1.0, dunker.interval_minutes * 60)
                        dunker.skip_count += 1
                        dunker.total_skips += 1
                        dunker.last_skipped_at = time.time()
                        if (dunker.last_error
                                and "target pane not found" in dunker.last_error):
                            # The pane is back; the old error is historical.
                            dunker.last_error = None
                        dunker.status = ("Skipped (unrecognised box)"
                                         if dunker.last_skip_reason == "unrecognised"
                                         else "Skipped (unconsumed)")
                        self.last_message = f"Skipped {dunker.id}: previous send unconsumed"
                        self._save()
                    self.metrics.record("dunk.skipped", pane_id=dunker.target_pane_id,
                                        mailbox=dunker.target_label(), source=dunker.id,
                                        outcome="held", reason=dunker.last_skip_reason)
                    log.info("%s -> %s: skipped, previous send not consumed",
                             dunker.id, dunker.target_pane_id)
                    if stop_event.wait(1.0):
                        return
                    continue

                dunker.status = "Waiting..."
                if stop_event.is_set():
                    return
                dunker.status = "Sending..."
                sent, ok, _message = self.send_guarded(
                    dunker.target_pane_id, lambda: self._do_send(dunker),
                    dunker.last_sent_text, dunker.awaiting_consumption,
                    hold_while_typing=dunker.hold_while_typing)
                if not sent:
                    # They started typing while we waited for the send lock -
                    # or a panel is open, which blocks a send without being a
                    # draft. A dunk used to hold on that for ever (34 times
                    # over 34 minutes on a one-minute interval), because only
                    # the message outbox had any recovery.
                    now = time.time()
                    signature = self._box_signature(dunker.target_pane_id)
                    if signature != panel_seen:
                        panel_seen, panel_since = signature, now
                    elif self._note_stale_panel(dunker.target_pane_id, panel_seen,
                                                   panel_since):
                        panel_seen, panel_since = None, now
                        continue
                    elif (panel_seen[0] == "box"
                          and self._unwedge(dunker.target_pane_id, panel_seen[1],
                                            panel_since)):
                        # Our own text stranded in the box. The outbox has had
                        # this recovery since 2.6.0; a dunk had none, so d39
                        # held on one notice for ten hours.
                        panel_seen, panel_since = None, now
                        continue
                    with self._lock:
                        dunker.hold_count += 1
                        dunker.last_held_at = now
                        dunker.status = "Held (typing)"
                        self._save()
                    self.metrics.record("dunk.held", pane_id=dunker.target_pane_id,
                                        mailbox=dunker.target_label(), source=dunker.id,
                                        outcome="held",
                                        reason=self._last_block_reason.get(
                                            dunker.target_pane_id))
                    if stop_event.wait(TYPING_RECHECK_S):
                        return
                    continue

                with self._lock:
                    if stop_event.is_set():
                        return
                    dunker.next_send_at = time.time() + max(1.0, dunker.interval_minutes * 60)
                    stamp = time.strftime("%H:%M:%S")
                    dunker.status = f"Sent {stamp}" if ok else "Send failed!"
                    done = bool(dunker.max_sends) and dunker.send_count >= dunker.max_sends
                    self._save()
                if done:
                    log.info("%s reached max_sends=%s, removing", dunker.id, dunker.max_sends)
                    self.remove(dunker.id)
                    return
                if stop_event.wait(1.0):
                    return
        except Exception as error:  # noqa: BLE001 - never kill the daemon
            dunker.status = "Error"
            dunker.last_error = str(error)
            log.exception("timer loop for %s crashed", dunker.id)

    # -- lifecycle ----------------------------------------------------------

    def close(self):
        """Halt every timer thread, keeping persisted running flags so a new
        Flock resumes them."""
        self._task_stop.set()
        self._next_job_wake.set()
        if self.memory_ingestor is not None:
            self.memory_ingestor.stop()
        if self._owns_memory and self.memory is not None:
            self.memory.close()
        with self._lock:
            self.closed = True
            for dunker in self.dunkers:
                self._halt(dunker)
            for queue in self.outbox.values():
                for message in queue:
                    message.done.set()
        # F5: let the task workers (and any queued direct-message threads)
        # exit before a caller removes their temp state.
        for thread in getattr(self, "_task_threads", ()):
            thread.join(timeout=2)
        for thread in list(getattr(self, "outbox_threads", {}).values()):
            thread.join(timeout=2)


def _looks_like_usage(text):
    """herdr prints usage for a bare command group (exit non-zero); that is an
    answer, not an error."""
    head = (text or "").lstrip()[:200].lower()
    return head.startswith("usage:") or head.startswith("herdr") and "commands:" in head


def _describe(panes):
    return ", ".join(
        f"{p.get('pane_id')} ({p.get('workspace_label')}/{p.get('tab_label')}"
        f"{' ' + p['agent'] if p.get('agent') else ''})"
        for p in panes
    )


def _numeric_id(dunk_id):
    match = re.fullmatch(r"d(\d+)", str(dunk_id))
    return int(match.group(1)) if match else 0


def _check_only_when(value):
    if value in ("", "none", "None", "any", "always"):
        value = None
    if value not in ONLY_WHEN_CHOICES:
        raise DunkError("only_when must be 'idle' or empty")
    return value


def _collapse(text):
    """Whitespace-insensitive form of text, so wrapped lines still match."""
    return " ".join(str(text).split())


def _is_border_line(line):
    stripped = _strip_ansi(line).strip()
    return stripped.startswith("\u2500") and stripped.count("\u2500") >= 10


def _bordered_input_area(tail):
    """The pi TUI has no prompt sigil: its input is the line(s) between the
    last two full-width box borders, with the cwd/model footer below. Return
    that region (possibly empty) or None if the bottom does not look like a
    bordered box."""
    if not tail:
        return None
    lines = tail.splitlines()
    borders = [index for index, line in enumerate(lines) if _is_border_line(line)]
    if len(borders) < 2:
        return None
    bottom = borders[-1]
    if bottom < len(lines) - 5:  # footer sits below the box
        return None
    top = borders[-2]
    if bottom - top < 1:
        return None
    return "\n".join(lines[top + 1:bottom])


def _input_area(tail):
    """Return the agent's input-box region: everything from the last prompt
    sigil line (Claude ❯, Codex ›) to the end of the visible screen. None if no
    such line is present, meaning we cannot isolate the box. The box sits at the
    very bottom of the screen, so the *last* sigil line starts it; transcript
    text above it (including transcript-collapse chips) is excluded."""
    if not tail:
        return None
    lines = tail.splitlines()
    last = None
    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped[:1] in INPUT_BOX_SIGILS:
            last = index
    if last is None:
        return _bordered_input_area(tail)
    return "\n".join(lines[last:])


def _input_pending(tail, last_sent_text):
    """True if the target's input box still holds unconsumed input, False if it
    is clear, None if we cannot tell (unreadable pane or no recognisable box)."""
    if tail is None:
        return None
    area = _input_area(tail)
    if area is None:
        return None
    low = area.lower()
    if any(marker in low for marker in COLLAPSED_INPUT_MARKERS):
        return True
    needle = _collapse(last_sent_text)[:CONSUMPTION_PREFIX_CHARS].lower()
    if needle and needle in _collapse(area).lower():
        return True
    return False


def _strip_ansi(text):
    """Plain text of an ANSI-styled screen."""
    return ANSI_RE.sub("", text)


def _sgr_dim(params, dim):
    """Apply one SGR parameter list to the current dim state."""
    if not params:
        return False
    index = 0
    while index < len(params):
        code = params[index]
        if code in ("", "0"):
            dim = False
        elif code == "2":
            dim = True
        elif code == "22":
            dim = False
        elif code in ("38", "48", "58"):
            # Extended colour: skip its sub-parameters so "38;2;r;g;b" is not
            # mistaken for dim.
            if index + 1 < len(params) and params[index + 1] == "5":
                index += 1
            elif index + 1 < len(params) and params[index + 1] == "2":
                index += 4
        index += 1
    return dim


def _bright_text(line):
    """The characters of one ANSI-styled line that are not rendered dim."""
    out = []
    dim = False
    position = 0
    for match in ANSI_RE.finditer(line):
        if not dim:
            out.append(line[position:match.start()])
        sequence = match.group(0)
        if sequence.endswith("m"):
            dim = _sgr_dim(sequence[2:-1].split(";"), dim)
        position = match.end()
    if not dim:
        out.append(line[position:])
    return "".join(out)


def _notice_text(mailbox, unread, sender=None, subject=None):
    """The line typed into a pane to say mail has arrived.

    It names the inbox, because an agent may be asked about several, and says
    who it is from and what it is about so the recipient can judge urgency
    without reading first. It stays one short line on purpose: a short send is
    the one that always submits, and the point of the inbox is that the
    content does not have to be typed at all."""
    head = f"{NOTICE_PREFIX}new message for {mailbox}"
    if unread > 1:
        head += f" ({unread} unread)"
    if sender:
        head += f" from {sender}"
    if subject:
        head += f": {str(subject).strip()[:60]}"
    return f"{head}. Read it with: dunkingsheep inbox read"


def _banner_marker(screen, markers):
    """A marker that starts its own line is a banner; one mid-sentence is prose.

    A dead pane prints `■ Server connection could not be restored` as a status
    line. A live pane that merely wrote those words into a message shows them
    inside a sentence. Matching anywhere in the scrollback classified the live
    pane as dead - which is a guard that becomes the outage.
    """
    for row in delivery_rows(screen):
        stripped = row.lstrip(" \t\u2500\u2502\u25a0\u25cf\u2022*\u00b7-\u2014>|#~[]").lower()
        for marker in markers:
            if stripped.startswith(marker):
                return marker
    return None


def _is_placeholder(text):
    """True when a line is the harness's empty-box hint rather than a state."""
    # Strip the box border and the prompt sigil first: the hint is drawn INSIDE the
    # composer ('│ › Ask Codex to do anything'), so an anchored pattern missed it.
    plain = _strip_ansi(text or "").strip().lstrip(BOX_LEADING).strip()
    plain = plain.lstrip("".join(INPUT_BOX_SIGILS)).strip()
    return any(pattern.search(plain) for pattern in PLACEHOLDER_PATTERNS)


def _status_marker(screen, markers, lines=STATUS_TAIL_LINES, *, placeholder_safe=False,
                   dim_ok=False):
    """A status decoration on the composer line: '... · Goal paused (/goal resume)'.

    Only the last few non-blank lines count, because the composer is at the very
    bottom, and only NON-DIM text counts: Claude draws its empty-box hint dim
    ("new task? /clear to save 497.4k tokens"), so a marker found in dim text is
    a placeholder, not a state. Reading the placeholder as a state made a healthy
    lane permanently unreachable while it looked alive - the third variant of
    that failure in one night.
    """
    # Raw rows, NOT delivery_rows: those have the ANSI stripped, and the dim/not-dim
    # distinction is carried by the escapes.
    raw = [row for row in (screen or "").splitlines() if _strip_ansi(row).strip()]
    for marker in markers:
        for row in raw[-lines:]:
            # Dim text is excluded by default: a dim line is usually a hint. But some
            # harnesses draw their WORKING indicator dim (codex prints
            # "• Working (55m · esc to interrupt)" grey), so a caller that is asking
            # "is anything happening" may say dim_ok - a false positive there only
            # delays a message to a harness that would have lost it.
            plain = (_strip_ansi(row) if dim_ok else _strip_ansi(_bright_text(row))).lower()
            if marker not in plain:
                continue
            if placeholder_safe and _is_placeholder(row):
                continue
            return marker
    return None


def delivery_rows(screen):
    """Non-blank rows with colour codes removed.

    A banner starts with an escape sequence before its glyph, so stripping the
    glyph alone left '\x1b[31m■ Server connection...' unparsed and the dead pane
    was classified by its draft instead of its banner.
    """
    plain = _strip_ansi(screen) if screen else ""
    return [row.rstrip() for row in plain.splitlines() if row.strip()]


def delivery_tail(screen, lines=DELIVERY_TAIL_LINES):
    """The last non-blank lines of a pane, lowercased: where state banners live.

    A marker anywhere in the scrollback is not a state. The pane that wrote the
    string "server connection could not be restored" into a file then re-read its
    own transcript was classified dead while it was working.
    """
    rows = [row for row in (screen or "").splitlines() if row.strip()]
    return "\n".join(rows[-lines:]).lower()


def classify_delivery(screen, status=None, *, typed=None, our_text=None,
                      awaiting_consumption=False):
    """Why a pane can or cannot receive a message, as (state, reason).

    States: dead, blocked, working, ready. `blocked` is the one that matters:
    the pane is alive and idle but a send would be held, so a dispatcher must
    count it as unavailable and say why rather than queue behind it.

    Pure so a test can assert on a canned screen without a terminal.
    """
    marker = _banner_marker(screen, DEAD_SESSION_MARKERS)
    if marker:
        return "dead", f"dead session ({marker})"
    marker = _status_marker(screen, PAUSED_GOAL_MARKERS, placeholder_safe=False)
    if marker:
        return "blocked", f"paused goal ({marker}) - it will not act on a message"
    marker = _status_marker(screen, CONTEXT_FULL_MARKERS, placeholder_safe=True)
    if marker:
        return "blocked", f"context full ({marker}) - needs a /clear first"
    progress = _status_marker(screen, WORKING_MARKERS, placeholder_safe=True)
    if progress:
        # The pane is mid-work: it offers an interrupt, so something is running.
        return "working", f"mid-work ({progress})"
    if typed and not _is_our_send(typed, our_text, awaiting_consumption):
        # OWNER POLICY (2026-10-24): 'a human is typing' is not a real state on this
        # path - every message goes through the daemon, and the daemon is the only
        # thing that types into a pane. Text we did not type is PRESERVED at send time
        # (submitted first, or left untouched), never a reason to refuse delivery.
        return "ready", None
    status_text = str(status or "").lower()
    if status_text in ("working", "thinking", "running", "busy"):
        return "working", status_text
    return "ready", None


def _modal_open(screen):
    """True if the pane is showing a panel or dialog *right now*.

    Only the bottom of the screen counts. Scanning the whole thing matched the
    transcript as well as the live interface, so a pane that had shown an
    approval prompt at any point still read as a dialog long after it was
    answered - and that is how the daemon came to press Escape into working
    agents 1042 times."""
    if not screen:
        return False
    lines = [line for line in _strip_ansi(screen).splitlines() if line.strip()]
    low = "\n".join(lines[-MODAL_TAIL_LINES:]).lower()
    return any(marker in low for marker in MODAL_MARKERS)


# The composer lives at the bottom of a pane. Sigil lines above that are old
# prompts in the scrollback, not the box someone is typing into.
# How many rows at the bottom of a pane are searched for the harness's empty-box
# hint. codex draws it as a bare line two rows above the status footer.
COMPOSER_HINT_TAIL_ROWS = 5
BOX_REGION_ROWS = 14
# Stable discriminator for callers: empty text with this detail is NOT empty-box
# evidence and must never authorise a send/flush. Keep the public two-value API.
AMBIGUOUS_COMPOSER = "ambiguous composer/transcript:"
# Box borders and padding sit BEFORE the prompt sigil on a bordered composer, so a
# raw first-character test missed '│ ❯ check your inbox' entirely - which is
# exactly the row a stuck draft sits on.
BOX_LEADING = " \t\u2502\u2503\u2506\u250a\u2551|"


def _prompt_body(row):
    """The text after the prompt sigil on one row, or None if the row has no sigil.

    The sigil is detected from the PLAIN row and the text is read from the BRIGHT
    part of it. Codex draws its sigil dim ('\x1b[2m› ') while the draft after it is
    bright, so a brightness-based detector loses the sigil and a plain-text reader
    would also pick up dim placeholder hints - each needs the other.
    """
    plain = _strip_ansi(row).lstrip(BOX_LEADING)
    if plain[:1] not in INPUT_BOX_SIGILS:
        return None
    body = DECORATION_RE.sub(" ", _bright_text(row)).lstrip(BOX_LEADING)
    if body[:1] in INPUT_BOX_SIGILS:
        body = body[1:]
    body = _collapse(body)
    if body and _is_placeholder(row):
        # A hint drawn bright is still a hint: the box is empty, and calling it text
        # is how codex's "Ask Codex to do anything" became 'a human is typing here'.
        return ""
    return body


def _composer_region_text(screen):
    """The live composer's visible content, or "" when it cannot be delimited
    (FAIL CLOSED - CoS i31543/i31555: never submit on an undelimited frame).

    Rules, deliberately conservative because a single frame cannot distinguish
    transcript prompt echoes from edited composer content (the JSON read path
    carries no style, so no brightness or colour evidence may be assumed):
    - the status footer must be the LAST non-blank row of the screen AND look
      like one (" · " separator, no prompt sigil). A human line holding a
      middle dot is then NOT a footer - it stays inside the region;
    - exactly ONE prompt-sigil row may exist above that footer: it is the
      composer. A second sigil row - a transcript echo, a human continuation
      with a sigil, an inserted copy of our text - is indistinguishable from
      edited composer content, so the frame fails closed;
    - the region is the sigil row's body plus every non-blank row below it, up
      to the footer. LIMIT (documented, not hidden): a real transcript echo of
      a previously submitted prompt makes the frame a refusal too - those
      frames need a renderer ground truth that does not exist yet.
    """
    if not screen:
        return ""
    raw = screen.splitlines()
    nonblank = [index for index, row in enumerate(raw)
                if _strip_ansi(row).strip()]
    if not nonblank:
        return ""
    last_row = nonblank[-1]
    last_plain = _strip_ansi(raw[last_row])
    if " \u00b7 " not in last_plain or _prompt_body(raw[last_row]) is not None:
        return ""          # the last visible row is not a status footer
    sigils = [index for index, row in enumerate(raw)
              if _prompt_body(row) is not None and index < last_row]
    if len(sigils) != 1:
        return ""          # zero or several sigil candidates: fail closed
    composer = sigils[0]
    parts = []
    body = _prompt_body(raw[composer])
    if body:
        parts.append(body)
    for row in raw[composer + 1:last_row]:
        text = _strip_ansi(row).strip()
        if text:
            parts.append(text)
    return " ".join(parts)


def box_text(screen):
    """What is in the input box, and how we know, as (text, detail).

    ONE reader, because three components have now answered 'is there text in the
    box?' from something other than the box. Two rules learned from the failures:

    - scan every sigil line in the composer region, not just the LAST one: a draft
      can sit on an earlier line while the bottom line is an empty prompt, and
      reading only the last one reported 'already clear' over a real draft;
    - return a sentence describing what was read, so a caller that finds nothing
      can say *what it looked at* instead of 'already clear'. A checker that reports
      'nothing to do' without its evidence is how a stuck box survives two hours.

    Empty text with detail starting AMBIGUOUS_COMPOSER is an unknown boundary,
    not an empty-box assertion; guarded callers must refuse keys in that case.
    """
    if not screen:
        return "", "the pane could not be read"
    rows = [row for row in screen.splitlines() if _strip_ansi(row).strip()]
    region_start = max(0, len(rows) - BOX_REGION_ROWS)
    region_end = len(rows)
    sigils = [(index, row) for index, row in enumerate(rows)
              if _prompt_body(row) is not None]
    # Where the "the box is empty" hint can be drawn: the tail of the pane.
    hint_from = max(0, len(rows) - COMPOSER_HINT_TAIL_ROWS)

    # Footer proximity identifies the contiguous composer in these captures:
    #
    #   idle echo frame      last prompt 19, footer 22, 2 blanks  -> ambiguous
    #   transcript+hint      last prompt 15, footer 22, 2 blanks  -> ambiguous
    #   busy lane            last prompt 20, footer 23, 1 blank   -> composer (hint)
    #   working + draft x3   last prompt 34, footer 38, 1 blank   -> composer (draft)
    #   idle + draft x3      last prompt 34, footer 38, 1 blank   -> composer (draft)
    #
    # Two blank rows do NOT prove emptiness: pending text and human drafts can
    # have the same geometry as transcript. Keep that case ambiguous (not a
    # draft guess or permission to press keys). Only an explicit placeholder
    # without other visible content in that region establishes an empty box.
    # BLANK ROWS MATTER HERE, and `rows` has them filtered out - so this rule reads
    # the raw lines. Getting that wrong made the rule silently unreachable: every
    # count came out 0 and every echo still read as a draft.
    raw_rows = screen.splitlines()
    raw_prompts = [(index, row) for index, row in enumerate(raw_rows)
                   if _prompt_body(row) is not None]
    footers = [index for index, row in enumerate(raw_rows)
               if " \u00b7 " in _strip_ansi(row) and _prompt_body(row) is None]
    # BORDERED COMPOSERS ARE NOT THIS RULE'S BUSINESS. claude draws a box and keeps
    # an empty prompt row at the bottom with the draft above it; that fixture must
    # keep reading as a draft, and the footer rule would call it empty. codex draws
    # no border, which is why the rule keys on the absence of one.
    bordered_tail = any(_is_border_line(row) for row in raw_rows[-6:])
    if (not bordered_tail and raw_prompts and footers
            and footers[-1] > raw_prompts[-1][0]):
        last_prompt_index, last_prompt_row = raw_prompts[-1]
        footer_index = footers[-1]
        blanks = sum(1 for index in range(last_prompt_index + 1, footer_index)
                     if not _strip_ansi(raw_rows[index]).strip())
        if blanks >= 2:
            other_content = any(
                _strip_ansi(raw_rows[index]).strip()
                for index in range(last_prompt_index + 1, footer_index))
            if _is_placeholder(last_prompt_row) and not other_content:
                return "", (f"the composer at row {last_prompt_index + 1} holds only "
                            "the explicit harness hint, so the box is empty")
            return "", (f"{AMBIGUOUS_COMPOSER} prompt row {last_prompt_index + 1} "
                        f"is separated from footer row {footer_index + 1} by "
                        f"{blanks} blank rows; visible prompt content may be pending "
                        "input or transcript, so no keys are authorised")
        body = _prompt_body(last_prompt_row)
        if body:
            return body, (f"composer: the prompt row at {last_prompt_index + 1} is "
                          f"contiguous with the status footer ({footer_index + 1})")
        return "", (f"the composer at row {last_prompt_index + 1} holds only the "
                    "harness hint, so the box is empty")
    # Submitted prompts use the same sigil as drafts. When the live composer
    # has borders, use those boundaries rather than including nearby transcript
    # in the last 14 rows. Keep EVERY prompt inside it: a multiline draft can
    # end in an empty prompt, which is not evidence the preceding text was sent.
    borders = [index for index, row in enumerate(rows) if _is_border_line(row)]
    if len(borders) >= 2:
        top, bottom = borders[-2:]
        if bottom >= len(rows) - 5 and any(top < index < bottom for index, _ in sigils):
            region_start, region_end = top + 1, bottom
    in_region = [(index, row) for index, row in sigils
                 if region_start <= index < region_end]
    if not in_region:
        return "", f"no input box found (no prompt sigil in the last {BOX_REGION_ROWS} rows of {len(rows)})"
    # A PLACEHOLDER ON THE LAST PROMPT ROW MEANS THE BOX IS EMPTY, and every earlier
    # prompt-marked line is transcript. This is the BUSY codex frame (pane %17,
    # 06:35): the composer is the last prompt row holding 'Ask Codex to do anything'
    # with the status line under it, while an inbox nudge echoed into the transcript
    # above - so the hint-below-the-candidate rule of 2.70.21 missed it and a plain
    # send still queued as "someone is typing" (m1977). A BLANK last prompt row is
    # NOT a placeholder, so a draft above a trailing empty prompt keeps its meaning.
    last_index, last_row = in_region[-1]
    if _is_placeholder(last_row):
        return "", (f"the last prompt row (row {last_index + 1} of {len(rows)}) holds "
                    f"only the harness hint "
                    f"('{_collapse(_strip_ansi(last_row))[:40]}'), so the box is empty "
                    "and the earlier prompt lines are transcript")
    candidates = []
    for index, row in in_region:
        body = _prompt_body(row)
        if body:
            candidates.append((index, body))
    if candidates:
        # A HINT UNDER THE CANDIDATE, WITH NO PROMPT BELOW IT, MEANS THE TEXT ABOVE
        # IS TRANSCRIPT. codex echoes submitted messages into the transcript with the
        # same '›' sigil its composer uses and draws a BARE hint ('tab to queue
        # message') where a draft would be; the reader took a wrapped line of the
        # previous instruction for a live draft and held every send as "someone is
        # typing" (pane %17, m1843/m1844, 5+ minutes on a ready lane). The condition
        # matters: a real draft above a TRAILING EMPTY PROMPT still has that prompt
        # row under it, which is the fixture this repository requires to survive.
        first_index = candidates[0][0]
        hint_rows = [index for index, row in enumerate(rows[hint_from:], start=hint_from)
                     if _is_placeholder(row)]
        # AND the hint is not directly under it: the hint lives IN the composer, one
        # row below the line being typed. A hint two or more rows below the candidate
        # means the candidate is somewhere else on screen (codex's transcript).
        near_hint = [index for index in hint_rows if index - first_index >= 2]
        if near_hint and not any(first_index < index for index, _ in sigils):
            return "", (f"the composer hint on row {near_hint[0] + 1} of {len(rows)} "
                        f"('{_collapse(_strip_ansi(rows[near_hint[0]]))[:40]}') sits below "
                        f"the text on row {first_index + 1} with no prompt after it, so "
                        "that text is transcript, not a draft")
    if candidates:
        # The draft STARTS at the first prompt line that carries text. A later prompt
        # line with text is a continuation (a pasted multi-line draft draws a sigil on
        # every line), not a new box: taking the LAST one reported the tail of a
        # multi-line draft as if it were the whole thing.
        index, text = candidates[0]
        continuations = [body for _i, body in candidates[1:]]
        below = sum(1 for row in rows[index + 1:region_end]
                    if _strip_ansi(row).strip() and _prompt_body(row) is None)
        extra = []
        if continuations:
            extra.append(f"+{len(continuations)} prompt line(s) below it "
                         f"({continuations[0][:40]!r}...)")
        if below:
            extra.append(f"+{below} indented line(s)")
        return text, (f"read {len(in_region)} prompt line(s); text starts on row "
                      f"{index + 1} of {len(rows)}: {text[:60]!r}"
                      + ((" " + ", ".join(extra)) if extra else ""))
    index, _row = in_region[-1]
    hint = " (only the harness's own hint was there)" if any(
        _is_placeholder(row) for _i, row in in_region) else ""
    return "", (f"read {len(in_region)} prompt line(s); the last is at row {index + 1} "
                f"of {len(rows)} with nothing after the sigil{hint}")


def _typed_text(screen):
    """What a human has typed into the target's input box, judged from an
    ANSI-styled screen: the non-dim text on the last prompt-sigil line, minus
    the sigil and box decoration. '' when the box is empty or shows only a dim
    placeholder hint; None when no input box can be found. Only the sigil line
    is inspected: any draft starts there, and continuation lines are hard to
    tell from an agent's footer."""
    if not screen:
        return None
    rows = [row for row in screen.splitlines() if _strip_ansi(row).strip()]
    if not any(_prompt_body(row) is not None
               for row in rows[max(0, len(rows) - BOX_REGION_ROWS):]):
        return None
    text, _detail = box_text(screen)
    return text


def _is_our_send(typed, last_sent_text, awaiting_consumption):
    """True if the text in the box is our own previous send rather than a
    human's draft: the sent text (the visible line is a prefix of it, or starts
    with its prefix), or a collapsed-paste placeholder while our last send is
    still unaccounted for."""
    low = _collapse(typed).lower()
    if last_sent_text:
        full = _collapse(last_sent_text).lower()
        needle = full[:CONSUMPTION_PREFIX_CHARS]
        if low and (full.startswith(low) or low.startswith(needle)):
            return True
    if awaiting_consumption and any(marker in low for marker in COLLAPSED_INPUT_MARKERS):
        return True
    return False


def _check_bool(value, name):
    if isinstance(value, bool):
        return value
    if value in (None, "", 0, 1):
        return bool(value)
    text = str(value).strip().lower()
    if text in ("1", "true", "yes", "y", "on"):
        return True
    if text in ("0", "false", "no", "n", "off"):
        return False
    raise DunkError(f"{name} must be true or false")


def _check_max_sends(value):
    if value in (None, "", 0, "0"):
        return None
    try:
        number = int(value)
    except (TypeError, ValueError):
        raise DunkError("max_sends must be a positive integer") from None
    if number <= 0:
        raise DunkError("max_sends must be a positive integer")
    return number
