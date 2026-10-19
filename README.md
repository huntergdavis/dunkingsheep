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
| **Idle gating** | `--only-idle` / `only_when="idle"` holds a send until herdr reports the target agent idle. On by default for a new schedule aimed at a pane that has an agent, since typing into an agent mid-turn can abort that turn. `--any-time` opts out. |
| **Backpressure** | `--skip-if-unconsumed` / `skip_if_unconsumed=true` skips a send (no-op, no error, `skip_count`+1) while the previous one still sits unread in the pane. Stops prompts piling up in an unattended agent. See [`docs/BACKPRESSURE.md`](docs/BACKPRESSURE.md). |
| **Never type over you** | On by default, for scheduled dunks, fired dunks, direct messages and key presses. The daemon reads the target's input box with its styling; bright (non-dim) text after the prompt means someone is mid-sentence. It waits, and sends only once the box has read clear twice a second apart **and once more at the instant of sending**, with the send lock held. `--ignore-typing` / `hold_while_typing=false` turns it off. |
| **Dialogs are yours** | While an agent shows an approval prompt or panel, the daemon types nothing and presses nothing in that pane, and says so in `messages`. Answering it would choose on your behalf and, in a working pane, interrupt the turn. Delivery resumes once you answer. |
| **Confirmed submit** | One Enter does not reliably submit. Claude takes it first time at any size; Codex needs the pane quiet first, longer the bigger the paste. The daemon reads the box back and presses Enter again, with growing gaps, while its own text is still there. Measured on Codex: 0.4s short, 4s at 600 chars, 19s at 2.2k. A send that never leaves the box is reported failed, not sent; `flush <target>` submits text stranded by anything else. |
| **Max sends** | `--max-sends N` removes the dunk after N sends. `1` is a one-shot delayed nudge. |
| **Build the herd** | `create_workspace`, `create_tab`, `split_pane` (each can run a command like `claude` in the new pane), `start_agent` (herdr-registered by name), `rename` / `focus` / `close` for workspaces, tabs and panes, `list_workspaces`. An admin agent spins up a team, then dunks on it. |
| **Any herdr command** | `herdr <args…>` / `herdr(command=…)` runs any herdr subcommand through the daemon and returns its JSON; `herdr_help` returns herdr's own usage. Refused: stopping/updating/attaching herdr itself, and the four subcommands that type into a pane (use `send_text` / `send_keys`, which wait for the human). |
| **herdr guard** | `dunkingsheep guard install` replaces the `herdr` binary with a wrapper (the real one becomes `herdr-real`) that routes `pane run`, `pane send-text`, `pane send-keys` and `agent send` through the daemon. Shadowing the binary, not just PATH, is what reaches agents herdr exec's itself, which inherit the server's environment and never read a shell profile. Everything else execs the real herdr untouched. |
| **Inboxes** | `inbox_send` / `dunkingsheep inbox send` stores a message for an agent to read with `inbox_read`. The only thing typed is one short line naming the inbox, sender and subject, so nothing of the message itself goes into the terminal: no Enter failures, no stranding, no collision with a human, no size limit, and it survives a daemon restart. Attachments supported. Every send returns a stable id that `inbox_status` tracks without exposing the message text. A mailbox identifies one pane by its label, tab label or pane id, never by agent type. |
| **Direct messages** | `send <target> <text>` / `send_text` types into any pane, no dunk. Queues behind a human who is typing, exactly like a dunk, and delivers in order when they finish. `messages` / `list_messages` show the queue; `cancel-message` drops one. |
| **Operations metrics** | `metrics` / `dunkingsheep metrics --window 3600` returns a versioned, self-describing JSON snapshot: counts by sender, recipient, pane and source, duration distributions for queue wait, delivery and handling, the Enter-retry histogram, live queue depth, and a coverage block. Identifiers and numbers only, never message text. Durations are null rather than zero when unmeasured. |
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

## 🧵 Global task queue (deterministic coordination)

Tasks are the fleet's shared work queue, stored by the daemon in SQLite and
served over the same CLI / socket / MCP surfaces. Selection, ordering,
dependency gating, atomic claims, leases and requeueing are deterministic
algorithms — no model in the loop.

- **Scope**: `global` (fleet-wide, the default) or a pane id (private to that
  pane and its agent).
- **Fields**: id, title, body, status (`open|in_progress|blocked|done|cancelled`),
  priority (`urgent > high > medium > low`), assignees, `parent_id` subtasks,
  `depends_on` order, attempts, and a lease.
- **Global completion**: any assignee completing a shared task completes it for
  everyone.
- **Atomic claim**: `task_next` selects, assigns the caller and sets
  `in_progress` in one `BEGIN IMMEDIATE` transaction, so two agents never take
  the same exclusive task.
- **Leases**: 1800s by default; `task_heartbeat` extends one. Expired or
  dead-pane leases are requeued automatically.
- **Push**: `task_follow` has the daemon push the next task to an idle pane,
  reusing the same idle gate and guarded inbox nudge as dunks. An empty
  `task_next` subscribes the caller, so agents do not poll.
- **Inbox**: `inbox_read` returns the reader's global and own-pane tasks after
  the messages.
- **Metrics**: `task_metrics` is an append-only, timeseries-friendly query
  endpoint: queue depth, status/priority/scope mix, per-assignee load,
  created/claimed/completed/cancelled/requeued throughput in hourly or daily
  buckets, and completion/claim latency (avg/p50/p90/max). Scope `all`, `global`,
  `self` or a pane id.

```bash
dunkingsheep task add "Ship the parser" -p high
dunkingsheep task next                 # atomic claim -> in_progress + lease
dunkingsheep task heartbeat t12
dunkingsheep task complete t12         # done for everyone
dunkingsheep task follow               # pushed work when idle
dunkingsheep task list --all
```

MCP tools: `task_add`, `task_list`, `task_read`, `task_update`, `task_assign`,
`task_complete`, `task_block`, `task_unblock`, `task_cancel`, `task_delete`,
`task_next`, `task_heartbeat`, `task_release`, `task_follow`, `task_unfollow`,
`task_followers`, `task_stats`, `task_metrics`.

## 🧠 Memory and recall service (v1)

SQLite + FTS5 in WAL mode: one background ingest writer, a pool of read-only
query connections. A query never reindexes, never swaps an index, and never
waits for ingest — the fix for the 2026-10-18 stalls.

```bash
dunkingsheep memory recall "query" --limit 10
dunkingsheep memory context "query" --sessions 5
dunkingsheep memory search "query" --harness codex --project poolrad
dunkingsheep memory stats          # p50/p95/p99, chunks, DB bytes, queue
dunkingsheep memory status
dunkingsheep memory ingest <path>  # superuser; async
```

MCP tool: `memory(action=recall|context|search|stats|status|ingest|backfill)`.
The service is scoped to 1–10 M chunks on this
box; billions is a later sharded/remote tier. The pi/Claude/opencode/Codex agents now use memory as the **only** recall
path; the legacy index (deja) is retired. See
[`docs/MEMORY_MIGRATION.md`](docs/MEMORY_MIGRATION.md) and
[`docs/MEMORY_SERVICE_ADR.md`](docs/MEMORY_SERVICE_ADR.md).

Comparison harness (memory-only by default; `--compare-deja` is archival;
rollback proof): `python3 tools/memory_parity.py`. Deja-free audit:
`python3 tools/check_deja_free.py`. Acceptance: `python3 tools/memory_acceptance.py`.

## ♻️ Self-sustaining (t12)

systemd owns the daemon (`Restart=always`), and a deterministic
`dunkingsheep health [--repair] [--deep]` check watches the memory DB, stale
leases, WAL size and disk space. No model is in the loop: every threshold is a
constant and every repair is fixed. When the unit is installed the client never
spawns a second daemon — it asks systemd — so a client auto-start can never race
the supervisor for the socket.

```bash
dunkingsheep health              # fast, read-only check
dunkingsheep health --repair     # requeue stale leases, checkpoint a large WAL
bash tools/self_heal_test.sh     # kill -9 -> automatic restart -> no data loss
```

Install the units from [`packaging/systemd/`](packaging/systemd/) (daemon +
10-minute health timer + weekly deep check). Details:
[`docs/SELF_SUSTAINING.md`](docs/SELF_SUSTAINING.md).

## 🔒 Superusers and destructive operations

Only panes and names on the superuser allowlist may close a pane/tab/workspace,
stop the daemon, manage dunks, spawn agents, or use the raw herdr passthrough.
Everyone else communicates through the inbox and task queue, and may type only
into their own pane. This is the response to the 2026-10-18 incident where a
free model's cleanup closed the VP's tab and took nine panes with it.

```bash
dunkingsheep superuser status
dunkingsheep superuser add <pane-or-name>
dunkingsheep superuser remove <pane-or-name>
dunkingsheep superuser recover    # setup/takeover: add this pane + VP name
```

The allowlist lives in `~/.config/dunkingsheep/superusers.json` (0600). Pane
ids are exact; names match a pane's own label, tab label or workspace label, so
the VP is recognised after a pane-id change. An empty allowlist refuses every
destructive operation; the first `superuser recover` bootstraps it.

Even a superuser cannot close the pane/tab/workspace it is running in without
`close --force`. An agent started without an explicit workspace/tab is placed
in the dedicated `sandbox` workspace, never the caller's tab. The installed
`herdr` guard routes `pane close`, `pane kill`, `tab close`, `workspace close`
and layout mutations through the daemon (fail-closed: it never falls back to
the real herdr on a refusal).

## 📥 Setting agents up

```bash
./dunkingsheep install                   # put the CLI on PATH as a plain command
./dunkingsheep mcp-setup claude --apply  # register the MCP server
```

`install` matters: herdr starts agents by exec'ing them from its server process,
so they inherit ~/.local/bin on PATH but not this project. Without the launcher
an agent must invoke the CLI by its workspace path, which a sandboxed agent
refuses as external code. Add `Bash(dunkingsheep *)` to the allow list in
`~/.claude/settings.json` so auto mode runs it without prompting.

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

The declared surface is **one namespaced tool per domain**, each taking an
`action` (t8): `inbox`, `task`, `dunk`, `pane`, `workspace`, `superuser`,
`memory`, `metrics`, `system`. That is 70 flat tools merged into 9 (schema
42,792 → 15,609 bytes, -63.5%); the hot set `inbox`/`task`/`memory` is 5,583
bytes. **Nothing was removed**: every old flat name still resolves inside the
server, and every command stays on the CLI and socket API. See the full
[`docs/MCP_TOOL_MAP.md`](docs/MCP_TOOL_MAP.md) or `dunkingsheep mcp-map`.

Errors are `isError` results with a plain message; `shutdown` is not exposed
to agents.

## ⌨️ CLI

```bash
dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle] [--skip-if-unconsumed] [--ignore-typing] [--max-sends N] [--no-start]
dunkingsheep list | get <id> | update <id> [-T …] [-e …] [-t …] [--only-idle|--any-time] [--skip-if-unconsumed|--always-send] [--hold-while-typing|--ignore-typing] [--max-sends N]
dunkingsheep start|stop|toggle|remove <id>    stop-all    fire <id>
dunkingsheep send <target> <text…> [--wait S] [--ignore-typing]   read <target> [-l N]
dunkingsheep send-keys <target> <keys…> [--wait S] [--ignore-typing]
dunkingsheep inbox count|read|list|stats | inbox send <to> <body…> [-s subject] [-a file]
dunkingsheep inbox status <id> | ack|cancel|delete <id…> | fetch <attachment-id> [-d dir]
dunkingsheep messages | cancel-message <id> | flush <target> | guard [status|install|check|uninstall]
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
[`docs/DESIGN.md`](docs/DESIGN.md), [`docs/RESEARCH.md`](docs/RESEARCH.md),
[`docs/MEMORY_MIGRATION.md`](docs/MEMORY_MIGRATION.md),
[`docs/SELF_SUSTAINING.md`](docs/SELF_SUSTAINING.md).
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
