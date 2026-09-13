# Dunking Sheep 2.0.0 guide

Dunking Sheep sends text (followed by Enter) into herdr terminal panes on a
schedule. It exists to keep AI coding agents moving ("continue"), to let
agents message each other, and to let an agent schedule future nudges to
itself. Everything below is the same whether you drive it from the keyboard,
the command line, a unix socket, or the Model Context Protocol (MCP).

## Concepts

- dunk / dunker: one schedule. Fields: id (d1, d2, ...), name, target pane,
  text, interval_minutes, running, only_when, max_sends, send_count, status.
- target: a herdr pane. Give a pane id (w8:p3), the word self (the pane the
  caller runs in, from $HERDR_PANE_ID), a terminal id, or a unique
  case-insensitive substring of a tab label, workspace label, agent name or
  directory. An exact tab/workspace/agent label wins over a substring; among
  several substring matches a lone agent pane wins; anything still ambiguous
  is rejected with the candidates listed.
- send: the daemon runs `herdr pane send-text` then `herdr pane send-keys
  Enter` (chunked for long texts). Nothing touches the OS keyboard.
- daemon: `dunkingsheep serve`, auto-started by any client. Listens on a unix
  socket at ~/.config/dunkingsheep/dunkingsheep.sock ($DUNKINGSHEEP_SOCKET).
  Logs to ~/.config/dunkingsheep/daemon.log.
- persistence: dunks are saved to ~/.config/dunkingsheep/dunks.json on every
  change and restored, still running, when the daemon next starts. A dunk
  whose send came due while the daemon was down fires shortly after restart.

## Text placeholders

These expand at send time; any other braces are left untouched:

```
{id}        the dunk's id, e.g. d3      {name}      its name (or id)
{count}     this send's number (1, 2..) {target}    tab / agent label
{interval}  minutes between sends       {time} {date}  local clock
```

So a dunk can say who it is: "...when the backlog is finished, remove
dunk {id}" tells the receiving agent exactly which dunk to delete.

## Orchestration options

- only_when = idle: hold each send until herdr reports the target agent as
  idle (not working or blocked); re-checked every 5 seconds. Panes without a
  detected agent count as idle.
- max_sends = N: the dunk removes itself after its N-th send. N = 1 is a
  one-shot delayed nudge ("in 30 minutes, tell me to check CI").
- skip_if_unconsumed = true: backpressure. Before each send the daemon
  decides whether the previous send was picked up: it was if the target was
  ever seen working/blocked since then (sampled every 5 s while waiting), or
  if the sent text is no longer sitting in the pane's last lines. Otherwise
  the send is skipped as a no-op: send_count unchanged, no error, skip_count
  +1, status 'Skipped (unconsumed)', next send rescheduled as usual. The
  first send always goes. Composes with only_when. Turn it on for self-dunks
  and unattended agents, where four hourly prompts would otherwise queue in
  the input box and flush into one turn. Blind spots: a turn that starts and
  ends inside one 5 s sampling gap is only caught by the tail check, and the
  tail check is a heuristic (a transcript echo of the prompt can look like
  unread input). Meant for agent panes, not shells.
- start = false: create the dunk stopped; start it later.
- Changing the interval of a running dunk reschedules its next send.

## Surface 1: command line

```
dunkingsheep panes                          list targets (* marks this pane)
dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle]
                 [--skip-if-unconsumed] [--max-sends N] [--no-start]
                 -e takes 15, 90s, 1.5h
dunkingsheep list | get <id> | update <id> ... | remove <id>
dunkingsheep start|stop|toggle <id>    stop-all    fire <id> (send now)
dunkingsheep send <target> <text...>   one-off send, no dunk
dunkingsheep read <target> [-l N]      last N lines of a pane
dunkingsheep status | shutdown [--stop-all] | serve | tui | mcp
dunkingsheep commands [--json]         the full command registry
Add --json to any command for machine-readable output.
```

## Surface 2: unix socket API

Newline-delimited JSON, any number of requests per connection. The command
names and arguments are exactly the registry from `dunkingsheep commands`.

```
-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",
    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}
<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, ...}}
<- {"id": 1, "ok": false, "error": "no dunk with id 'd9'"}

# from a shell:
printf '%s\n' '{"id":1,"cmd":"list_dunks"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
# discover everything, including JSON schemas:
printf '%s\n' '{"id":1,"cmd":"help"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
```

`self_pane_id` is optional; it is what target "self" resolves to. Clients
normally pass their own $HERDR_PANE_ID.

## Surface 3: MCP server (Claude Code, Codex, Gemini CLI, Cursor, ...)

`dunkingsheep mcp` is a Model Context Protocol server over stdio (newline-
delimited JSON-RPC, protocol versions 2024-11-05 through 2025-11-25, tools
only, standard library only). Every registry command marked for MCP is a
tool with a flat JSON schema (string/number/integer/boolean, no unions), so
strict validators such as OpenAI's accept it. `shutdown` is not exposed.

`dunkingsheep mcp-setup` prints the exact registration for each harness and
`dunkingsheep mcp-setup <harness> --apply` runs it when that harness has a CLI:

```
claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp
codex mcp add dunkingsheep -- python3 /path/to/dunkingsheep mcp
gemini mcp add --scope user dunkingsheep python3 /path/to/dunkingsheep mcp

# any harness that reads an mcpServers JSON block (Cursor, Windsurf, Claude
# Desktop, VS Code, ...):
{"mcpServers": {"dunkingsheep": {"command": "python3",
                                 "args": ["/path/to/dunkingsheep", "mcp"]}}}

# ~/.codex/config.toml equivalent:
[mcp_servers.dunkingsheep]
command = "python3"
args = ["/path/to/dunkingsheep", "mcp"]
```

The harness spawns the MCP server inside the agent's own herdr pane, so the
server inherits HERDR_PANE_ID and target "self" means the agent itself. If a
harness scrubs the environment, pass `--self-pane <pane_id>` after `mcp` or
set HERDR_PANE_ID in the server's env block.

Tools: help, status, list_panes, read_pane, list_dunks, get_dunk, add_dunk,
update_dunk, remove_dunk, start_dunk, stop_dunk, stop_all, fire_dunk,
send_text. Call `help` first if unsure; `initialize` also returns these
instructions. Errors come back as tool results with isError=true and a
plain-English message (never as protocol errors), so the agent can recover.

## Surface 4: the TUI

`dunkingsheep tui` (or ./run_dunking_sheep.sh) shows the same dunks live.
Keys: j/k move, a add, d remove, c choose target, t test send, i interval,
e text, n name, o toggle idle-gate, m max sends, space/s start-stop, q quit
(dunks keep running), Q stop every dunk and shut the daemon down.

## Recipe: one agent watching and steering many

Because every pane is addressable, one agent can supervise a herd:

```
list_panes()                                  # who is running where, idle or working
read_pane(target="Codex Site", lines=80)      # what is that agent doing?
send_text(target="Codex Site", text="Stop and write tests first.")
add_dunk(target="Melt Squad", interval_minutes=20, only_when="idle",
         text="Progress report: what changed since the last one?")
```

send_text is a direct message with no schedule attached; read_pane is the
eyes. Together with add_dunk they let an agent communicate with, monitor and
pace several projects in parallel.

## Recipe: meta-dunking (an agent schedules itself)

An agent with the MCP tools can implement a backlog under external control:

```
add_dunk(target="self", interval_minutes=60, only_when="idle",
         skip_if_unconsumed=True, name="backlog",
         text="Backlog check #{count}: read BACKLOG.md. If every item is done,\n"
              "call remove_dunk('{id}') and stop. Otherwise implement the next\n"
              "item, commit, and update BACKLOG.md.")
```

Every hour the daemon types that prompt into the agent's pane (waiting for
it to be idle first, and skipping the hour entirely if the previous prompt
is still sitting unread). The prompt carries its own dunk id, so the future
agent can end the loop by removing the dunk. The same agent can read other
panes (read_pane), nudge other agents (send_text), or put them on their own
schedules (add_dunk with their pane as target).

## Troubleshooting

- "herdr server not running": start herdr (`herdr status server`).
- "no herdr pane matches": run `dunkingsheep panes` and use the pane id.
- "target 'self' needs HERDR_PANE_ID": the caller is not inside a herdr
  pane; pass a pane id or `--self-pane w8:p3`.
- Daemon problems: ~/.config/dunkingsheep/daemon.log; `dunkingsheep shutdown`
  then any command restarts it with the saved dunks.
- Send failed: the pane was closed. Re-target with `update <id> -T ...`.

