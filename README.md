# Dunking Sheep 🐑

A terminal-first automation tool that sends text to **herdr** panes at regular
intervals. Perfect for keeping coding agents engaged with prompts like
"continue" or "keep going" — without touching your keyboard or fighting window
focus.

**New in 2.0: agents can drive it.** Every dunk lives in a small background
daemon, and the same commands are available from the keyboard (TUI), the
command line, a unix socket, and an **MCP server** that plugs into Claude Code,
OpenAI Codex, Gemini CLI, Cursor and any other MCP harness. An agent can list
dunks, create dunks for other agents, and dunk on *itself* — an external,
repeating control loop for one agent or a whole herd of them.

Dunking Sheep is a [herdr](https://herdr.dev)-native sibling of
[Dunking Bird](../dunkingbird). Instead of capturing an OS window and typing
through `ydotool`, it targets a herdr **pane** and delivers text over herdr's
socket API.

## 🎬 Demo

https://github.com/user-attachments/assets/7cb5f9d6-4cab-44e1-82c7-6dd3567b21ac

*Picking an agent pane from the workspace-grouped target list and dunking on it. ([download the mp4](https://raw.githubusercontent.com/huntergdavis/dunkingsheep/main/dunkingsheep-demo.mp4))*

## 🚀 Release 2.0 — Agent Control

| Surface | How | Who uses it |
| --- | --- | --- |
| **TUI** | `./run_dunking_sheep.sh` or `dunkingsheep tui` | you, in a herdr tab |
| **CLI** | `dunkingsheep add -T self -e 60 -t "..."` | you, scripts, cron, agents in a shell |
| **Socket API** | JSON lines on `~/.config/dunkingsheep/dunkingsheep.sock` | anything that can open a unix socket |
| **MCP server** | `dunkingsheep mcp` (stdio) | Claude Code, Codex, Gemini CLI, Cursor, … |

All four call the same command registry on the same daemon, so a dunk created
by Claude shows up in your TUI with a live countdown, and a dunk you add in the
TUI is visible to Codex.

### What's new

- **Background daemon.** Dunks keep running after you close the TUI. The daemon
  is started automatically by any client and persists dunks to
  `~/.config/dunkingsheep/dunks.json`, resuming them (still running) after a
  restart or reboot.
- **`dunkingsheep` CLI** with `--json` output on every command and a
  self-documenting `--help`, `guide`, `commands` and `mcp-setup`.
- **Unix socket API** with a `help` command that returns the guide, the
  protocol, and every command's JSON schema.
- **MCP server** (standard library only, protocol 2024-11-05 → 2025-11-25) with
  14 tools and flat schemas that strict validators accept.
- **Target by name:** `self`, a pane id, or a unique substring of a tab,
  workspace, agent or directory.
- **Text placeholders:** `{id}` `{name}` `{count}` `{target}` `{interval}`
  `{time}` `{date}` expand at send time, so a dunk can tell its recipient which
  dunk it came from.
- **Idle gating** (`--only-idle`): hold a send until herdr reports the target
  agent idle.
- **Max sends** (`--max-sends N`): the dunk removes itself after N sends;
  `1` is a one-shot delayed nudge.
- **One-off sends and pane reading:** `dunkingsheep send <target> <text>` and
  `dunkingsheep read <target>` (also MCP tools `send_text` / `read_pane`).
- **TUI additions:** `n` name, `o` toggle idle gate, `m` max sends, `Q` stop
  every dunk and shut the daemon down. `q` now just closes the view.

## 🧠 Meta-dunking: an agent that schedules itself

With the MCP server registered, you can say to Claude (or Codex):

> I want this backlog implemented. Create a dunk that nudges you every 60
> minutes; when the backlog is complete, the dunk should remove itself.

The agent calls one tool:

```
add_dunk(target="self", interval_minutes=60, only_when="idle", name="backlog",
         text="Backlog check #{count}: read BACKLOG.md. If every item is done,
               call remove_dunk('{id}') and stop. Otherwise implement the next
               item, commit, and update BACKLOG.md.")
```

Every hour the daemon types that prompt into the agent's own pane (waiting until
the agent is idle). The prompt carries its own dunk id, so the future agent can
end the loop by removing the dunk. The same agent can `read_pane` other agents,
`send_text` them a message, or put them on their own schedules — orchestration of
many agents through herdr and Dunking Sheep, with no extra infrastructure.

## 📋 Requirements

- **Python 3.8+** (standard library only — no `pip install` needed)
- **[herdr](https://herdr.dev)** 0.7.4 or newer with a running server
  (`herdr` on `PATH` or at `~/.local/bin/herdr`)

```bash
herdr status server        # should report status: running
```

## 🔌 Register the MCP server

```bash
./dunkingsheep mcp-setup                 # prints the command/config for every harness
./dunkingsheep mcp-setup claude --apply  # Claude Code (user scope)
./dunkingsheep mcp-setup codex --apply   # OpenAI Codex CLI
./dunkingsheep mcp-setup gemini --apply  # Gemini CLI
./dunkingsheep mcp-setup cursor          # JSON block for Cursor/Windsurf/VS Code/Claude Desktop
```

Under the hood these are just:

```bash
claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp
codex  mcp add dunkingsheep -- python3 /path/to/dunkingsheep mcp
```

Verify with `claude mcp list` / `codex mcp list`. Because the harness spawns the
server inside the agent's own herdr pane, it inherits `HERDR_PANE_ID` and the
target `self` means the agent itself.

MCP tools: `help`, `status`, `list_panes`, `read_pane`, `list_dunks`,
`get_dunk`, `add_dunk`, `update_dunk`, `remove_dunk`, `start_dunk`,
`stop_dunk`, `stop_all`, `fire_dunk`, `send_text`. Errors come back as tool
results with `isError: true` and a plain-English message so the agent can
recover; `shutdown` is deliberately not exposed to agents.

## ⌨️ Command line

```bash
./dunkingsheep                          # help + quick start
./dunkingsheep panes                    # targets (* marks the pane you're in)
./dunkingsheep add -T "Codex Site" -e 15 -t continue
./dunkingsheep add -T self -e 30 --max-sends 1 -t "check CI now"
./dunkingsheep add -T w8:p3 -e 1.5h --only-idle -n hourly -t "status report please"
./dunkingsheep list                     # ids, live status, countdowns
./dunkingsheep update d2 -e 45 --any-time
./dunkingsheep fire d2                  # send now (the TUI's test send)
./dunkingsheep send "Claude Proj" please summarize your progress
./dunkingsheep read "Claude Proj" -l 60
./dunkingsheep stop d2 | start d2 | toggle d2 | stop-all | remove d2
./dunkingsheep status | shutdown [--stop-all]
./dunkingsheep guide                    # the full guide (also: docs/GUIDE.md)
./dunkingsheep commands --json          # every command with its JSON schema
```

`-e` accepts minutes (`15`, `1.5`) or `90s` / `2h`. Add `--json` anywhere for
machine-readable output; errors exit non-zero with the message on stderr.

## 🔧 Socket API

Newline-delimited JSON on `~/.config/dunkingsheep/dunkingsheep.sock`
(`$DUNKINGSHEEP_SOCKET` to override). Command names and arguments are exactly
the registry shown by `dunkingsheep commands`.

```
-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",
    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}
<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, ...}}
<- {"id": 1, "ok": false, "error": "no dunk with id 'd9'"}
```

```bash
printf '%s\n' '{"id":1,"cmd":"help"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
```

## 🖥️ TUI

Open a tab in herdr and run `./run_dunking_sheep.sh` (or `./dunkingsheep tui`).
It shows the daemon's dunks live and every key is one registry command:

| Key | Action |
| --- | --- |
| `j` / `k` / ↑ / ↓ | Move selection |
| `a` | Add a dunker |
| `d` | Remove selected dunker |
| `c` | Choose target pane (grouped by workspace; `(this)` marks your pane) |
| `t` | Test send now |
| `i` | Edit interval (`10`, `90s`, `1.5h`) |
| `e` | Edit text to send (`Ctrl+G` saves, `Esc` cancels) |
| `n` | Name the dunker |
| `o` | Toggle only-when-idle gating |
| `m` | Set max sends (blank = forever) |
| `Space` / `s` | Start / stop |
| `q` / `Esc` | Quit the view (dunks keep running) |
| `Q` | Stop every dunk and shut the daemon down |

## 🧩 How it works

```
 TUI (curses)  ─┐
 dunkingsheep CLI ─┼─► unix socket ─► daemon (Flock) ─► herdr CLI ─► herdr server ─► pane
 MCP server (stdio) ┘        JSON lines        timers, persistence      send-text / send-keys Enter
```

- `dunk_core.py` — the `Flock`: dunkers, timers, templates, idle gating,
  persistence. The single mechanism every surface calls.
- `dunk_api.py` — the command registry (names, descriptions, JSON schemas,
  handlers) shared by the socket, CLI and MCP.
- `dunk_server.py` / `dunk_client.py` — the daemon and its client (with
  auto-start).
- `dunk_mcp.py` — MCP over stdio, standard library only.
- `dunkingsheep` — the CLI; `dunking_sheep_tui.py` — the curses view.
- `herdr_client.py` — the herdr transport: `herdr pane send-text` then
  `herdr pane send-keys <pane> Enter`, chunked for long texts.

See [`docs/GUIDE.md`](docs/GUIDE.md) (generated from `dunkingsheep guide`),
[`docs/DESIGN.md`](docs/DESIGN.md) and [`docs/RESEARCH.md`](docs/RESEARCH.md).

## 🧪 Tests

```bash
python3 -m unittest discover -s tests
```

Covers the core state machine (with a fake herdr), the socket round trip, the
MCP protocol over in-memory streams, and an end-to-end CLI run that auto-starts
a real daemon against a fake `herdr` binary. No test touches your real herdr or
`~/.config/dunkingsheep`.

## 🔧 Troubleshooting

**"herdr server not running - start herdr"** — start herdr, verify with
`herdr status server`.

**"no herdr pane matches ..."** — run `dunkingsheep panes` and use the pane id,
or a longer substring; ambiguous names list the candidates.

**"target 'self' needs HERDR_PANE_ID"** — the caller is not inside a herdr
pane. Pass a pane id, or `--self-pane w8:p3`.

**Daemon problems** — `~/.config/dunkingsheep/daemon.log`. `dunkingsheep
shutdown` stops it; the next command restarts it with the saved dunks.

**Send failed** — the target pane was closed. Re-target with
`dunkingsheep update <id> -T ...` or press `c` in the TUI.

## 📄 Files

| File | Purpose |
| --- | --- |
| `dunkingsheep` | CLI: client commands, `serve`, `tui`, `mcp`, `mcp-setup`, `guide` |
| `dunk_core.py` | The flock of dunkers and their timers (the shared mechanism) |
| `dunk_api.py` | Command registry shared by socket, CLI and MCP |
| `dunk_server.py` | Daemon: unix socket server around the flock |
| `dunk_client.py` | Socket client with daemon auto-start |
| `dunk_mcp.py` | MCP server (stdio, stdlib only) |
| `dunk_help.py` | The guide text used by `--help`, `guide`, MCP and the socket |
| `dunking_sheep_tui.py` | The curses TUI (a live view onto the daemon) |
| `herdr_client.py` | Wrapper around the `herdr` CLI (the transport) |
| `text_editor.py` | Logical-line buffer for the TUI text editor |
| `run_dunking_sheep.sh` | TUI launcher |
| `docs/GUIDE.md` | The full guide (generated) |
| `docs/DESIGN.md` | Architecture and the Bird→Sheep mapping |
| `docs/RESEARCH.md` | Study of Dunking Bird + the herdr API |

---

*Dunking Sheep is to herdr what Dunking Bird is to your desktop. 🐑*

---

<p align="center">
  <img src="dunkingsheep.png" alt="A sheep slam-dunking a keyboard through a basketball hoop" width="70%"><br>
  <sub><em>required AI slop</em></sub>
</p>
