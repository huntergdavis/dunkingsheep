# Design: Dunking Sheep

## Goal

Reproduce the Dunking Bird TUI feature-for-feature, deliver text through herdr's
socket API instead of the OS input layer, and (since 2.0) let agents control
every faculty through the same mechanism the keyboard uses.

## Components

```
dunkingsheep              CLI entry point: client commands, serve, tui, mcp, mcp-setup, guide
dunk_core.py              Flock + Dunker: state, timers, templates, gating, persistence
dunk_api.py               command registry: names, descriptions, JSON schemas, handlers
dunk_server.py            daemon: unix socket (JSON lines) around one Flock
dunk_client.py            FlockClient: socket client + daemon auto-start
dunk_mcp.py               MCP server over stdio (JSON-RPC 2.0, stdlib only)
dunk_help.py              the guide text shared by --help, guide, MCP and the socket
dunking_sheep_tui.py      curses view; every key is a registry command
herdr_client.py           thin wrapper around the `herdr` CLI (the transport)
text_editor.py            logical-line state for the full-screen text editor
run_dunking_sheep.sh      TUI launcher (ensures ~/.local/bin on PATH)
```

### One mechanism, four surfaces

```
 keyboard ──► TUI ──┐
 shell ──► CLI ─────┼──► FlockClient ──► unix socket ──► FlockServer ──► dispatch() ──► Flock
 agent ──► MCP ─────┘                                                      (dunk_api)
 anything ──► raw JSON lines ─────────────────────────────┘
```

`dunk_api.COMMANDS` is the contract. Each `Command` has a name, a description
written for an agent, `params` (JSON-schema properties with flat types only),
what is `required`, and a handler `(flock, args, self_pane_id)`. The socket
server calls `dispatch()`; the CLI maps subcommands onto command names; the MCP
server publishes every `mcp=True` command as a tool with `input_schema()`.
Adding a faculty means adding one `Command`; all surfaces pick it up, and the
`help` command / `dunkingsheep commands --json` document it automatically.

`self_pane_id` travels with every request because `target: "self"` must mean
the *caller's* pane (the agent's), not the daemon's. Clients default it from
`$HERDR_PANE_ID`, which herdr sets in every pane it spawns; the MCP server
inherits it from the harness that launched it.

### `dunk_core.Flock`

Owns the list of `Dunker`s and their timer threads. Thread-safe (one RLock for
state, one Lock serializing sends across dunkers, as in Dunking Bird).

- `add / update / remove / start / stop / toggle / stop_all / get / list`.
- `fire(id)` — the classic test send; synchronous by default so agents learn
  whether it worked. `send_now(target, text)` — one-off send, no dunk.
- `read_pane(target, lines)` — agents inspect another pane before deciding.
- `resolve_target(spec, self_pane_id)` — pane id, `self`, terminal id, or a
  unique case-insensitive substring of tab/workspace/agent/cwd/title. If several
  panes match but exactly one hosts an agent, that one wins; otherwise the error
  lists the candidates.
- Timer loop per running dunk: countdown against an absolute `next_send_at`
  (so interval edits and daemon restarts are honoured), optional idle gate
  (`herdr pane get` → `agent_status`, re-polled every 5 s while `working` or
  `blocked`), send under the shared `send_lock`, reschedule, persist, and
  self-remove when `max_sends` is reached. `stop_event.wait()` instead of
  `sleep()` makes stop immediate.
- Templates: only the known placeholders are substituted (regex), so JSON or
  code braces in a prompt survive intact.
- Persistence: `dunks.json` written atomically on every structural change and
  after every send (`send_count`, `next_send_at`). `Flock.close()` halts threads
  but keeps `running=True` so the next daemon resumes them; an overdue send
  fires after a short grace period.

### `dunk_server.FlockServer` and `dunk_client.FlockClient`

Protocol: newline-delimited JSON, many requests per connection,
`{"id", "cmd", "args", "self_pane_id"}` → `{"id", "ok", "result" | "error"}`.
Errors are always replies, never dropped connections. Binding reclaims a stale
socket file (nobody listening) and refuses when a live daemon holds it.
`shutdown` replies first, then stops; SIGTERM/SIGINT do the same.

The client opens a connection per call (cheap on a unix socket, no reconnect
logic) and `connect_or_start()` spawns `dunkingsheep serve` detached
(`start_new_session`, output to `daemon.log`, `HERDR_PANE_ID` scrubbed so the
daemon never thinks it *is* a pane) and waits for the socket.

### `dunk_mcp.McpServer`

A hand-written MCP stdio server: newline-delimited JSON-RPC 2.0, `initialize`
(negotiates 2024-11-05 … 2025-11-25 and returns the guide as `instructions`),
`tools/list`, `tools/call`, `ping`, and empty `resources/prompts` lists so
harnesses that probe them are happy. Tool failures are `isError: true` results
with the `DunkError` message, never JSON-RPC errors, because agents can read and
recover from the former. `structuredContent` is included only for protocol
versions that define it. Schemas use only string/number/integer/boolean (no
unions, no null enums) so OpenAI/Codex and Gemini validators accept them.

### `dunking_sheep_tui.DunkingSheepTui`

Now a client. Each frame (200 ms) it fetches `list_dunks` and redraws; keys map
1:1 onto registry commands (`a`→`add_dunk`, `d`→`remove_dunk`, `space`→
`toggle_dunk`, `c`→`update_dunk(target)`, `t`→`fire_dunk(countdown_s=2,
wait=false)`, `i/e/n/o/m`→`update_dunk`). `q` closes the view; `Q` sends
`shutdown(stop_all=true)`. If the daemon cannot be started at all, the TUI falls
back to an in-process `Flock` (clearly labelled, not shared) so it still works.
Modals (line input, workspace-grouped target picker, logical-line text editor)
are unchanged from 1.x.

## Why a daemon

Agents create dunks from processes that come and go; a dunk that dies with its
creator is useless as an external control loop. Putting the flock in a daemon
also makes the TUI optional, lets several surfaces see one truth, and makes
persistence natural.

## Preserved behavior details

- Interval parsing with a helpful error; live `Next: MM:SS` countdown.
- Test send with a 2-second countdown from the TUI.
- Full-screen logical-line text editor (Ctrl+G save / Esc cancel).
- `send_lock` serializes sends across dunkers; chunked `send-text` and a settle
  pause before `Enter` (see `herdr_client.py`) so Codex/Claude submit reliably.
- Defensive `safe_addstr` / `clip` so a small terminal never crashes curses.

## Non-goals

- A TCP/HTTP transport. The unix socket is local by design; the MCP server is
  the remote-friendly surface (harnesses spawn it over stdio).
- Exposing `shutdown` to agents.
