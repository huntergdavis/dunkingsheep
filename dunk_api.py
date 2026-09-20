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

from dunk_core import DEFAULT_INTERVAL_MINUTES, DEFAULT_TEXT, DunkError

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


# -- handlers -----------------------------------------------------------------

def h_status(flock, args, self_pane_id):
    result = flock.status()
    result["self_pane_id"] = self_pane_id
    return result


def h_list_dunks(flock, args, self_pane_id):
    return {"dunks": flock.list()}


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
    return flock.send_now(args["target"], args["text"], self_pane_id=self_pane_id)


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


def h_create_workspace(flock, args, self_pane_id):
    return flock.create_workspace(
        label=args.get("label"), cwd=args.get("cwd"), command=args.get("command"),
        focus=_bool(args.get("focus"), False),
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
    return flock.close_target(args["kind"], args["target"], self_pane_id=self_pane_id)


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
    ),
    Command(
        "list_dunks",
        "List every dunk with its target, text, interval, running state, "
        "live status, send count, skip count and seconds until the next send.",
        h_list_dunks,
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
        "(the TUI's 't' test send).",
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
        "Send a direct message or instruction to a pane right now: type text + "
        "Enter once, with no dunk created. Use it to talk to another agent "
        "(target its pane) or to inject a command into a shell pane.",
        h_send_text,
        params={"target": TARGET_PARAM,
                "text": {"type": "string",
                         "description": "Text to type, followed by Enter. Sent verbatim (no placeholders)."}},
        required=["target", "text"],
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
        "to let herdr choose. Returns the agent's pane id, ready to dunk on.",
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
        params={"kind": KIND_PARAM, "target": KIND_TARGET_PARAM},
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
        "Stop the daemon process. With stop_all, also stop every dunk first "
        "(otherwise they resume when the daemon next starts).",
        h_shutdown,
        params={"stop_all": {"type": "boolean"}},
        mcp=False,
    ),
]

COMMANDS_BY_NAME = {command.name: command for command in COMMANDS}


def mcp_commands():
    return [command for command in COMMANDS if command.mcp]


def dispatch(flock, name, args=None, self_pane_id=None):
    """Run a named command. Raises DunkError for unknown commands or bad args."""
    command = COMMANDS_BY_NAME.get(name)
    if command is None:
        raise DunkError(f"unknown command {name!r}")
    args = dict(args or {})
    missing = [key for key in command.required if args.get(key) in (None, "")]
    if missing:
        raise DunkError(f"{name}: missing required argument(s): {', '.join(missing)}")
    unknown = [key for key in args if key not in command.params]
    if unknown:
        raise DunkError(f"{name}: unknown argument(s): {', '.join(sorted(unknown))}")
    return command.handler(flock, args, self_pane_id)
