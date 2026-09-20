# Dunking Sheep 🐑

Sends text (plus Enter) into [herdr](https://herdr.dev) terminal panes on a
schedule. Keep coding agents moving with "continue", message one agent from
another, or let an agent schedule nudges to *itself*.

Each schedule is a **dunk**: a target pane, a text, an interval, a running
flag. Dunks live in a small background daemon, so the TUI, the CLI, a unix
socket and an **MCP server** (Claude Code, Codex, Gemini CLI, Cursor, …) all
see and control the same dunks. Standard library only; no `pip install`.

## 🎬 Demo

https://github.com/user-attachments/assets/7cb5f9d6-4cab-44e1-82c7-6dd3567b21ac

*Picking an agent pane from the workspace-grouped target list and dunking on it. ([mp4](https://raw.githubusercontent.com/huntergdavis/dunkingsheep/main/dunkingsheep-demo.mp4))*

## 🚀 Quick start

```bash
herdr status server                     # herdr must be running
./dunkingsheep panes                    # what can I target? (* = this pane)
./dunkingsheep add -T "Codex Site" -e 15 -t continue
./dunkingsheep list                     # ids, live status, countdowns
./dunkingsheep tui                      # the curses view of the same dunks
./dunkingsheep mcp-setup claude --apply # let Claude Code drive it
```

## ✨ Capabilities

| Capability | How |
| --- | --- |
| **Four control surfaces** | TUI keys, `dunkingsheep` CLI, JSON-lines unix socket, MCP tools. One command registry behind all of them. |
| **Background daemon** | Auto-started by any client. Dunks survive closing the TUI, persist to `~/.config/dunkingsheep/dunks.json`, and resume running after a restart. |
| **Target by name** | Pane id (`w8:p3`), `self` (the caller's own pane), or a unique substring of a tab, workspace, agent or directory. |
| **Placeholders** | `{id}` `{name}` `{count}` `{target}` `{interval}` `{time}` `{date}` expand at send time. A dunk can tell its recipient which dunk to remove. |
| **Idle gating** | `--only-idle` / `only_when="idle"` holds a send until herdr reports the target agent idle. |
| **Backpressure** | `--skip-if-unconsumed` / `skip_if_unconsumed=true` skips a send (no-op, no error, `skip_count`+1) while the previous one still sits unread in the pane. Stops prompts piling up in an unattended agent. |
| **Never type over you** | On by default. Before each send the daemon reads the target's input box with its styling; bright (non-dim) text after the prompt means someone is mid-sentence, so the send is held and re-checked every minute until the box clears. `--ignore-typing` / `hold_while_typing=false` turns it off. |
| **Max sends** | `--max-sends N` removes the dunk after N sends. `1` is a one-shot delayed nudge. |
| **Build the herd** | `create_workspace`, `create_tab`, `split_pane` (each can run a command like `claude` in the new pane), `start_agent` (herdr-registered by name), `rename` / `focus` / `close` for workspaces, tabs and panes, `list_workspaces`. An admin agent spins up a team, then dunks on it. |
| **Any herdr command** | `herdr <args…>` / `herdr(command=…)` runs any herdr subcommand through the daemon and returns its JSON; `herdr_help` returns herdr's own usage. Only stopping, updating or attaching herdr itself is refused. |
| **Direct messages** | `send <target> <text>` / `send_text` types into any pane now, no dunk. |
| **Pane reading** | `read <target>` / `read_pane` returns a pane's recent output plus its agent and status. |
| **Self-documenting** | `dunkingsheep` (no args), `guide`, `commands --json`, `mcp-setup`, MCP `help` tool, socket `help` command. |

## 🧠 Meta-dunking

Tell Claude (or Codex), with the MCP server registered:

> Implement this backlog. Nudge yourself every 60 minutes; when the backlog is
> done, remove the nudge.

It calls one tool:

```
add_dunk(target="self", interval_minutes=60, only_when="idle",
         skip_if_unconsumed=true, name="backlog",
         text="Backlog check #{count}: read BACKLOG.md. If every item is done,
               call remove_dunk('{id}') and stop. Otherwise implement the next
               item, commit, and update BACKLOG.md.")
```

Every hour the daemon types that into the agent's own pane once it is idle,
and skips the hour if the previous prompt is still unread (so an unattended
session finds one prompt waiting, not four). The prompt carries its own dunk
id, so the future agent can end the loop. The
same agent can `read_pane` other agents, `send_text` them instructions, and
`add_dunk` schedules for them: one agent watching and pacing a whole herd.

## 🔌 MCP server

```bash
./dunkingsheep mcp-setup                 # commands and config blocks for every harness
./dunkingsheep mcp-setup claude --apply  # claude mcp add --scope user dunkingsheep -- python3 …/dunkingsheep mcp
./dunkingsheep mcp-setup codex  --apply  # codex mcp add dunkingsheep -- python3 …/dunkingsheep mcp
./dunkingsheep mcp-setup gemini --apply  # Gemini CLI
./dunkingsheep mcp-setup cursor          # mcpServers JSON for Cursor / Windsurf / VS Code / Claude Desktop
```

Stdio transport, protocol 2024-11-05 through 2025-11-25, flat schemas that
strict validators accept. The harness spawns the server inside the agent's
pane, so `HERDR_PANE_ID` is inherited and `self` means the agent itself.

Tools: `help` `status` `list_panes` `read_pane` `list_dunks` `get_dunk`
`add_dunk` `update_dunk` `remove_dunk` `start_dunk` `stop_dunk` `stop_all`
`fire_dunk` `send_text`. Errors are `isError` results with a plain message;
`shutdown` is not exposed to agents.

## ⌨️ CLI

```bash
dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle] [--skip-if-unconsumed] [--ignore-typing] [--max-sends N] [--no-start]
dunkingsheep list | get <id> | update <id> [-T …] [-e …] [-t …] [--only-idle|--any-time] [--skip-if-unconsumed|--always-send] [--hold-while-typing|--ignore-typing] [--max-sends N]
dunkingsheep start|stop|toggle|remove <id>    stop-all    fire <id>
dunkingsheep send <target> <text…>            read <target> [-l N]
dunkingsheep workspaces | new-workspace -l L [--cwd D] [-r CMD] | new-tab [-w WS] -l L [-r CMD] | split <target> [--down] [-r CMD]
dunkingsheep start-agent <name> [-w WS | --tab T [--split right|down]] -- <command…>
dunkingsheep rename|focus|close workspace|tab|pane <target> [label]
dunkingsheep herdr <herdr args…>              herdr-help [group]
dunkingsheep panes | status | shutdown [--stop-all] | serve | tui | mcp
dunkingsheep guide | commands [--json] | mcp-setup [harness] [--apply]
```

`-e` takes minutes (`15`, `1.5`) or `90s` / `2h`. `--json` anywhere gives
machine-readable output. Errors exit non-zero with the message on stderr.

## 🔧 Socket API

Newline-delimited JSON on `~/.config/dunkingsheep/dunkingsheep.sock`
(`$DUNKINGSHEEP_SOCKET` overrides). Command names and arguments match
`dunkingsheep commands --json`.

```
-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",
    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}
<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, …}}
<- {"id": 1, "ok": false, "error": "no dunk with id 'd9'"}
```

```bash
printf '%s\n' '{"id":1,"cmd":"help"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
```

## 🖥️ TUI

`./run_dunking_sheep.sh` in a herdr tab. Every key is one registry command.

| Key | Action | Key | Action |
| --- | --- | --- | --- |
| `j`/`k` ↑/↓ | move | `c` | choose target pane |
| `a` / `d` | add / remove | `t` | test send now |
| `Space` / `s` | start / stop | `i` / `e` / `n` | interval / text / name |
| `o` / `u` | idle gate / skip-if-unconsumed | `m` | max sends |
| `y` | hold while someone is typing (default on) | | |
| `q` / `Q` | quit view / stop all + shut down daemon | | |

## 🧩 How it works

```
TUI ─┐
CLI ─┼─► unix socket ─► daemon (Flock: timers, persistence) ─► herdr pane send-text + send-keys Enter
MCP ─┘
```

| File | Purpose |
| --- | --- |
| `dunkingsheep` | CLI, plus `serve`, `tui`, `mcp`, `mcp-setup`, `guide` |
| `dunk_core.py` | Dunkers, timers, templates, gating, persistence |
| `dunk_api.py` | Command registry shared by socket, CLI and MCP |
| `dunk_server.py` / `dunk_client.py` | Daemon and client with auto-start |
| `dunk_mcp.py` | MCP server (stdio, stdlib only) |
| `dunk_help.py` | The guide behind every help surface |
| `dunking_sheep_tui.py` | Curses view |
| `herdr_client.py` | Wrapper around the `herdr` CLI |

Docs: [`docs/GUIDE.md`](docs/GUIDE.md) (generated from `dunkingsheep guide`),
[`docs/DESIGN.md`](docs/DESIGN.md), [`docs/RESEARCH.md`](docs/RESEARCH.md).
Tests: `python3 -m unittest discover -s tests`.

## 📋 Requirements

Python 3.8+ and herdr 0.7.4+ with a running server (`herdr` on `PATH` or at
`~/.local/bin/herdr`).

## 🔧 Troubleshooting

- **herdr server not running** — start herdr; check `herdr status server`.
- **no herdr pane matches** — `dunkingsheep panes`, then use the pane id.
- **target 'self' needs HERDR_PANE_ID** — not inside a herdr pane; pass a pane id or `--self-pane`.
- **Daemon** — log at `~/.config/dunkingsheep/daemon.log`; `dunkingsheep shutdown` then any command restarts it with saved dunks.
- **Send failed** — pane was closed; `update <id> -T …` or `c` in the TUI.

---

*Dunking Sheep is to herdr what Dunking Bird is to your desktop. 🐑*

<p align="center">
  <img src="dunkingsheep.png" alt="A sheep slam-dunking a keyboard through a basketball hoop" width="70%"><br>
  <sub><em>required AI slop</em></sub>
</p>
