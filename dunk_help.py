#!/usr/bin/env python3
"""
The one guide to Dunking Sheep, shared by every surface.

- `dunkingsheep guide` prints it (plain text or `--markdown`).
- `dunkingsheep --help` shows the short version as its epilog.
- The MCP server returns it from `initialize.instructions` and the `help` tool.
- The socket API returns it from the `help` command, with the command registry.
- `docs/GUIDE.md` is the markdown rendering (a test keeps it in sync).

Keep it accurate: this is what an agent reads to teach itself the tool.
"""

from dunk_core import VERSION

SHORT = """\
Dunking Sheep types text (plus Enter) into herdr terminal panes on a schedule.
Each schedule is a "dunk": a target pane, a text, an interval, and a running
flag. Dunks live in a background daemon, so the TUI, this CLI, the unix
socket and the MCP server all see and control the same dunks.

quick start
  dunkingsheep panes                              # what can I target?
  dunkingsheep add -T "Codex Site" -e 15 -t continue
  dunkingsheep add -T self -e 60 --only-idle \\
      -t "Check the backlog. If done: dunkingsheep remove {id}. Else keep going."
  dunkingsheep list                               # live status, ids, countdowns
  dunkingsheep remove d2
  dunkingsheep tui                                # the curses view of the same dunks

learn more
  dunkingsheep guide          full guide (concepts, MCP, socket protocol, recipes)
  dunkingsheep commands       every command the socket/MCP expose (--json: schemas)
  dunkingsheep <command> -h   per-command flags and examples
"""


def guide(markdown=False):
    """Return the full guide as plain text (default) or GitHub markdown."""
    out = []

    def h(level, text):
        if markdown:
            return f"{'#' * level} {text}\n\n"
        return _underline(text, level) + "\n"

    def code(body):
        if markdown:
            return f"```\n{body.rstrip()}\n```\n\n"
        return "".join(f"    {line}\n" for line in body.rstrip().splitlines()) + "\n"

    def w(text):
        # Paragraphs and lists are already newline-terminated; separate blocks.
        out.append(text if text.endswith("\n\n") else text + "\n")

    w(h(1, f"Dunking Sheep {VERSION} guide"))
    w("Dunking Sheep sends text (followed by Enter) into herdr terminal panes on a\n"
      "schedule. It exists to keep AI coding agents moving (\"continue\"), to let\n"
      "agents message each other, and to let an agent schedule future nudges to\n"
      "itself. Everything below is the same whether you drive it from the keyboard,\n"
      "the command line, a unix socket, or the Model Context Protocol (MCP).\n")

    w(h(2, "Concepts"))
    w("- dunk / dunker: one schedule. Fields: id (d1, d2, ...), name, target pane,\n"
      "  text, interval_minutes, running, only_when, max_sends, send_count, status.\n"
      "- target: a herdr pane. Give a pane id (w8:p3), the word self (the pane the\n"
      "  caller runs in, from $HERDR_PANE_ID), a terminal id, or a unique\n"
      "  case-insensitive substring of a tab label, workspace label, agent name or\n"
      "  directory. Ambiguous names are rejected with the candidates listed.\n"
      "- send: the daemon runs `herdr pane send-text` then `herdr pane send-keys\n"
      "  Enter` (chunked for long texts). Nothing touches the OS keyboard.\n"
      "- daemon: `dunkingsheep serve`, auto-started by any client. Listens on a unix\n"
      "  socket at ~/.config/dunkingsheep/dunkingsheep.sock ($DUNKINGSHEEP_SOCKET).\n"
      "  Logs to ~/.config/dunkingsheep/daemon.log.\n"
      "- persistence: dunks are saved to ~/.config/dunkingsheep/dunks.json on every\n"
      "  change and restored, still running, when the daemon next starts. A dunk\n"
      "  whose send came due while the daemon was down fires shortly after restart.\n")

    w(h(2, "Text placeholders"))
    w("These expand at send time; any other braces are left untouched:\n")
    w(code("{id}        the dunk's id, e.g. d3      {name}      its name (or id)\n"
           "{count}     this send's number (1, 2..) {target}    tab / agent label\n"
           "{interval}  minutes between sends       {time} {date}  local clock"))
    w("So a dunk can say who it is: \"...when the backlog is finished, remove\n"
      "dunk {id}\" tells the receiving agent exactly which dunk to delete.\n")

    w(h(2, "Orchestration options"))
    w("- only_when = idle: hold each send until herdr reports the target agent as\n"
      "  idle (not working or blocked); re-checked every 5 seconds. Panes without a\n"
      "  detected agent count as idle.\n"
      "- max_sends = N: the dunk removes itself after its N-th send. N = 1 is a\n"
      "  one-shot delayed nudge (\"in 30 minutes, tell me to check CI\").\n"
      "- start = false: create the dunk stopped; start it later.\n"
      "- Changing the interval of a running dunk reschedules its next send.\n")

    w(h(2, "Surface 1: command line"))
    w(code("dunkingsheep panes                          list targets (* marks this pane)\n"
           "dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle]\n"
           "                 [--max-sends N] [--no-start]     -e takes 15, 90s, 1.5h\n"
           "dunkingsheep list | get <id> | update <id> ... | remove <id>\n"
           "dunkingsheep start|stop|toggle <id>    stop-all    fire <id> (send now)\n"
           "dunkingsheep send <target> <text...>   one-off send, no dunk\n"
           "dunkingsheep read <target> [-l N]      last N lines of a pane\n"
           "dunkingsheep status | shutdown [--stop-all] | serve | tui | mcp\n"
           "dunkingsheep commands [--json]         the full command registry\n"
           "Add --json to any command for machine-readable output."))

    w(h(2, "Surface 2: unix socket API"))
    w("Newline-delimited JSON, any number of requests per connection. The command\n"
      "names and arguments are exactly the registry from `dunkingsheep commands`.\n")
    w(code('-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",\n'
           '    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}\n'
           '<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, ...}}\n'
           '<- {"id": 1, "ok": false, "error": "no dunk with id \'d9\'"}\n'
           "\n"
           "# from a shell:\n"
           "printf '%s\\n' '{\"id\":1,\"cmd\":\"list_dunks\"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock\n"
           "# discover everything, including JSON schemas:\n"
           "printf '%s\\n' '{\"id\":1,\"cmd\":\"help\"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock"))
    w("`self_pane_id` is optional; it is what target \"self\" resolves to. Clients\n"
      "normally pass their own $HERDR_PANE_ID.\n")

    w(h(2, "Surface 3: MCP server (Claude Code, Codex, Gemini CLI, Cursor, ...)"))
    w("`dunkingsheep mcp` is a Model Context Protocol server over stdio (newline-\n"
      "delimited JSON-RPC, protocol versions 2024-11-05 through 2025-11-25, tools\n"
      "only, standard library only). Every registry command marked for MCP is a\n"
      "tool with a flat JSON schema (string/number/integer/boolean, no unions), so\n"
      "strict validators such as OpenAI's accept it. `shutdown` is not exposed.\n")
    w("`dunkingsheep mcp-setup` prints the exact registration for each harness and\n"
      "`dunkingsheep mcp-setup <harness> --apply` runs it when that harness has a CLI:\n")
    w(code("claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp\n"
           "codex mcp add dunkingsheep -- python3 /path/to/dunkingsheep mcp\n"
           "gemini mcp add --scope user dunkingsheep python3 /path/to/dunkingsheep mcp\n"
           "\n"
           "# any harness that reads an mcpServers JSON block (Cursor, Windsurf, Claude\n"
           "# Desktop, VS Code, ...):\n"
           '{"mcpServers": {"dunkingsheep": {"command": "python3",\n'
           '                                 "args": ["/path/to/dunkingsheep", "mcp"]}}}\n'
           "\n"
           "# ~/.codex/config.toml equivalent:\n"
           "[mcp_servers.dunkingsheep]\n"
           'command = "python3"\n'
           'args = ["/path/to/dunkingsheep", "mcp"]'))
    w("The harness spawns the MCP server inside the agent's own herdr pane, so the\n"
      "server inherits HERDR_PANE_ID and target \"self\" means the agent itself. If a\n"
      "harness scrubs the environment, pass `--self-pane <pane_id>` after `mcp` or\n"
      "set HERDR_PANE_ID in the server's env block.\n")
    w("Tools: help, status, list_panes, read_pane, list_dunks, get_dunk, add_dunk,\n"
      "update_dunk, remove_dunk, start_dunk, stop_dunk, stop_all, fire_dunk,\n"
      "send_text. Call `help` first if unsure; `initialize` also returns these\n"
      "instructions. Errors come back as tool results with isError=true and a\n"
      "plain-English message (never as protocol errors), so the agent can recover.\n")

    w(h(2, "Surface 4: the TUI"))
    w("`dunkingsheep tui` (or ./run_dunking_sheep.sh) shows the same dunks live.\n"
      "Keys: j/k move, a add, d remove, c choose target, t test send, i interval,\n"
      "e text, n name, o toggle idle-gate, m max sends, space/s start-stop, q quit\n"
      "(dunks keep running), Q stop every dunk and shut the daemon down.\n")

    w(h(2, "Recipe: one agent watching and steering many"))
    w("Because every pane is addressable, one agent can supervise a herd:\n")
    w(code("list_panes()                                  # who is running where, idle or working\n"
           "read_pane(target=\"Codex Site\", lines=80)      # what is that agent doing?\n"
           "send_text(target=\"Codex Site\", text=\"Stop and write tests first.\")\n"
           "add_dunk(target=\"Melt Squad\", interval_minutes=20, only_when=\"idle\",\n"
           "         text=\"Progress report: what changed since the last one?\")"))
    w("send_text is a direct message with no schedule attached; read_pane is the\n"
      "eyes. Together with add_dunk they let an agent communicate with, monitor and\n"
      "pace several projects in parallel.\n")

    w(h(2, "Recipe: meta-dunking (an agent schedules itself)"))
    w("An agent with the MCP tools can implement a backlog under external control:\n")
    w(code("add_dunk(target=\"self\", interval_minutes=60, only_when=\"idle\",\n"
           "         name=\"backlog\",\n"
           "         text=\"Backlog check #{count}: read BACKLOG.md. If every item is done,\\n\"\n"
           "              \"call remove_dunk('{id}') and stop. Otherwise implement the next\\n\"\n"
           "              \"item, commit, and update BACKLOG.md.\")"))
    w("Every hour the daemon types that prompt into the agent's pane (waiting for\n"
      "it to be idle first). The prompt carries its own dunk id, so the future\n"
      "agent can end the loop by removing the dunk. The same agent can read other\n"
      "panes (read_pane), nudge other agents (send_text), or put them on their own\n"
      "schedules (add_dunk with their pane as target).\n")

    w(h(2, "Troubleshooting"))
    w("- \"herdr server not running\": start herdr (`herdr status server`).\n"
      "- \"no herdr pane matches\": run `dunkingsheep panes` and use the pane id.\n"
      "- \"target 'self' needs HERDR_PANE_ID\": the caller is not inside a herdr\n"
      "  pane; pass a pane id or `--self-pane w8:p3`.\n"
      "- Daemon problems: ~/.config/dunkingsheep/daemon.log; `dunkingsheep shutdown`\n"
      "  then any command restarts it with the saved dunks.\n"
      "- Send failed: the pane was closed. Re-target with `update <id> -T ...`.\n")
    return "".join(out)


def _underline(text, level):
    char = {1: "=", 2: "-"}.get(level, ".")
    return f"{text}\n{char * len(text)}\n"


if __name__ == "__main__":
    import sys
    print(guide(markdown="--markdown" in sys.argv), end="")
