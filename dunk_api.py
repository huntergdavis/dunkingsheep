#!/usr/bin/env python3
"""
The Dunking Sheep command registry.

One table of named commands drives all three remote control surfaces:

- the unix-socket API (`dunk_server.py`) dispatches `{"cmd": ..., "args": ...}`
  requests through `dispatch()`;
- the command-line tool (`dunkingsheep`) maps its subcommands onto these names;
- the MCP server (`dunk_mcp.py`) publishes every command with `mcp=True` as a
  tool, generating the JSON schema from `params`.

Each command's handler receives the `Flock`, the argument dict and the caller's
own herdr pane id (so `target: "self"` means the *caller's* pane, not the
daemon's). Handlers return JSON-serialisable data or raise `DunkError`.
"""

from dunk_core import DEFAULT_INTERVAL_MINUTES, DEFAULT_TEXT, DunkError, PANE_ID_RE
from dunk_tasks import PRIORITIES as TASK_PRIORITIES, STATUSES as TASK_STATUSES

# A direct send waits briefly for actual delivery before reporting back, so
# the common case (nobody typing) answers "sent" rather than "queued".
DEFAULT_SEND_WAIT_S = 3.0

TARGET_DOC = (
    "Which herdr pane. Accepts a pane id like 'w8:p3', the word 'self' for "
    "the pane this agent is running in, a terminal id, or a unique "
    "case-insensitive substring of a tab label, workspace label, agent name "
    "or directory (an exact label wins over a substring). Use list_panes to "
    "see candidates."
)
TEXT_DOC = (
    "Text to type into the pane, followed by Enter. Placeholders {id}, "
    "{name}, {count}, {target}, {interval}, {time} and {date} are expanded at "
    "send time, so a dunk can tell its recipient which dunk it came from "
    "(e.g. '... when the backlog is done, remove dunk {id}')."
)


class Command:
    def __init__(self, name, description, handler, params=None, required=(),
                 mcp=True, mcp_required=None):
        self.name = name
        self.description = description
        self.handler = handler
        self.params = params or {}
        # `required` is enforced for every caller; `mcp_required` is what the
        # tool schema advertises (agents are asked to be explicit about more).
        self.required = list(required)
        self.mcp_required = list(mcp_required) if mcp_required is not None else self.required
        self.mcp = mcp

    def input_schema(self):
        return {
            "type": "object",
            "properties": {key: dict(value) for key, value in self.params.items()},
            "required": list(self.mcp_required),
            "additionalProperties": False,
        }


def _opt(args, key, default=None):
    value = args.get(key, default)
    return default if value is None else value


def _bool(value, default):
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("1", "true", "yes", "y", "on")


# -- task params --------------------------------------------------------------
TASK_ID_PARAM = {"type": "string", "description": "Task id, e.g. t12."}
TASK_SCOPE_PARAM = {
    "type": "string",
    "description": "'global' for the fleet queue, a pane id for a private queue, "
                   "or 'self' for the caller's pane.",
}
TASK_SCOPE_ADD_PARAM = {
    "type": "string",
    "description": "'global' (default, fleet-wide), 'self' (private to the "
                   "caller's pane), or a pane id.",
}
TASK_SCOPE_NEXT_PARAM = {
    "type": "string",
    "description": "'global' (default), 'self' (own pane queue), 'auto' (pane "
                   "then global), or a pane id.",
}
TASK_TITLE_PARAM = {"type": "string", "description": "Short task title."}
TASK_BODY_PARAM = {
    "type": "string",
    "description": "Longer description or acceptance notes.",
}
TASK_STATUS_PARAM = {
    "type": "string",
    "enum": list(TASK_STATUSES),
    "description": "open, in_progress, blocked, done or cancelled.",
}
TASK_PRIORITY_PARAM = {
    "type": "string",
    "enum": list(TASK_PRIORITIES),
    "description": "urgent, high, medium or low (default medium).",
}
TASK_ASSIGNEES_PARAM = {
    "type": "array",
    "items": {"type": "string"},
    "description": "Pane ids or agent names. 'self' means the caller's pane.",
}
TASK_PARENT_PARAM = {"type": "string", "description": "Parent task id, for a subtask."}
TASK_DEPS_PARAM = {
    "type": "array",
    "items": {"type": "string"},
    "description": "Task ids that must be done before this one is eligible.",
}
TASK_LEASE_PARAM = {
    "type": "number",
    "minimum": 1,
    "description": "Lease length in seconds (default 1800).",
}
SUPERUSER_ENTRY_PARAM = {
    "type": "string",
    "description": "A pane id (e.g. w9:p8) or a name matching a pane's "
                   "label/tab label (e.g. 'VP of agentry').",
}
SUPERUSER_NAME_PARAM = {
    "type": "string",
    "description": "Name to re-assert (default 'VP of agentry').",
}
MEMORY_QUERY_PARAM = {"type": "string", "description": "Memory search text."}
MEMORY_LIMIT_PARAM = {"type": "integer", "minimum": 1, "maximum": 50,
                      "description": "Maximum results (default 10)."}
MEMORY_HARNESS_PARAM = {"type": "string",
                        "enum": ["claude", "codex", "opencode", "pi", "generic"],
                        "description": "Only this harness's sessions."}
MEMORY_PROJECT_PARAM = {"type": "string", "description": "Only this project label."}
MEMORY_SINCE_PARAM = {"type": "number",
                      "description": "Only chunks updated at/after this unix timestamp."}
MEMORY_PATH_PARAM = {"type": "string",
                     "description": "Session JSONL file or sqlite DB to ingest."}


# -- handlers -----------------------------------------------------------------

def h_status(flock, args, self_pane_id):
    # The cheap top of status is the cached view; the expensive availability
    # probe stays opt-in (check_herdr) and `full=true` keeps the old answer.
    if not _bool(args.get("full"), False):
        view = flock.view(self_pane_id, fresh=_bool(args.get("fresh"), False))
        result = dict(view["status"])
        if _bool(args.get("check_herdr"), False):
            result["herdr_available"] = flock.herdr.is_available()
        return result
    result = flock.status()
    result["self_pane_id"] = self_pane_id
    return result


def h_turn_start(flock, args, self_pane_id):
    return flock.turn_start(args.get("pane"), ttl_s=args.get("ttl_s"),
                            source=args.get("source"),
                            self_pane_id=self_pane_id)


def h_turn_end(flock, args, self_pane_id):
    return flock.turn_end(args.get("pane"),
                          reason=args.get("reason") or "turn_end",
                          self_pane_id=self_pane_id)


def h_list_dunks(flock, args, self_pane_id):
    return {"dunks": flock.view(self_pane_id,
                                fresh=_bool(args.get("fresh"), False))["dunks"]}


def h_poll(flock, args, self_pane_id):
    """One batched read for sidebars: status + inbox + dunks.

    Read-only: it does not mark mail read. `panes`/`tasks` are opt-in because
    listing panes is the expensive part; the client asks for them only when it
    needs them. A missing self pane is reported in `error`, not raised, so the
    status/dunks half still renders.
    """
    status = flock.poll_status()
    status["self_pane_id"] = self_pane_id
    result = {"status": status}
    try:
        result["inbox_count"] = flock.inbox_count(
            args.get("mailbox"), self_pane_id=self_pane_id)
        listed = flock.inbox_list(
            args.get("mailbox"), unread_only=True,
            limit=int(_opt(args, "limit", 5)), self_pane_id=self_pane_id)
        result["dunks"] = {"dunks": flock.list(),
                           "mailbox": listed["mailbox"],
                           "messages": listed["messages"]}
    except DunkError as error:
        result["error"] = str(error)
        result["dunks"] = {"dunks": flock.list(), "messages": []}
    if _bool(args.get("panes"), False):
        result["panes"] = h_list_panes(flock, args, self_pane_id)
    if _bool(args.get("tasks"), False):
        result["tasks"] = flock.task_list(
            scope=args.get("task_scope"), assignee=args.get("assignee"),
            include_done=_bool(args.get("include_done"), False),
            limit=int(_opt(args, "task_limit", 50)), self_pane_id=self_pane_id)
    return result


def h_get_dunk(flock, args, self_pane_id):
    return flock.get(args["id"])


def h_add_dunk(flock, args, self_pane_id):
    return flock.add(
        target=args.get("target"),
        text=_opt(args, "text", DEFAULT_TEXT),
        interval_minutes=_opt(args, "interval_minutes", DEFAULT_INTERVAL_MINUTES),
        name=args.get("name"),
        start=_bool(args.get("start"), True),
        only_when=args.get("only_when"),
        max_sends=args.get("max_sends"),
        skip_if_unconsumed=_bool(args.get("skip_if_unconsumed"), False),
        hold_while_typing=_bool(args.get("hold_while_typing"), True),
        self_pane_id=self_pane_id,
    )


def h_update_dunk(flock, args, self_pane_id):
    kwargs = {}
    for key in ("target", "text", "interval_minutes", "name"):
        if key in args and args[key] is not None:
            kwargs[key] = args[key]
    # These two may be cleared by passing null / empty explicitly.
    if "only_when" in args:
        kwargs["only_when"] = args["only_when"]
    if "max_sends" in args:
        kwargs["max_sends"] = args["max_sends"]
    if args.get("skip_if_unconsumed") is not None:
        kwargs["skip_if_unconsumed"] = _bool(args["skip_if_unconsumed"], False)
    if args.get("hold_while_typing") is not None:
        kwargs["hold_while_typing"] = _bool(args["hold_while_typing"], True)
    if not kwargs:
        raise DunkError("nothing to update")
    return flock.update(args["id"], self_pane_id=self_pane_id, **kwargs)


def h_remove_dunk(flock, args, self_pane_id):
    return flock.remove(args["id"])


def h_start_dunk(flock, args, self_pane_id):
    return flock.start(args["id"])


def h_stop_dunk(flock, args, self_pane_id):
    return flock.stop(args["id"])


def h_toggle_dunk(flock, args, self_pane_id):
    return flock.toggle(args["id"])


def h_stop_all(flock, args, self_pane_id):
    return {"dunks": flock.stop_all()}


def h_fire_dunk(flock, args, self_pane_id):
    return flock.fire(
        args["id"],
        countdown_s=int(_opt(args, "countdown_s", 0)),
        wait=_bool(args.get("wait"), True),
    )


def h_send_text(flock, args, self_pane_id):
    flock.require_send_self(args["target"], self_pane_id, "send_text")
    return flock.send_now(args["target"], args["text"], self_pane_id=self_pane_id,
                          hold_while_typing=_bool(args.get("hold_while_typing"), True),
                          wait_s=float(_opt(args, "wait_s", DEFAULT_SEND_WAIT_S)),
                          press_enter=_bool(args.get("press_enter"), True))


def h_send_keys(flock, args, self_pane_id):
    flock.require_send_self(args["target"], self_pane_id, "send_keys")
    return flock.send_keys_now(args["target"], args["keys"], self_pane_id=self_pane_id,
                               hold_while_typing=_bool(args.get("hold_while_typing"), True),
                               wait_s=float(_opt(args, "wait_s", DEFAULT_SEND_WAIT_S)))


def h_flush_box(flock, args, self_pane_id):
    flock.require_send_self(args["target"], self_pane_id, "flush_box")
    return flock.flush_box(args["target"], self_pane_id=self_pane_id)


def h_retire_pane(flock, args, self_pane_id):
    """Retire a pane: stop feeding it, stop paging about it, keep its mail.

    Superuser-gated because it silences an alert, and the reason is required
    because an unexplained silence is how an alert dies without anyone learning.
    """
    import dunk_panes
    action = args.get("action") or "list"
    if action == "list":
        retired = dunk_panes.all_retired()
        return {"retired": retired, "count": len(retired)}
    if action == "unretire":
        return dunk_panes.unretire(args["pane"])
    if action != "retire":
        raise ValueError("retire action must be retire, unretire or list")
    flock.require_superuser(self_pane_id, "retire_pane")
    labels = []
    try:
        pane = flock.resolve_target(args["pane"], self_pane_id)
        labels = [value for value in (pane.get("tab_label"), pane.get("agent_name"),
                                      pane.get("label"), pane.get("name")) if value]
        pane_id = pane["pane_id"]
    except Exception:                       # a pane that is already gone is still recordable
        pane_id = str(args["pane"])
    # Every mailbox this pane owns, by its pane_id column: a name like
    # 'crackdown-codex' outlives the pane and is what the alert and the archive key on.
    boxes = []
    try:
        with flock.inbox._lock:
            boxes = [row[0] for row in flock.inbox.db.execute(
                "SELECT DISTINCT mailbox FROM messages WHERE pane_id = ? AND mailbox IS NOT NULL",
                (pane_id,))]
    except Exception:                       # noqa: BLE001 - retirement must still record
        boxes = []
    record = dunk_panes.retire(pane_id, args["reason"], by=self_pane_id,
                               labels=labels, mailboxes=boxes)
    # The mail is evidence: report how much is now quiet, and keep every row.
    waiting = (flock.inbox.count(pane_id) or {}).get("unread") if getattr(flock, "inbox", None) else None
    return dict(record, unread_kept=waiting)


def h_state_capture(flock, args, self_pane_id):
    import dunk_state
    return dunk_state.capture(flock, source=args.get("source") or "periodic")


def h_state_latest(flock, args, self_pane_id):
    import dunk_state
    rows = dunk_state.StateStore().latest(pane_id=args.get("pane_id"),
                                          project=args.get("project"),
                                          limit=int(args.get("limit") or 50))
    return {"rows": rows, "count": len(rows)}


def h_owner_push(flock, args, self_pane_id):
    """Owner -> agent delivery with NO inbox row and NO typing.

    The owner's own words: 'there was never a need to wait for agent input to clear,
    you could just send messages directly'. A push is stored for the pane and drained by
    the harness extension, so no typing guard can hold it.
    """
    import dunk_owner
    # A pane id is already the answer: resolving it costs a terminal-backend round
    # trip (~250ms) and the owner's budget is two seconds for the whole path.
    # One seam for direct delivery, with tokenised refusals the desk can render
    # (dead_lane / unhoused_lane / unknown_name) instead of a generic error.
    pane = flock.delivery_target(args["target"], self_pane_id)
    store = dunk_owner.OwnerPush()
    return store.push(pane["pane_id"], args["body"],
                      target=args.get("target"), subject=args.get("subject"))


def h_owner_take(flock, args, self_pane_id):
    """What is waiting for this pane; the extension calls it about once a second."""
    import dunk_owner
    pane_id = args.get("pane_id") or self_pane_id
    if not pane_id:
        raise ValueError("owner_take needs a pane")
    store = dunk_owner.OwnerPush()
    return {"pane_id": pane_id, "messages": store.take(pane_id, by=args.get("by"))}


def h_owner_stats(flock, args, self_pane_id):
    import dunk_owner
    store = dunk_owner.OwnerPush()
    return {"stats": store.stats(), "pending": store.pending()}


def h_roster(flock, args, self_pane_id):
    import dunk_views
    return dunk_views.roster(flock)


def h_message_state(flock, args, self_pane_id):
    import dunk_views
    ids = args.get("message_ids") or []
    if isinstance(ids, str):
        ids = [part.strip() for part in ids.split(",") if part.strip()]
    return dunk_views.message_state(flock, ids)


def h_reply_stats(flock, args, self_pane_id):
    import dunk_views
    boxes = args.get("mailboxes")
    if isinstance(boxes, str):
        boxes = [part.strip() for part in boxes.split(",") if part.strip()]
    return dunk_views.reply_stats(flock, boxes)


def h_project_summary(flock, args, self_pane_id):
    import dunk_views
    return dunk_views.project_summary(flock, args.get("project"), args.get("path"))


def h_dispatch(flock, args, self_pane_id):
    """The deterministic feeder: standby queue + a tick that fills idle lanes.

    action=add/list/remove/tick/status. Nothing here consults a model: the gate
    is `delivery_state` plus an empty mailbox, and the send goes through the same
    guarded nudge a human would use.
    """
    import dunk_dispatch
    action = args.get("action") or "status"
    queue = dunk_dispatch.Queue()
    if action == "add":
        return queue.add(args["brief"], task_id=args.get("task_id"), lane=args.get("lane"))
    if action == "list":
        return {"items": queue.items(), "pending": len(queue.items())}
    if action == "remove":
        return queue.remove(args["id"])
    if action == "status":
        state = dunk_dispatch._load_state()
        return {"state": state, "summary": dunk_dispatch.summary(state) if state else "no tick yet",
                "pending": len(queue.items())}
    if action != "tick":
        raise ValueError("dispatch action must be add, list, remove, tick or status")
    import dunk_panes
    panes = [pane for pane in flock.resolve_lanes()
             if (pane.get("agent_status") not in (None, "unknown"))
             and not dunk_panes.is_retired(pane["pane_id"])]
    inbox = flock.inbox

    def send(pane_id, text):
        result = inbox.send(pane_id, text, subject="standby", sender=self_pane_id)
        return result.get("message_id")

    # Derive from the BOARD, not from a list somebody tops up: a hand-maintained
    # queue ran dry twice while open tasks sat unclaimed.
    tasks = (flock.task_list(include_done=False, limit=500).get("tasks") or [])
    for task in tasks:
        try:
            task["leases"] = flock.tasks.leases_for(task["id"])
        except Exception:                       # noqa: BLE001 - visibility is optional
            task["leases"] = []
    recent = {entry["task_id"] for entry in (dunk_dispatch._load_state().get("history") or [])
              if entry.get("task_id") and entry.get("at", 0) >= __import__("time").time() - 6 * 3600}
    queued_ids = {str(item.get("task_id")) for item in queue.items() if item.get("task_id")}
    derived, coarse = dunk_dispatch.derive_board_items(
        tasks, dispatched_task_ids=(recent | queued_ids))
    state = dunk_dispatch.tick(
        queue, panes, derived=derived,
        # A pane that reports working needs no screen read to be excluded: reading
        # 35 panes at ~0.4 s each is what made a tick exceed the client timeout.
        # Only a lane that might be fed is worth a read.
        delivery=lambda pane: ({"state": "working", "reason": str(pane.get("agent_status"))}
                               if str(pane.get("agent_status") or "").lower() in ("working", "busy")
                               else flock.delivery_state(pane["pane_id"],
                                                         pane.get("agent_status"), pane=pane)),
        unread=lambda pane: (inbox.count(pane["pane_id"]) or {}).get("unread", 0),
        send=send, dry_run=bool(args.get("dry_run")))
    state["retired"] = len(dunk_panes.all_retired())
    state["board_open"] = sum(1 for task in tasks if str(task.get("status")) == "open")
    state["coarse_tasks"] = coarse
    return state


def h_clear_box(flock, args, self_pane_id):
    """Destroy an unsubmitted draft. Superuser only, and every use is logged."""
    return flock.clear_box(args["target"], self_pane_id=self_pane_id,
                           reason=args.get("reason"))


def h_delivery(flock, args, self_pane_id):
    pane = flock.resolve_target(args["target"], self_pane_id)
    return flock.delivery_state(pane["pane_id"], args.get("status"))


def h_list_messages(flock, args, self_pane_id):
    return flock.list_messages()


def h_get_message(flock, args, self_pane_id):
    return flock.get_message(args["message_id"])


def h_cancel_message(flock, args, self_pane_id):
    return flock.cancel_message(args["message_id"])


# -- inboxes ------------------------------------------------------------------

def h_inbox_send(flock, args, self_pane_id):
    attach = args.get("attach")
    if isinstance(attach, str):
        attach = [p for p in attach.split(",") if p.strip()]
    return flock.inbox_send(args["to"], args["body"], subject=args.get("subject"),
                            attach=attach, notify=_bool(args.get("notify"), True),
                            self_pane_id=self_pane_id,
                            system_sender=args.get("system_sender") or None)


def h_inbox_count(flock, args, self_pane_id):
    if args.get("mailbox"):
        return flock.inbox_count(args.get("mailbox"), self_pane_id=self_pane_id)
    view = flock.view(self_pane_id, fresh=_bool(args.get("fresh"), False))
    if "inbox_count" in view:
        return view["inbox_count"]
    return flock.inbox_count(None, self_pane_id=self_pane_id)


def h_pm_digest(flock, args, self_pane_id):
    return flock.pm_digest(args["project"], args.get("payload") or "",
                           to=args.get("to"), self_pane_id=self_pane_id)


def h_pm_routing(flock, args, self_pane_id):
    import dunk_pm
    return {"routing": dunk_pm.load_routing(),
            "registry": str(dunk_pm.ROUTING),
            "vp_pane": dunk_pm.DEFAULT_VP_PANE}


def h_inbox_list(flock, args, self_pane_id):
    return flock.inbox_list(args.get("mailbox"),
                            unread_only=_bool(args.get("unread_only"), True),
                            limit=int(_opt(args, "limit", 50)),
                            self_pane_id=self_pane_id)


def h_inbox_read(flock, args, self_pane_id):
    return flock.inbox_read(args.get("mailbox"), limit=int(_opt(args, "limit", 10)),
                            peek=_bool(args.get("peek"), False),
                            self_pane_id=self_pane_id)


def h_inbox_status(flock, args, self_pane_id):
    return flock.inbox_status(args["message_id"])


def _ids(args):
    ids = args["message_ids"]
    return [i for i in (ids.split(",") if isinstance(ids, str) else ids) if str(i).strip()]


def h_inbox_ack(flock, args, self_pane_id):
    return flock.inbox_ack(_ids(args), mailbox=args.get("mailbox"), self_pane_id=self_pane_id)


def h_inbox_cancel(flock, args, self_pane_id):
    return flock.inbox_cancel(_ids(args))


def h_inbox_delete(flock, args, self_pane_id):
    return flock.inbox_delete(_ids(args))


def _archive_before(value):
    if value in (None, ""):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    text = str(value).strip()
    try:
        return float(text)
    except ValueError:
        pass
    from datetime import datetime
    try:
        return datetime.fromisoformat(text).timestamp()
    except ValueError:
        raise DunkError(
            f"before must be an ISO date/time or a unix timestamp: {value!r}") from None


def h_inbox_archive(flock, args, self_pane_id):
    states = args.get("state", args.get("states"))
    if isinstance(states, str):
        states = [part.strip() for part in states.split(",") if part.strip()]
    return flock.inbox_archive(args["mailbox"], before=_archive_before(args.get("before")),
                               states=states)


def h_inbox_stats(flock, args, self_pane_id):
    return flock.inbox_stats()


def h_inbox_fetch(flock, args, self_pane_id):
    return flock.inbox_fetch(args["attachment_id"], args.get("dest"))


def _task_values(value):
    if value is None:
        return None
    if isinstance(value, list):
        return [str(item).strip() for item in value if str(item).strip()]
    return [part.strip() for part in str(value).split(",") if part.strip()]


def h_task_add(flock, args, self_pane_id):
    return flock.task_add(
        args["title"], scope=args.get("scope"), body=args.get("body"),
        status=args.get("status"), priority=args.get("priority"),
        assignees=_task_values(args.get("assignees")),
        parent_id=args.get("parent_id"),
        depends_on=_task_values(args.get("depends_on")),
        multi_assignee=_bool(args.get("multi_assignee"), False),
        self_pane_id=self_pane_id)


def h_task_list(flock, args, self_pane_id):
    return flock.task_list(scope=args.get("scope"), status=args.get("status"),
                           assignee=args.get("assignee"),
                           include_done=_bool(args.get("include_done"), True),
                           limit=int(_opt(args, "limit", 200)),
                           self_pane_id=self_pane_id)


def h_task_read(flock, args, self_pane_id):
    return flock.task_read(args["id"], self_pane_id=self_pane_id)


def h_task_update(flock, args, self_pane_id):
    fields = {}
    for key in ("title", "body", "status", "priority", "parent_id",
                "block_reason"):
        if key in args:
            fields[key] = args[key]
    if "assignees" in args:
        fields["assignees"] = _task_values(args["assignees"])
    if "depends_on" in args:
        fields["depends_on"] = _task_values(args["depends_on"])
    if "multi_assignee" in args:
        fields["multi_assignee"] = _bool(args["multi_assignee"], False)
    return flock.task_update(args["id"], self_pane_id=self_pane_id, **fields)


def h_task_assign(flock, args, self_pane_id):
    return flock.task_assign(args["id"], _task_values(args["assignees"]),
                             mode=args.get("mode") or "add",
                             self_pane_id=self_pane_id)


def h_task_complete(flock, args, self_pane_id):
    return flock.task_complete(args["id"], by=args.get("by"),
                               self_pane_id=self_pane_id)


def h_task_block(flock, args, self_pane_id):
    return flock.task_block(args["id"], reason=args.get("reason"),
                            self_pane_id=self_pane_id)


def h_task_unblock(flock, args, self_pane_id):
    return flock.task_unblock(args["id"], self_pane_id=self_pane_id)


def h_task_cancel(flock, args, self_pane_id):
    return flock.task_cancel(args["id"], reason=args.get("reason"),
                             self_pane_id=self_pane_id)


def h_task_delete(flock, args, self_pane_id):
    return flock.task_delete(args["id"], self_pane_id=self_pane_id)


def h_task_note(flock, args, self_pane_id):
    return flock.task_note(args["id"], event=args.get("event") or "note",
                           reason=args.get("reason"),
                           self_pane_id=self_pane_id)


def h_task_next(flock, args, self_pane_id):
    return flock.task_next(scope=args.get("scope"),
                           lease_seconds=args.get("lease_seconds"),
                           self_pane_id=self_pane_id)


def h_task_heartbeat(flock, args, self_pane_id):
    return flock.task_heartbeat(args["id"], lease_seconds=args.get("lease_seconds"),
                                self_pane_id=self_pane_id)


def h_task_release(flock, args, self_pane_id):
    return flock.task_release(args["id"], self_pane_id=self_pane_id)


def h_task_follow(flock, args, self_pane_id):
    return flock.task_follow(scope=args.get("scope"), self_pane_id=self_pane_id)


def h_task_unfollow(flock, args, self_pane_id):
    return flock.task_unfollow(self_pane_id=self_pane_id)


def h_task_followers(flock, args, self_pane_id):
    return flock.task_followers()


def h_task_reap(flock, args, self_pane_id):
    return flock.task_reap(self_pane_id=self_pane_id)


def h_task_stats(flock, args, self_pane_id):
    return flock.task_stats(scope=args.get("scope"), self_pane_id=self_pane_id)


def h_task_metrics(flock, args, self_pane_id):
    return flock.task_metrics(
        window_s=float(_opt(args, "window_s", 3600)),
        since=args.get("since"), until=args.get("until"),
        scope=args.get("scope"), bucket=args.get("bucket") or "hour",
        self_pane_id=self_pane_id)


def h_memory_recall(flock, args, self_pane_id):
    return flock.memory_recall(args["query"], limit=int(_opt(args, "limit", 10)),
                               harness=args.get("harness"),
                               project=args.get("project"),
                               since=args.get("since"))


def h_memory_context(flock, args, self_pane_id):
    return flock.memory_context(args["query"],
                                sessions=int(_opt(args, "sessions", 5)))


def h_memory_search(flock, args, self_pane_id):
    return flock.memory_search(args["query"], limit=int(_opt(args, "limit", 10)),
                               harness=args.get("harness"),
                               project=args.get("project"),
                               since=args.get("since"))


def h_memory_stats(flock, args, self_pane_id):
    return flock.memory_stats()


def h_memory_status(flock, args, self_pane_id):
    return flock.memory_status()


def h_memory_backfill(flock, args, self_pane_id):
    return flock.memory_backfill(action=args.get("action") or "start",
                                 limit=args.get("limit"))


def h_memory_ingest(flock, args, self_pane_id):
    return flock.memory_ingest(args["path"], harness=args.get("harness"),
                               project=args.get("project"))


def h_health(flock, args, self_pane_id):
    return flock.health(repair=bool(_opt(args, "repair", False)),
                        deep=bool(_opt(args, "deep", False)))


def h_recover(flock, args, self_pane_id):
    return flock.recover(apply=bool(_opt(args, "apply", False)),
                         record=bool(_opt(args, "record", False)))


def h_doctor(flock, args, self_pane_id):
    return flock.doctor()


def h_metrics(flock, args, self_pane_id):
    return flock.metrics_summary(window_s=float(_opt(args, "window_s", 3600)),
                                 since=args.get("since"), until=args.get("until"))


def h_list_panes(flock, args, self_pane_id):
    panes = flock.list_panes(self_pane_id=self_pane_id)
    keep = ("pane_id", "is_self", "workspace_label", "tab_label", "agent",
            "agent_status", "cwd", "terminal_title_stripped", "terminal_id",
            "workspace_id", "tab_id")
    return {
        "self_pane_id": self_pane_id,
        "panes": [{key: pane.get(key) for key in keep if key in pane} for pane in panes],
    }


def h_read_pane(flock, args, self_pane_id):
    return flock.read_pane(
        args["target"], lines=int(_opt(args, "lines", 40)), self_pane_id=self_pane_id
    )


def h_list_workspaces(flock, args, self_pane_id):
    own = None
    if self_pane_id:
        pane = flock.herdr.get_pane(self_pane_id)
        own = {"workspace_id": pane.get("workspace_id"), "tab_id": pane.get("tab_id")} \
            if pane else None
    return {"self": own, "workspaces": flock.list_workspaces()}


def h_delivery_capability(flock, args, self_pane_id):
    """What the desk may do with this target: push queue or immediate send."""
    return flock.delivery_capability(args["target"], self_pane_id)


def h_lanes(flock, args, self_pane_id):
    action = (args.get("action") or "list").strip().lower()
    if action == "set":
        # Same gate as the lane_set command: a name that decides who receives
        # mail is not something any lane may assert about another.
        flock.require_superuser(self_pane_id, "lane_set")
        return flock.lane_set(args.get("pane"), args.get("mailbox"),
                              self_pane_id=self_pane_id)
    if action != "list":
        raise ValueError("lanes action must be list or set")
    rows = flock.lanes()
    return {"lanes": rows, "count": len(rows)}


def h_lane_set(flock, args, self_pane_id):
    # No KeyError for a parameterless call: the refusal names what is missing.
    return flock.lane_set(args.get("pane"), args.get("mailbox"),
                          self_pane_id=self_pane_id)


def h_create_workspace(flock, args, self_pane_id):
    return flock.create_workspace(
        label=args.get("label"), cwd=args.get("cwd"), command=args.get("command"),
        focus=_bool(args.get("focus"), False), mailbox=args.get("mailbox"),
    )


def h_create_tab(flock, args, self_pane_id):
    return flock.create_tab(
        workspace=args.get("workspace"), label=args.get("label"), cwd=args.get("cwd"),
        command=args.get("command"), focus=_bool(args.get("focus"), False),
        self_pane_id=self_pane_id,
    )


def h_split_pane(flock, args, self_pane_id):
    return flock.split_pane(
        args["target"], direction=_opt(args, "direction", "right"), cwd=args.get("cwd"),
        command=args.get("command"), focus=_bool(args.get("focus"), False),
        self_pane_id=self_pane_id,
    )


def h_start_agent(flock, args, self_pane_id):
    return flock.start_agent(
        args["name"], args["command"], workspace=args.get("workspace"), tab=args.get("tab"),
        split=args.get("split"), cwd=args.get("cwd"), focus=_bool(args.get("focus"), False),
        self_pane_id=self_pane_id,
    )


def h_rename(flock, args, self_pane_id):
    return flock.rename(args["kind"], args["target"], args["label"], self_pane_id=self_pane_id)


def h_focus(flock, args, self_pane_id):
    return flock.focus(args["kind"], args["target"], self_pane_id=self_pane_id)


def h_close(flock, args, self_pane_id):
    return flock.close_target(args["kind"], args["target"],
                              self_pane_id=self_pane_id,
                              force=_bool(args.get("force"), False))


def h_herdr(flock, args, self_pane_id):
    return flock.herdr_command(args["command"], self_pane_id=self_pane_id)


def h_herdr_help(flock, args, self_pane_id):
    return flock.herdr_help(args.get("topic"))


def h_help(flock, args, self_pane_id):
    from dunk_help import guide
    try:
        herdr_overview = flock.herdr_help().get("text")
    except DunkError:
        herdr_overview = None
    return {
        "guide": guide(markdown=_bool(args.get("markdown"), False)),
        "herdr_help": herdr_overview,
        "protocol": {
            "transport": "unix socket, newline-delimited JSON",
            "request": {"id": "any", "cmd": "<command name>", "args": {},
                        "self_pane_id": "optional pane id that 'self' resolves to"},
            "response": {"id": "echoed", "ok": True, "result": {}},
            "error": {"id": "echoed", "ok": False, "error": "message"},
        },
        "commands": [
            {"name": c.name, "description": c.description,
             "input_schema": c.input_schema(), "mcp_tool": c.mcp}
            for c in COMMANDS
        ],
    }


def h_superuser_status(flock, args, self_pane_id):
    return flock.superuser_status(self_pane_id)


def h_superuser_add(flock, args, self_pane_id):
    return flock.superuser_add(args["entry"], self_pane_id)


def h_superuser_remove(flock, args, self_pane_id):
    return flock.superuser_remove(args["entry"], self_pane_id)


def h_superuser_recover(flock, args, self_pane_id):
    return flock.superuser_recover(self_pane_id, name=args.get("name"))


def h_shutdown(flock, args, self_pane_id):
    # The server intercepts this command to exit after replying; here we only
    # apply the optional stop-all.
    if _bool(args.get("stop_all"), False):
        flock.stop_all()
    return {"shutting_down": True, "stopped_all": _bool(args.get("stop_all"), False)}


# -- registry --------------------------------------------------------------------

ID_PARAM = {"type": "string", "description": "Dunk id, e.g. 'd3' (see list_dunks)."}
TARGET_PARAM = {"type": "string", "description": TARGET_DOC}
TEXT_PARAM = {"type": "string", "description": TEXT_DOC}
INTERVAL_PARAM = {
    "type": "number",
    "minimum": 0.01,
    "description": "Minutes between sends, greater than zero (fractions allowed, e.g. 0.5).",
}
NAME_PARAM = {"type": "string", "description": "Optional human label for the dunk."}
ONLY_WHEN_PARAM = {
    "type": "string",
    "enum": ["idle", "any"],
    "description": (
        "'idle' holds each send until herdr reports the target agent as idle "
        "(not working/blocked). 'any' (the default) sends on schedule."
    ),
}
SKIP_IF_UNCONSUMED_PARAM = {
    "type": "boolean",
    "description": (
        "Backpressure: skip a scheduled send (no-op, send_count unchanged) when "
        "the previous send was never picked up by the target, instead of piling "
        "duplicate prompts into an unattended pane. Default false. Recommended "
        "for self-dunks and unattended agents."
    ),
}
HOLD_WHILE_TYPING_PARAM = {
    "type": "boolean",
    "description": (
        "Never type over a human: before each send the daemon reads the "
        "target's input box and, if someone is mid-way through typing there, "
        "holds the send and re-checks every minute until the box is clear. "
        "Default true. Set false only for a pane nobody types in."
    ),
}
MAILBOX_PARAM = {
    "type": "string",
    "description": ("Whose inbox: an agent name, a pane id, a unique label substring, "
                    "or 'self' for your own. Agent names are preferred because they "
                    "survive a pane moving or restarting. A name that matches no live "
                    "pane still works, so mail can be left for an agent that is not "
                    "running."),
}
MESSAGE_ID_PARAM_INBOX = {"type": "string",
                          "description": "Inbox message id, e.g. 'i42', as returned by inbox_send."}
MESSAGE_IDS_PARAM = {"type": "string",
                     "description": "One inbox message id, or several comma separated."}
MESSAGE_ID_PARAM = {"type": "string", "description": "Message id, e.g. 'm3' (see list_messages)."}
LABEL_PARAM = {"type": "string", "description": "Name to give it (how you will target it later)."}
CWD_PARAM = {"type": "string", "description": "Working directory for the new shell (default: herdr's)."}
RUN_COMMAND_PARAM = {
    "type": "string",
    "description": ("Optional command line to type + Enter into the new pane once it "
                    "exists, e.g. 'claude' to start an agent there."),
}
FOCUS_PARAM = {"type": "boolean",
               "description": "Switch the human's view to it (default false: create in the background)."}
WORKSPACE_PARAM = {
    "type": "string",
    "description": ("Workspace id ('w9'), label, unique label substring, or 'self' for the "
                    "caller's own workspace (the default when omitted)."),
}
TAB_PARAM = {
    "type": "string",
    "description": "Tab id ('w9:t2'), label, unique label substring, or 'self' for the caller's tab.",
}
KIND_PARAM = {"type": "string", "enum": ["workspace", "tab", "pane"],
              "description": "What `target` names."}
KIND_TARGET_PARAM = {
    "type": "string",
    "description": ("Id, label, unique label substring or 'self'. For panes the same rules as "
                    "other pane targets apply."),
}
MAX_SENDS_PARAM = {
    "type": "integer",
    "minimum": 0,
    "description": (
        "Remove the dunk automatically after this many sends. 1 makes a "
        "one-shot delayed nudge. 0 (the default) repeats forever."
    ),
}

COMMANDS = [
    Command(
        "owner_push",
        "Deliver an OWNER message to an agent with no inbox row and no typing: it is "
        "pushed for the pane and the harness extension reads it. Use for owner -> CoS.",
        h_owner_push,
        params={"target": {"type": "string"},
                "body": {"type": "string"},
                "subject": {"type": "string"}},
        required=("target", "body")),
    Command(
        "owner_take",
        "Drain the owner messages waiting for a pane (called by the harness extension "
        "about once a second).",
        h_owner_take,
        params={"pane_id": {"type": "string"}, "by": {"type": "string"}}),
    Command(
        "owner_stats",
        "Owner-push counts and delivery latency (p50/p90/max) in the last day.",
        h_owner_stats,
        params={}),
    Command(
        "state_capture",
        "Record one row per agent lane: project, harness, argv, session, model, thinking, "
        "activity and task. Null with a reason where not readable. The capture half of "
        "restartable lanes.",
        h_state_capture,
        params={"source": {"type": "string", "description": "Why this capture ran (default periodic)."}}),
    Command(
        "state_latest",
        "The most recent captured rows, newest first, optionally for one pane or project.",
        h_state_latest,
        params={"pane_id": {"type": "string"}, "project": {"type": "string"},
                "limit": {"type": "integer"}}),
    Command(
        "roster",
        "One row per agent lane for the Command Center: name, harness, model (null with "
        "a reason), state, current task, inbox count and oldest unread, cost, cpu. No "
        "screen reads: safe to poll.",
        h_roster,
        params={}),
    Command(
        "message_state",
        "sent / delivered / seen for one or more messages, with timestamps. 'replied' "
        "is null with a reason: the answer link lives in the COS/VP application.",
        h_message_state,
        params={"message_ids": {"type": "string", "description": "Comma-separated ids."}},
        required=("message_ids",)),
    Command(
        "reply_stats",
        "Per mailbox: how long mail sat before the lane read it - last and median - "
        "from read_at minus created_at.",
        h_reply_stats,
        params={"mailboxes": {"type": "string", "description": "Comma-separated mailboxes (default all)."}}),
    Command(
        "project_summary",
        "Everything a project page needs: ground truths, wiki articles, tasks in "
        "flight, agents on it, and a completed-task timeline. project omitted = global.",
        h_project_summary,
        params={"project": {"type": "string", "description": "Project name, or omit for global."},
                "path": {"type": "string", "description": "Explicit project directory."}}),
    Command(
        "lanes",
        "The lane registry: which pane is which lane, whether that name was "
        "DECLARED (new-workspace --as, lane_set) or inferred from a title, and "
        "whether the lane is alive. action=list (default) or set.",
        h_lanes,
        params={"action": {"type": "string", "enum": ["list", "set"],
                           "description": "list (default) or set"},
                "pane": {"type": "string", "description": "set: the pane id"},
                "mailbox": {"type": "string",
                            "description": "set: the lane name this pane IS"}}),
    Command(
        "lane_set",
        "Declare who a pane IS, after the fact, in the registry's immutable field. "
        "Repairs a lane whose declared name was overwritten by an agent's pane "
        "title. Refuses a pane that does not exist. Superuser-gated.",
        h_lane_set,
        params={"pane": {"type": "string", "description": "the pane id"},
                "mailbox": {"type": "string", "description": "the lane name"}}),
    Command(
        "retire_pane",
        "Retire a pane we deliberately stopped feeding (superseded, or its job "
        "moved to the dispatcher): it stops paging inbox.unread_live and stops "
        "counting as idle/ready, while its mail rows stay as the record. "
        "action=retire/unretire/list. Superuser-gated; a reason is required.",
        h_retire_pane,
        params={"action": {"type": "string", "description": "retire (default), unretire or list."},
                "pane": {"type": "string", "description": "Pane id or name."},
                "reason": {"type": "string",
                           "description": "Why it is retired, e.g. superseded-by:wW:p7."}}),
    Command(
        "dispatch",
        "Deterministic feeding, no model: a standby queue plus a tick that sends "
        "the next item to a lane that is READY and has no unread mail. "
        "action=add/list/remove/tick/status. tick reports idle lanes, dispatched "
        "this hour, and blocked lanes with reasons.",
        h_dispatch,
        params={"action": {"type": "string",
                           "description": "add, list, remove, tick or status (default status)."},
                "brief": {"type": "string", "description": "One-line assignment (add)."},
                "task_id": {"type": "string", "description": "Task this assignment advances (add)."},
                "lane": {"type": "string", "description": "Reserve the item for one lane (add)."},
                "id": {"type": "integer", "description": "Standby item id (remove)."},
                "dry_run": {"type": "boolean", "description": "Plan the tick, send nothing."}}),
    Command(
        "clear_box",
        "DESTRUCTIVE, superuser only: clear (Ctrl-U, Escape) an unsubmitted draft "
        "in a pane's input box. Use when a send could not be submitted - a draft "
        "that stays there blocks every later message to that lane. Use flush_box "
        "to SUBMIT text that simply never went in.",
        h_clear_box,
        params={"target": {"type": "string", "description": "Pane id, name or label."},
                "reason": {"type": "string", "description": "Why this draft is being destroyed (logged)."}},
        required=("target",)),
    Command(
        "delivery",
        "Can this lane receive a message, and if not why: ready / working / "
        "blocked (dead session, paused goal, full context, unsubmitted draft) / unknown.",
        h_delivery,
        params={"target": {"type": "string", "description": "Pane id, name or label."},
                "status": {"type": "string", "description": "The pane's reported status, if known."}},
        required=("target",)),
    Command(
        "help",
        "Explain Dunking Sheep: concepts, placeholders, the meta-dunking recipe, "
        "every command with its schema, and the socket protocol. Call this first "
        "if unsure how the tool works.",
        h_help,
        params={"markdown": {"type": "boolean",
                             "description": "Render the guide as markdown (default plain text)."}},
    ),
    Command(
        "status",
        "Daemon health: version, dunk counts, and whether herdr is reachable.",
        h_status,
        params={"check_herdr": {"type": "boolean",
                                "description": "Probe the terminal backend (slow; default off)."},
                "full": {"type": "boolean",
                         "description": "Use the full status rather than the cached view."},
                "fresh": {"type": "boolean",
                          "description": "Rebuild the cached view before answering."}},
    ),
    Command(
        "turn_start",
        "Declare a busy turn for a pane (pi/claude hooks). Holds sends until "
        "turn_end; the TTL releases the hold and records turn.expired if the "
        "turn-end never arrives.",
        h_turn_start,
        params={"pane": {"type": "string",
                         "description": "Pane id; defaults to the caller's."},
                "ttl_s": {"type": "number",
                          "description": "Override the turn TTL in seconds."},
                "source": {"type": "string",
                           "description": "Hook/extension that declared it."}},
        mcp=False,
    ),
    Command(
        "turn_end",
        "End a declared busy turn and release the hold.",
        h_turn_end,
        params={"pane": {"type": "string",
                         "description": "Pane id; defaults to the caller's."},
                "reason": {"type": "string",
                           "description": "Why the turn ended."}},
        mcp=False,
    ),
    Command(
        "poll",
        "One cheap read for sidebars: status + inbox count + unread messages + "
        "dunks. Read-only (does not mark mail read). panes/tasks are opt-in.",
        h_poll,
        params={
            "mailbox": {"type": "string",
                        "description": "Mailbox to poll; defaults to the caller's."},
            "limit": {"type": "integer", "minimum": 1,
                      "description": "Unread messages to return (default 5)."},
            "panes": {"type": "boolean",
                      "description": "Include the pane list (expensive; startup only)."},
            "tasks": {"type": "boolean",
                      "description": "Include the caller's task queue."},
            "task_scope": {"type": "string", "enum": ["global", "self"],
                           "description": "Task scope when tasks=true."},
            "assignee": {"type": "string",
                         "description": "Only tasks assigned to this pane/agent."},
            "include_done": {"type": "boolean",
                             "description": "Include completed tasks (default false)."},
            "task_limit": {"type": "integer", "minimum": 1,
                           "description": "Maximum tasks (default 50)."},
            "check_herdr": {"type": "boolean",
                            "description": "Probe the terminal backend (default false; slow)."},
        },
        mcp=False,
    ),
    Command(
        "list_dunks",
        "List every dunk with its target, text, interval, running state, "
        "live status, send count, skip count and seconds until the next send.",
        h_list_dunks,
        params={"fresh": {"type": "boolean",
                          "description": "Rebuild the cached view before answering."}},
    ),
    Command(
        "get_dunk",
        "Fetch one dunk by id.",
        h_get_dunk,
        params={"id": ID_PARAM},
        required=["id"],
    ),
    Command(
        "add_dunk",
        "Create a dunk: periodically type `text` + Enter into a herdr pane. "
        "Starts immediately unless start=false. Target 'self' dunks the pane "
        "this agent runs in (meta-dunking). Returns the new dunk including its id.",
        h_add_dunk,
        params={
            "target": TARGET_PARAM,
            "text": TEXT_PARAM,
            "interval_minutes": INTERVAL_PARAM,
            "name": NAME_PARAM,
            "start": {"type": "boolean", "description": "Start right away (default true)."},
            "only_when": ONLY_WHEN_PARAM,
            "max_sends": MAX_SENDS_PARAM,
            "skip_if_unconsumed": SKIP_IF_UNCONSUMED_PARAM,
            "hold_while_typing": HOLD_WHILE_TYPING_PARAM,
        },
        mcp_required=["target", "text", "interval_minutes"],
    ),
    Command(
        "update_dunk",
        "Change a dunk's target, text, interval, name, only_when, max_sends, "
        "skip_if_unconsumed or hold_while_typing. "
        "Changing the interval of a running dunk reschedules its next send.",
        h_update_dunk,
        params={
            "id": ID_PARAM,
            "target": TARGET_PARAM,
            "text": TEXT_PARAM,
            "interval_minutes": INTERVAL_PARAM,
            "name": NAME_PARAM,
            "only_when": ONLY_WHEN_PARAM,
            "max_sends": MAX_SENDS_PARAM,
            "skip_if_unconsumed": SKIP_IF_UNCONSUMED_PARAM,
            "hold_while_typing": HOLD_WHILE_TYPING_PARAM,
        },
        required=["id"],
    ),
    Command(
        "remove_dunk",
        "Delete a dunk (stopping it first). A dunk may remove itself this way "
        "when its job is done.",
        h_remove_dunk,
        params={"id": ID_PARAM},
        required=["id"],
    ),
    Command(
        "start_dunk", "Start a stopped dunk's countdown.", h_start_dunk,
        params={"id": ID_PARAM}, required=["id"],
    ),
    Command(
        "stop_dunk", "Stop a running dunk (keeps its configuration).", h_stop_dunk,
        params={"id": ID_PARAM}, required=["id"],
    ),
    Command(
        "toggle_dunk", "Start the dunk if stopped, stop it if running.", h_toggle_dunk,
        params={"id": ID_PARAM}, required=["id"], mcp=False,
    ),
    Command(
        "stop_all", "Stop every running dunk.", h_stop_all,
    ),
    Command(
        "fire_dunk",
        "Send a dunk's text right now without disturbing its countdown "
        "(the TUI's 't' test send). If someone is typing in the target pane "
        "and the dunk holds while typing, it is queued instead and delivered "
        "when they finish.",
        h_fire_dunk,
        params={
            "id": ID_PARAM,
            "countdown_s": {"type": "integer", "minimum": 0,
                            "description": "Seconds to count down first (default 0)."},
            "wait": {"type": "boolean",
                     "description": "Wait for the send to finish (default true)."},
        },
        required=["id"],
    ),
    Command(
        "send_text",
        "Send a direct message or instruction to a pane: type text + Enter once, "
        "with no dunk created. Use it to talk to another agent (target its pane) "
        "or to inject a command into a shell pane. Like a dunk it never types "
        "over a human: if someone is composing in that pane the message is "
        "queued and delivered, in order, once the box has been clear for a "
        "second. The result says whether it was sent or queued (`message_id`).",
        h_send_text,
        params={"target": TARGET_PARAM,
                "text": {"type": "string",
                         "description": "Text to type, followed by Enter. Sent verbatim (no placeholders)."},
                "hold_while_typing": {
                    "type": "boolean",
                    "description": ("Queue the message instead of typing over a human who is "
                                    "composing in the target's input box (default true). The "
                                    "daemon delivers it, in order, once the box has been clear "
                                    "for a second.")},
                "wait_s": {
                    "type": "number", "minimum": 0, "maximum": 3600,
                    "description": ("Block up to this many seconds for delivery before "
                                    "returning (default 3). 0 returns as soon as it is "
                                    "queued; use list_messages or get_message to follow "
                                    "up on a message still waiting.")},
                "press_enter": {
                    "type": "boolean",
                    "description": ("Press Enter after the text (default true). false "
                                    "leaves it in the box unsubmitted.")}},
        required=["target", "text"],
    ),
    Command(
        "send_keys",
        "Press named keys in a pane (e.g. 'Enter', 'Escape', 'ctrl+c'), queued "
        "behind a human who is typing just like send_text. Use this rather than "
        "the herdr passthrough: a bare Enter sent while someone is mid-sentence "
        "submits their half-written message. An Enter that is meant to submit "
        "text waiting in the box is confirmed and repeated until it does.",
        h_send_keys,
        params={"target": TARGET_PARAM,
                "keys": {"type": "string",
                         "description": "Key name, or several separated by spaces: 'Enter', 'ctrl+c'."},
                "hold_while_typing": {
                    "type": "boolean",
                    "description": "Queue behind a human who is typing (default true)."},
                "wait_s": {"type": "number", "minimum": 0, "maximum": 3600,
                           "description": "Block up to this many seconds for delivery (default 3)."}},
        required=["target", "keys"],
    ),
    Command(
        "flush_box",
        "Submit whatever is sitting unsent in a pane's input box. Use it when "
        "messages to a pane stay queued: something left text in the box (an "
        "agent typing through the herdr CLI, or a person who walked away "
        "mid-sentence), and every message queues behind it. Returns the text "
        "it submitted.",
        h_flush_box,
        params={"target": TARGET_PARAM},
        required=["target"],
    ),
    Command(
        "list_messages",
        "Direct messages waiting to be delivered (someone is typing in the "
        "target pane) and the recently delivered ones. A queued message shows "
        "how long it has waited; dunks fired while a human was typing appear "
        "here too, with the dunk id as `source`.",
        h_list_messages,
    ),
    Command(
        "get_message",
        "One message by id: its status (queued, sent, failed, cancelled), text "
        "and target. Use it to check whether a queued message has landed.",
        h_get_message,
        params={"message_id": MESSAGE_ID_PARAM},
        required=["message_id"],
    ),
    Command(
        "cancel_message",
        "Drop a queued message before it is delivered (e.g. it is no longer "
        "relevant by the time the human finishes typing).",
        h_cancel_message,
        params={"message_id": MESSAGE_ID_PARAM},
        required=["message_id"],
    ),
    # -- inboxes: machine-to-machine messaging that never types -------------
    Command(
        "inbox_send",
        "Put a message in another agent's inbox. Nothing is typed into their "
        "terminal, so it cannot collide with what a human is writing, cannot "
        "stick half-sent, and has no size limit. By default the recipient gets "
        "one short line telling them they have mail. Prefer this over send_text "
        "for anything going to another agent. Returns a stable message_id.",
        h_inbox_send,
        params={
            "to": MAILBOX_PARAM,
            "body": {"type": "string", "description": "The message. Any length."},
            "subject": {"type": "string", "description": "Optional one-line summary."},
            "attach": {"type": "string",
                       "description": "Optional local file path(s) to attach, comma separated."},
            "system_sender": {"type": "string",
                              "description": "Non-agent sender name (\"watchdog\" only); refused "
                                             "from any caller that has a pane."},
            "notify": {"type": "boolean",
                       "description": "Nudge the recipient's pane that mail arrived (default true)."},
        },
        required=["to", "body"],
    ),
    Command(
        "inbox_count",
        "How many messages are waiting: unread, total, and how long the oldest "
        "unread has waited. Cheap enough to poll. Defaults to your own inbox.",
        h_inbox_count,
        params={"mailbox": MAILBOX_PARAM,
                "fresh": {"type": "boolean",
                          "description": "Rebuild the cached view before answering."}},
    ),
    Command(
        "pm_digest",
        "Publish or update ONE digest for a project under a stable id "
        "(pm-digest:<project>) so a PM never becomes a stream. The payload is "
        "JSON with changed/blocked/owner_decisions/numbers, or plain text. The "
        "row is updated in place and becomes unread again.",
        h_pm_digest,
        params={
            "project": {"type": "string", "description": "Project name, e.g. poolrad."},
            "payload": {"type": "string",
                        "description": "JSON digest payload or plain text body."},
            "to": {"type": "string", "description": "VP pane (default from routing)."},
        },
        required=["project", "payload"],
    ),
    Command(
        "pm_routing",
        "Show the lane->PM routing registry: which project each lane reports to, "
        "its PM pane, and whether a stray VP message is redirected or flagged.",
        h_pm_routing,
        params={},
    ),
    Command(
        "inbox_list",
        "Summaries of the messages in a mailbox, oldest first: sender, subject, "
        "size, attachments and state. Does not mark anything read.",
        h_inbox_list,
        params={"mailbox": MAILBOX_PARAM,
                "unread_only": {"type": "boolean",
                                "description": "Only unread messages (default true)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500,
                          "description": "How many to return (default 50)."}},
    ),
    Command(
        "inbox_read",
        "Read waiting messages in order, with their full bodies, and mark them "
        "read. This is the one an agent calls when it has mail. Use peek=true "
        "to look without marking them read.",
        h_inbox_read,
        params={"mailbox": MAILBOX_PARAM,
                "limit": {"type": "integer", "minimum": 1, "maximum": 100,
                          "description": "How many to read (default 10)."},
                "peek": {"type": "boolean",
                         "description": "Do not mark them read (default false)."}},
    ),
    Command(
        "inbox_status",
        "State of one message by its id: queued, read, acked or cancelled, with "
        "its destination, when it was sent and read, size, attachment count and "
        "any failure reason. Deliberately carries no body or subject, so a "
        "status board can poll it without seeing the contents. Survives daemon "
        "restarts.",
        h_inbox_status,
        params={"message_id": MESSAGE_ID_PARAM_INBOX},
        required=["message_id"],
    ),
    Command(
        "inbox_ack",
        "Acknowledge messages: mark them finished with, so they stop showing as "
        "waiting and the sender can see they were handled. Ack stamps read_at "
        "as well, so a board that counts NULL read_at never reports drained "
        "mail as unread.",
        h_inbox_ack,
        params={"message_ids": MESSAGE_IDS_PARAM,
                "mailbox": MAILBOX_PARAM},
        required=["message_ids"],
    ),
    Command(
        "inbox_archive",
        "Archive a mailbox in bulk without deleting anything: marks messages "
        "read and archived (counted separately from unread) so unread boards "
        "drop. before=ISO date/time or unix timestamp; state=comma-separated "
        "queued,read,acked (default queued,acked).",
        h_inbox_archive,
        params={"mailbox": MAILBOX_PARAM,
                "before": {"type": "string",
                           "description": "Only messages created before this time."},
                "state": {"type": "string",
                          "description": "queued,read,acked (default queued,acked)."}},
        required=["mailbox"],
    ),
    Command(
        "inbox_cancel",
        "Withdraw messages you sent that have not been read yet.",
        h_inbox_cancel,
        params={"message_ids": MESSAGE_IDS_PARAM},
        required=["message_ids"],
    ),
    Command(
        "inbox_delete",
        "Delete messages outright, with their attachments.",
        h_inbox_delete,
        params={"message_ids": MESSAGE_IDS_PARAM},
        required=["message_ids"],
    ),
    Command(
        "inbox_stats",
        "Unread, archived, undeliverable and total counts for every mailbox, "
        "busiest first. Use it to see which agents have mail piling up.",
        h_inbox_stats,
    ),
    Command(
        "inbox_fetch",
        "Write an attachment out of the store to a local directory and return "
        "the path it was written to.",
        h_inbox_fetch,
        params={"attachment_id": {"type": "string",
                                  "description": "Attachment id, from inbox_read or inbox_list."},
                "dest": {"type": "string",
                         "description": "Directory to write into (default: the daemon's cwd)."}},
        required=["attachment_id"],
    ),
    # -- global and per-pane tasks ------------------------------------------
    Command(
        "task_add",
        "Create a global (fleet-wide) or per-pane task. Global is the default; "
        "scope='self' makes it private to the caller's pane. Use parent_id for "
        "subtasks and depends_on for ordering. Returns the task.",
        h_task_add,
        params={
            "title": TASK_TITLE_PARAM,
            "scope": TASK_SCOPE_ADD_PARAM,
            "body": TASK_BODY_PARAM,
            "status": TASK_STATUS_PARAM,
            "priority": TASK_PRIORITY_PARAM,
            "assignees": TASK_ASSIGNEES_PARAM,
            "parent_id": TASK_PARENT_PARAM,
            "depends_on": TASK_DEPS_PARAM,
            "multi_assignee": {"type": "boolean",
                               "description": "Allow several agents to claim this task."},
        },
        required=["title"],
    ),
    Command(
        "task_list",
        "List tasks visible to the caller: global tasks plus its own pane's "
        "queue. Filter by scope, status or assignee.",
        h_task_list,
        params={
            "scope": TASK_SCOPE_PARAM,
            "status": TASK_STATUS_PARAM,
            "assignee": {"type": "string",
                         "description": "Only tasks assigned to this pane/agent."},
            "include_done": {"type": "boolean",
                             "description": "Include done/cancelled (default true)."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 500,
                      "description": "Maximum tasks to return (default 200)."},
        },
    ),
    Command(
        "task_read",
        "Read one task with its children, live leases and blocking dependencies.",
        h_task_read,
        params={"id": TASK_ID_PARAM},
        required=["id"],
    ),
    Command(
        "task_update",
        "Change a task's title, body, status, priority, assignees, dependencies "
        "or parent. Any assignee completing it completes it for everyone.",
        h_task_update,
        params={
            "id": TASK_ID_PARAM,
            "title": TASK_TITLE_PARAM,
            "body": TASK_BODY_PARAM,
            "status": TASK_STATUS_PARAM,
            "priority": TASK_PRIORITY_PARAM,
            "assignees": TASK_ASSIGNEES_PARAM,
            "depends_on": TASK_DEPS_PARAM,
            "parent_id": TASK_PARENT_PARAM,
            "block_reason": {"type": "string", "description": "Why it is blocked."},
            "multi_assignee": {"type": "boolean",
                               "description": "Allow multiple claimants."},
        },
        required=["id"],
    ),
    Command(
        "task_assign",
        "Assign a task to one or more panes/agents. assignees=['self'] assigns "
        "it to the caller. mode=add|remove|set (default add).",
        h_task_assign,
        params={
            "id": TASK_ID_PARAM,
            "assignees": TASK_ASSIGNEES_PARAM,
            "mode": {"type": "string", "enum": ["add", "remove", "set"],
                     "description": "How to apply the assignees (default add)."},
        },
        required=["id", "assignees"],
    ),
    Command(
        "task_complete",
        "Complete a task for everyone. Global completion: the first assignee "
        "to complete a shared task completes it for all.",
        h_task_complete,
        params={
            "id": TASK_ID_PARAM,
            "by": {"type": "string",
                   "description": "Who completed it (defaults to caller pane)."},
        },
        required=["id"],
    ),
    Command(
        "task_block",
        "Mark a task blocked, with an optional reason.",
        h_task_block,
        params={"id": TASK_ID_PARAM,
                "reason": {"type": "string", "description": "Why it is blocked."}},
        required=["id"],
    ),
    Command(
        "task_unblock",
        "Return a blocked task to the open queue.",
        h_task_unblock,
        params={"id": TASK_ID_PARAM},
        required=["id"],
    ),
    Command(
        "task_cancel",
        "Cancel a task without completing it.",
        h_task_cancel,
        params={"id": TASK_ID_PARAM,
                "reason": {"type": "string", "description": "Why it was cancelled."}},
        required=["id"],
    ),
    Command(
        "task_delete",
        "Delete a task. Refused while it has children or is a dependency.",
        h_task_delete,
        params={"id": TASK_ID_PARAM},
        required=["id"],
    ),
    Command(
        "task_note",
        "Append an explanatory event to a task's history, including after the "
        "task row was deleted; the board's history explains its own gaps.",
        h_task_note,
        params={"id": TASK_ID_PARAM,
                "event": {"type": "string",
                          "description": "Event name (default 'note')."},
                "reason": {"type": "string",
                           "description": "Human rationale stored in meta.reason."}},
        required=["id"],
    ),
    Command(
        "task_next",
        "ATOMIC CLAIM: take the highest-priority unblocked task the caller may "
        "work on, assign it to the caller and set it in_progress in one SQLite "
        "transaction. scope=global (default), self (own pane), auto (pane then "
        "global), or a pane id. Returns {task, lease_seconds, expires_at}; when "
        "the queue is empty the caller is auto-subscribed for a push.",
        h_task_next,
        params={"scope": TASK_SCOPE_NEXT_PARAM,
                "lease_seconds": TASK_LEASE_PARAM},
    ),
    Command(
        "task_heartbeat",
        "Extend the lease on a task you have claimed. Expired or dead-pane "
        "leases are requeued automatically.",
        h_task_heartbeat,
        params={"id": TASK_ID_PARAM, "lease_seconds": TASK_LEASE_PARAM},
        required=["id"],
    ),
    Command(
        "task_release",
        "Give a claimed task back to the queue without completing it.",
        h_task_release,
        params={"id": TASK_ID_PARAM},
        required=["id"],
    ),
    Command(
        "task_follow",
        "Ask the daemon to PUSH the next task to this pane when it is idle. "
        "Deterministic: the daemon selects, claims and delivers it; no model "
        "polling. scope=global (default) or self.",
        h_task_follow,
        params={"scope": TASK_SCOPE_ADD_PARAM},
    ),
    Command(
        "task_unfollow",
        "Stop receiving pushed tasks.",
        h_task_unfollow,
    ),
    Command(
        "task_followers",
        "List panes currently subscribed to task pushes.",
        h_task_followers,
    ),
    Command(
        "task_stats",
        "Task counts by status across the caller's visible scopes.",
        h_task_stats,
        params={"scope": TASK_SCOPE_PARAM},
    ),
    Command(
        "task_metrics",
        "Timeseries-friendly task metrics: current queue depth, per-status / "
        "priority / scope breakdown, per-assignee load, created/claimed/"
        "completed/cancelled/requeued throughput in hourly or daily buckets, "
        "completion and claim latency (avg/p50/p90/max), and per-actor counts. "
        "Computed deterministically from an append-only event log; no model.",
        h_task_metrics,
        params={
            "window_s": {"type": "number", "minimum": 1,
                         "description": "Window ending now, seconds (default 3600)."},
            "since": {"type": "number",
                      "description": "Window start epoch (overrides window_s)."},
            "until": {"type": "number",
                      "description": "Window end epoch (default now)."},
            "scope": TASK_SCOPE_PARAM,
            "bucket": {"type": "string", "enum": ["hour", "day"],
                       "description": "Throughput bucket size (default hour)."},
        },
    ),
    Command(
        "task_reap",
        "Requeue expired or dead-pane task leases now (the daemon also does "
        "this on a timer).",
        h_task_reap,
        mcp=False,
    ),
    # -- memory and recall ---------------------------------------------------
    Command(
        "memory_recall",
        "Ranked snippets from past agent sessions (Claude, Codex, opencode, pi). "
        "Read-only: a query never reindexes and never waits for the ingest "
        "worker.",
        h_memory_recall,
        params={"query": MEMORY_QUERY_PARAM, "limit": MEMORY_LIMIT_PARAM,
                "harness": MEMORY_HARNESS_PARAM, "project": MEMORY_PROJECT_PARAM,
                "since": MEMORY_SINCE_PARAM},
        required=["query"],
    ),
    Command(
        "memory_context",
        "A compact digest of relevant past sessions for session start: top "
        "sessions with a few snippets and metadata.",
        h_memory_context,
        params={"query": MEMORY_QUERY_PARAM,
                "sessions": {"type": "integer", "minimum": 1, "maximum": 20,
                             "description": "Sessions to return (default 5)."}},
        required=["query"],
    ),
    Command(
        "memory_search",
        "Filtered memory search (harness, project, since). Same engine as "
        "memory_recall.",
        h_memory_search,
        params={"query": MEMORY_QUERY_PARAM, "limit": MEMORY_LIMIT_PARAM,
                "harness": MEMORY_HARNESS_PARAM, "project": MEMORY_PROJECT_PARAM,
                "since": MEMORY_SINCE_PARAM},
        required=["query"],
    ),
    Command(
        "memory_stats",
        "Memory service counts: sources, sessions, chunks by harness, DB bytes, "
        "ingest queue depth, and query p50/p95/p99.",
        h_memory_stats,
    ),
    Command(
        "memory_status",
        "Memory service health: schema version, database, ingest worker, queue "
        "and latency.",
        h_memory_status,
    ),
    Command(
        "memory_backfill",
        "Discover every known session source (Codex/Claude/pi/opencode) and "
        "enqueue it for the throttled background ingest. action=start|pause|"
        "resume|status; limit=N for a bounded run. Superusers only.",
        h_memory_backfill,
        params={"action": {"type": "string",
                            "enum": ["start", "pause", "resume", "status"]},
                "limit": {"type": "integer", "minimum": 1}},
    ),
    Command(
        "memory_ingest",
        "Queue a session log or sqlite DB for asynchronous ingest (superusers "
        "only). The worker ingests in the background; queries stay responsive.",
        h_memory_ingest,
        params={"path": MEMORY_PATH_PARAM, "harness": MEMORY_HARNESS_PARAM,
                "project": MEMORY_PROJECT_PARAM},
        required=["path"],
    ),
    Command(
        "health",
        "Deterministic self-check: memory DB integrity, stale ingest leases, "
        "WAL size, free disk space, and task counts. repair=true applies the "
        "fixed repairs (requeue stale leases, checkpoint a large WAL). No model "
        "is consulted; every threshold is a constant.",
        h_health,
        params={"repair": {"type": "boolean",
                           "description": "Apply the fixed repairs (default false)."},
                "deep": {"type": "boolean",
                         "description": "Also run PRAGMA quick_check (slow)."}},
    ),
    Command(
        "doctor",
        "Read-only post-upgrade doctor: herdr version, pane detection, "
        "read --source visible, guard status, and dunk target metadata that no "
        "longer matches the pane. Flags stale target_agent/target_cwd.",
        h_doctor,
    ),
    Command(
        "recover",
        "Post-upgrade recovery plan from dunks.json + launches.json + memory "
        "sessions. apply=true types launches only into panes that are still "
        "bare shells (never over a live agent); record=true snapshots live "
        "agent sessions first. A missing session is reported, never invented.",
        h_recover,
        params={"apply": {"type": "boolean",
                          "description": "Type launch commands into empty shells."},
                "record": {"type": "boolean",
                           "description": "Snapshot live agent sessions into launches.json."}},
    ),
    Command(
        "metrics",
        "Read-only operations metrics as JSON: counts by sender, recipient, "
        "pane and source, duration distributions for queue wait, delivery and "
        "handling, the Enter-retry histogram, live queue depth, and a coverage "
        "block saying what is and is not measured. Carries identifiers and "
        "numbers only, never message text. Durations are null rather than zero "
        "when nothing in the window measures them.",
        h_metrics,
        params={"window_s": {"type": "number", "minimum": 1, "maximum": 2592000,
                             "description": "Window ending now, in seconds (default 3600)."},
                "since": {"type": "number",
                          "description": "Window start as a unix timestamp (overrides window_s)."},
                "until": {"type": "number",
                          "description": "Window end as a unix timestamp (default now)."}},
    ),
    Command(
        "list_panes",
        "List herdr panes (targets) grouped by workspace and tab, with the "
        "agent in each pane and its status. `is_self` marks the caller's pane.",
        h_list_panes,
    ),
    Command(
        "read_pane",
        "Read the last N lines of any pane's terminal output (agent transcripts "
        "included). Use it to watch what other agents or projects are doing "
        "before deciding what to send or schedule; returns the pane's agent "
        "and its status too.",
        h_read_pane,
        params={"target": TARGET_PARAM,
                "lines": {"type": "integer", "minimum": 1, "maximum": 2000,
                          "description": "How many recent lines (default 40)."}},
        required=["target"],
    ),
    # -- herdr layout control: build the herd, then dunk on it ---------------
    Command(
        "delivery_capability",
        "How the desk may deliver to this target: does its harness drain a push "
        "queue (pi) or must the message go as an immediate guarded send (claude, "
        "codex)? Returns pane_id, mailbox (the PINNED name when declared), agent, "
        "alive, drains, send ('owner_push' or 'guarded') and expect. Refusals use "
        "the same tokens as a send: dead_lane / unhoused_lane / unknown_name.",
        h_delivery_capability,
        params={"target": {"type": "string",
                           "description": "pane id (%9) or pinned lane name"}}),
    Command(
        "list_workspaces",
        "List herdr workspaces with their tabs (ids, labels, agent status). "
        "`self` is the caller's own workspace/tab. Use it to pick where to "
        "create tabs or start agents.",
        h_list_workspaces,
    ),
    Command(
        "create_workspace",
        "Create a new herdr workspace (a named space with its first tab and a "
        "shell pane). Optionally run a command in that pane, e.g. 'claude' to "
        "start an agent there. Returns the new workspace, tab and pane ids; the "
        "pane id is a dunk target.",
        h_create_workspace,
        params={
            "label": LABEL_PARAM,
            "cwd": CWD_PARAM,
            "command": RUN_COMMAND_PARAM,
            "focus": FOCUS_PARAM,
            "mailbox": {"type": "string", "description":
                        "the lane mailbox this new pane IS; recorded in the lane "
                        "registry against the new pane id, so mail addressed to "
                        "that lane reaches this pane after a backend change"},
        },
    ),
    Command(
        "create_tab",
        "Create a new named tab (with a shell pane) in a workspace. Optionally "
        "run a command in it, e.g. 'codex' or 'claude --model opus'. Returns the "
        "new tab and pane ids.",
        h_create_tab,
        params={
            "workspace": WORKSPACE_PARAM,
            "label": LABEL_PARAM,
            "cwd": CWD_PARAM,
            "command": RUN_COMMAND_PARAM,
            "focus": FOCUS_PARAM,
        },
        mcp_required=["label"],
    ),
    Command(
        "split_pane",
        "Split an existing pane right or down, giving a new shell pane beside "
        "it. Optionally run a command in the new pane.",
        h_split_pane,
        params={
            "target": TARGET_PARAM,
            "direction": {"type": "string", "enum": ["right", "down"],
                          "description": "Where the new pane goes (default right)."},
            "cwd": CWD_PARAM,
            "command": RUN_COMMAND_PARAM,
            "focus": FOCUS_PARAM,
        },
        required=["target"],
    ),
    Command(
        "start_agent",
        "Launch an agent process in a new pane and register it with herdr under "
        "`name` (herdr then tracks it by that name and status). `command` is the "
        "full command line, e.g. 'claude' or 'codex --model gpt-5'. Place it in "
        "`tab` (splitting when `split` is given) or in `workspace` (herdr picks "
        "the tab; use create_tab + tab for a tab of your own naming); omit both "
        "to place it in the dedicated sandbox workspace, never the caller's tab. "
        "Superusers only. Returns the agent's pane id, ready to dunk on.",
        h_start_agent,
        params={
            "name": {"type": "string",
                     "description": "Agent name as herdr will show it, e.g. 'writer'."},
            "command": {"type": "string",
                        "description": "Command line to run, shell-split, e.g. 'claude --model opus'."},
            "workspace": WORKSPACE_PARAM,
            "tab": TAB_PARAM,
            "split": {"type": "string", "enum": ["right", "down"],
                      "description": "With `tab`: split that tab's pane instead of replacing it."},
            "cwd": CWD_PARAM,
            "focus": FOCUS_PARAM,
        },
        required=["name", "command"],
    ),
    Command(
        "rename",
        "Rename a workspace, tab or pane. Labels are how targets are named "
        "everywhere else, so name things as you want to address them.",
        h_rename,
        params={"kind": KIND_PARAM, "target": KIND_TARGET_PARAM,
                "label": {"type": "string", "description": "The new label."}},
        required=["kind", "target", "label"],
    ),
    Command(
        "focus",
        "Bring a workspace, tab or pane to the front of the human's herdr "
        "session (to draw their attention to it).",
        h_focus,
        params={"kind": KIND_PARAM, "target": KIND_TARGET_PARAM},
        required=["kind", "target"],
    ),
    Command(
        "close",
        "Close a workspace, tab or pane, killing whatever runs inside and "
        "stopping any dunks aimed at it. Refuses to close the caller's own "
        "pane or its tab/workspace.",
        h_close,
        params={"kind": KIND_PARAM, "target": KIND_TARGET_PARAM,
                "force": {"type": "boolean",
                          "description": "Allow closing the caller's own pane/tab/workspace. Superusers only; use with care."}},
        required=["kind", "target"],
    ),
    Command(
        "herdr",
        "Run any herdr CLI subcommand and get its JSON result: the escape hatch "
        "for everything herdr can do that has no dedicated tool here (`agent "
        "list`, `pane zoom w9:p2 --on`, `notification show 'Done'`, `wait "
        "agent-status w9:p2 --status idle --timeout 60000`, `api schema --json` "
        "to discover all of it). Pass the subcommand line without the leading "
        "'herdr'; the word `self` becomes the caller's pane id (its tab or "
        "workspace id in `tab ...` / `workspace ...` commands). herdr takes ids "
        "here, not labels: list_workspaces / list_panes give them. Commands that "
        "would stop, update or attach herdr itself are refused.",
        h_herdr,
        params={"command": {"type": "string",
                            "description": "herdr subcommand line, e.g. 'tab focus w9:t2'."}},
        required=["command"],
    ),
    Command(
        "herdr_help",
        "herdr's own usage text, for discovering what the `herdr` tool can run: "
        "no topic lists the command groups; a topic such as 'pane', 'tab', "
        "'workspace', 'agent', 'wait' or 'notification' lists that group's "
        "subcommands and flags.",
        h_herdr_help,
        params={"topic": {"type": "string",
                          "description": "A herdr command group, e.g. 'pane'. Omit for the overview."}},
    ),
    Command(
        "shutdown",
        "Stop the daemon process (superusers only). With stop_all, also stop "
        "every dunk first (otherwise they resume when the daemon next starts).",
        h_shutdown,
        params={"stop_all": {"type": "boolean"}},
        mcp=False,
    ),
    # -- superuser allowlist -------------------------------------------------
    Command(
        "superuser_status",
        "Show the superuser allowlist and whether this caller is on it. "
        "Non-superusers may use the inbox and task queue only.",
        h_superuser_status,
    ),
    Command(
        "superuser_add",
        "Add a pane id or name to the superuser allowlist (superusers only; "
        "the first entry may bootstrap an empty list). Names match a pane's "
        "label/tab label, so the VP is recognised across pane-id changes.",
        h_superuser_add,
        params={"entry": SUPERUSER_ENTRY_PARAM},
        required=["entry"],
    ),
    Command(
        "superuser_remove",
        "Remove a pane id or name from the allowlist (superusers only; the "
        "last superuser cannot be removed).",
        h_superuser_remove,
        params={"entry": SUPERUSER_ENTRY_PARAM},
        required=["entry"],
    ),
    Command(
        "superuser_recover",
        "Re-assert the caller's pane id and the configured VP name in the "
        "allowlist. Bootstrap when the list is empty; afterwards an existing "
        "superuser must run it. Run this on VP setup/takeover/recovery.",
        h_superuser_recover,
        params={"name": SUPERUSER_NAME_PARAM},
    ),
]

COMMANDS_BY_NAME = {command.name: command for command in COMMANDS}


def mcp_commands():
    return [command for command in COMMANDS if command.mcp]


# -- grouped MCP surface (t8) -------------------------------------------------
# One namespaced tool per domain, each taking an `action`. This merges the ~70
# flat tools into a small hot set while keeping every capability: every flat
# command stays callable by name, and each is reachable through its group.
MCP_GROUPS = (
    ("inbox",
     "Agent-to-agent mailbox. action=send (preferred over typing), count, "
     "list, read (also returns your tasks), status, ack, cancel, delete, "
     "stats, fetch. No size limit; a send nudges the recipient with one line.",
     (("send", "inbox_send"), ("count", "inbox_count"), ("list", "inbox_list"),
      ("read", "inbox_read"), ("status", "inbox_status"), ("ack", "inbox_ack"),
      ("cancel", "inbox_cancel"), ("delete", "inbox_delete"),
      ("stats", "inbox_stats"), ("fetch", "inbox_fetch"),
      ("archive", "inbox_archive"))),
    ("task",
     "Deterministic global task queue. action=next ATOMICALLY claims the "
     "highest-priority unblocked task (assigns you + in_progress + a lease); "
     "heartbeat extends it; complete/block/unblock finish it; add/list/read/"
     "update/assign/cancel/delete manage the queue; follow makes the daemon "
     "PUSH work when you are idle; stats/metrics report the board. scope is "
     "'global' (fleet) or 'self' (your pane). All SQL, no model.",
     (("add", "task_add"), ("list", "task_list"), ("read", "task_read"),
      ("update", "task_update"), ("assign", "task_assign"),
      ("complete", "task_complete"), ("block", "task_block"),
      ("unblock", "task_unblock"), ("cancel", "task_cancel"),
      ("delete", "task_delete"), ("note", "task_note"), ("next", "task_next"),
      ("heartbeat", "task_heartbeat"), ("release", "task_release"),
      ("follow", "task_follow"), ("unfollow", "task_unfollow"),
      ("followers", "task_followers"), ("stats", "task_stats"),
      ("metrics", "task_metrics"), ("reap", "task_reap"))),
    ("dunk",
     "Scheduled text into a pane (a 'dunk'). action=add/list/get/update/"
     "remove/start/stop/stop_all/fire. Superuser-gated (except list/get). "
     "{id} in text expands to the dunk's own id at send time.",
     (("list", "list_dunks"), ("get", "get_dunk"), ("add", "add_dunk"),
      ("update", "update_dunk"), ("remove", "remove_dunk"),
      ("start", "start_dunk"), ("stop", "stop_dunk"), ("stop_all", "stop_all"),
      ("fire", "fire_dunk"))),
    ("views",
     "Read-only compositions for the Command Center: roster, message_state, "
     "reply_stats, project_summary. All queries, no model, no pane writes.",
     (("push", "owner_push"), ("take", "owner_take"), ("stats", "owner_stats"),
      ("roster", "roster"), ("message_state", "message_state"),
      ("reply_stats", "reply_stats"), ("project_summary", "project_summary"),
      ("capture", "state_capture"), ("latest", "state_latest"))),
    ("dispatch",
     "Deterministic feeding, no model: the standby queue and the tick that fills "
     "idle lanes. action=add/list/remove/tick/status. A lane is fed only when it "
     "is ready AND has no unread mail; the tick reports idle lanes, dispatched "
     "this hour, and blocked lanes with reasons.",
     (("dispatch", "dispatch"), ("retire", "retire_pane"), ("unretire", "retire_pane"),
      ("retired", "retire_pane"),
      ("lanes", "lanes"), ("lane_set", "lane_set"))),
    ("pane",
     "Inspect and address a pane. action=list/read/delivery/send/send_keys/"
     "flush/clear_box/messages/get_message/cancel_message. delivery says whether "
     "a lane can receive anything and why not (ready/working/blocked/dead). "
     "send_text (send) types plus Enter; when a human is composing it returns "
     "queued=true and is delivered later. flush submits a stranded draft; "
     "clear_box DESTROYS one (superuser only). Prefer the inbox for agents.",
     (("list", "list_panes"), ("read", "read_pane"), ("delivery", "delivery"),
      ("capability", "delivery_capability"),
      ("send", "send_text"),
      ("send_keys", "send_keys"), ("flush", "flush_box"),
      ("clear_box", "clear_box"),
      ("messages", "list_messages"), ("get_message", "get_message"),
      ("cancel_message", "cancel_message"))),
    ("workspace",
     "Layout and the herd. action=list/create/create_tab/split/start_agent/"
     "rename/focus/close. Creators and close are superuser-gated; an agent "
     "started without a workspace goes to the sandbox, never the caller's tab.",
     (("list", "list_workspaces"), ("create", "create_workspace"),
      ("create_tab", "create_tab"), ("split", "split_pane"),
      ("start_agent", "start_agent"), ("rename", "rename"),
      ("focus", "focus"), ("close", "close"))),
    ("superuser",
     "The superuser allowlist. action=status/add/remove/recover. Destructive "
     "operations are refused unless the caller is on the list.",
     (("status", "superuser_status"), ("add", "superuser_add"),
      ("remove", "superuser_remove"), ("recover", "superuser_recover"))),
    ("memory",
     "Deterministic SQLite+FTS5 recall over every agent session. action=recall/"
     "context/search/stats/status are read-only and never wait on ingest; "
     "ingest/backfill (superuser) queue sources.",
     (("recall", "memory_recall"), ("context", "memory_context"),
      ("search", "memory_search"), ("stats", "memory_stats"),
      ("status", "memory_status"), ("ingest", "memory_ingest"),
      ("backfill", "memory_backfill"))),
    ("metrics",
     "Operations measurement. action=summary (counts, duration distributions, "
     "coverage), status (daemon health + counts), health (deterministic "
     "self-check; repair=true fixes stale leases and a large WAL).",
     (("summary", "metrics"), ("status", "status"), ("health", "health"))),
    ("pm",
     "Project PMs. action=digest publishes or updates ONE digest per project "
     "under a stable id (pm-digest:<project>) with changed/blocked/"
     "owner_decisions/numbers so a PM is a digest, not a stream; routing shows "
     "the lane->PM registry and whether stray VP mail is redirected or flagged.",
     (("digest", "pm_digest"), ("routing", "pm_routing"))),
    ("system",
     "Discovery, recovery and the raw escape hatch. action=help (full guide + registry), "
     "herdr (any herdr subcommand except the typing ones), herdr_help, recover, doctor.",
     (("help", "help"), ("herdr", "herdr"), ("herdr_help", "herdr_help"),
      ("recover", "recover"), ("doctor", "doctor"))),
)


class GroupedTool:
    """One domain tool: an `action` enum plus the union of its commands' params."""

    def __init__(self, name, description, actions):
        self.name = name
        self.description = description
        self.actions = dict(actions)  # action -> flat command name
        self.commands = {command: COMMANDS_BY_NAME[command]
                         for command in self.actions.values()
                         if command in COMMANDS_BY_NAME}

    def resolve(self, action):
        return self.actions.get(action)

    def input_schema(self):
        properties = {
            "action": {
                "type": "string",
                "enum": list(self.actions),
                "description": "Which operation to run.",
            }
        }
        for command in self.commands.values():
            for key, value in command.params.items():
                properties.setdefault(key, dict(value))
        return {
            "type": "object",
            "properties": properties,
            # Per-action required params are validated by the server before
            # dispatch, so a single grouped schema stays small and honest.
            "required": ["action"],
            "additionalProperties": False,
        }


def grouped_mcp_tools():
    return [GroupedTool(name, description, actions)
            for name, description, actions in MCP_GROUPS]


def mcp_tool_map():
    """OLD flat command -> {tool, action, mcp}. Covers every registered command,
    including the CLI/socket-only ones, so nothing is silently dropped."""
    grouped = {}
    for tool in grouped_mcp_tools():
        for action, command in tool.actions.items():
            grouped[command] = {"tool": tool.name, "action": action, "mcp": True}
    for command in COMMANDS:
        grouped.setdefault(command.name, {"tool": None, "action": None,
                                          "mcp": command.mcp})
    return grouped


MCP_TOOL_MAP = mcp_tool_map()


# Destructive or affecting-others operations. The 2026-10-18 incident: a free
# model closed the VP's tab (nine panes) through the herdr CLI. Non-superusers
# keep the inbox, the task queue and read-only tools; everything here needs an
# entry in superusers.json.
SUPERUSER_ONLY = {
    "add_dunk", "update_dunk", "remove_dunk", "start_dunk", "stop_dunk",
    "toggle_dunk", "stop_all", "fire_dunk",
    "start_agent", "create_workspace", "create_tab", "split_pane",
    "rename", "focus", "close", "herdr", "shutdown", "memory_ingest",
    "lane_set",
    "memory_backfill",
}


def dispatch(flock, name, args=None, self_pane_id=None):
    """Run a named command. Raises DunkError for unknown commands or bad args."""
    command = COMMANDS_BY_NAME.get(name)
    if command is None:
        raise DunkError(f"unknown command {name!r}")
    if name in SUPERUSER_ONLY:
        flock.require_superuser(self_pane_id, name)
    args = dict(args or {})
    missing = [key for key in command.required if args.get(key) in (None, "")]
    if missing:
        raise DunkError(f"{name}: missing required argument(s): {', '.join(missing)}")
    unknown = [key for key in args if key not in command.params]
    if unknown:
        raise DunkError(f"{name}: unknown argument(s): {', '.join(sorted(unknown))}")
    return command.handler(flock, args, self_pane_id)
