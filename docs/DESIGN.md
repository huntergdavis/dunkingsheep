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
- Backpressure (`skip_if_unconsumed`, off by default). The idle gate cannot
  see unread input: a pane sitting at a prompt with text queued in its input
  box reports `idle`, so on 2026-09-12 a 240-minute self-dunk landed four
  prompts in one unattended Claude pane and they flushed into a single turn.
  The first fix inferred consumption from agent busyness and a verbatim tail
  match; both failed in the field (0b8a113): busyness on an unrelated turn
  latched the newest send as taken, and Claude Code collapses a long queued
  paste into a placeholder (`Pasted text`, `+N lines (ctrl+o to expand)`), so
  the verbatim needle was never present and every send scored as consumed. It
  never skipped once in production.
- The mechanism now reads the pane. At send time (after the idle gate, so the
  box has settled) `_previous_send_consumed` reads the target's visible screen
  and, via `_input_area` + `_input_pending`, isolates the input-box region as
  everything from the last prompt-sigil line (Claude `❯`, Codex `›`) to the
  bottom. That excludes the transcript, so a transcript-collapse chip like
  `+28 lines (ctrl + t to view transcript)` is not mistaken for queued input.
  The previous send is still pending if the box holds a collapsed-input marker
  (`COLLAPSED_INPUT_MARKERS`) or the sent text's whitespace-collapsed prefix.
  A pending box, an unreadable pane, or an unrecognisable box all hold the
  send: for this opt-in feature "not sure" means skip, because a missed nudge
  is recoverable next interval and a stack is not. A skip reschedules
  `next_send_at`, bumps `skip_count`, sets status `Skipped (unconsumed)` and
  leaves `send_count`, `last_sent_at` and `last_error` alone. Busyness is no
  longer consulted for consumption, and there is no extra polling thread: the
  one pane read happens inline at send time. Blind spots: it recognises our own
  queued send (verbatim, or the collapse placeholder a long one becomes) but not
  arbitrary short text a human left in the box, since agents render rotating
  idle hints there that are indistinguishable from typed text; and it assumes an
  agent pane that draws an input box, not a bare shell.
- Never type over a human (`hold_while_typing`, on by default). A dunk that
  lands while the human is mid-sentence splices its text into their draft and
  Enter submits the mangled result; seen live on 2026-09-19 in a Claude pane
  whose transcript reads `...in priorit yoPM poll: check list_panes...`. The
  blind spot above ("hints are indistinguishable from typed text") is only
  true in plain text: read with `herdr pane read --format ansi`, both Claude
  Code and Codex draw their empty-box hints dim (SGR 2, including Claude's
  dim prompt suggestions and Codex's rotating `Use /skills ...`) and typed
  text at full intensity. `_typed_text` finds the last prompt-sigil line,
  keeps only its non-dim characters (a small SGR walker that skips the `2` in
  `38;2;r;g;b`), strips the sigil and box decoration (braille animation dots,
  rules) and returns the draft; '' for an empty or hint-only box, None when no
  box is drawn. Only the sigil line is judged: every draft starts there, and
  continuation lines are hard to tell from an agent's footer. `_is_our_send`
  excludes our own leftover text (the visible line is a prefix of the last
  send, or a collapsed-paste placeholder while `awaiting_consumption`), which
  stays backpressure's business. In the timer loop the gates form one loop:
  idle gate, then typing check; while typing, status `Held (typing)`,
  `hold_count` +1 once per hold, re-check every `TYPING_RECHECK_S` (60 s), and
  after the box clears the idle gate runs again because finishing typing
  usually means submitting. Unreadable pane or no box: never hold, since this
  guards the human's draft rather than the agent's queue. Since 2.3.0 `fire`
  and `send_now` respect it too, via the message queue below.
- Direct-message queue (2.3.0). `send_text` used to type immediately, so an
  agent messaging a pane could splice itself into a human's draft even though
  scheduled dunks would not. Messages now go through a per-pane outbox in the
  daemon: `Flock.send_now` builds a `Message`, `_enqueue` appends it to
  `outbox[pane_id]` and starts one delivery thread per pane, and
  `_outbox_worker` delivers in FIFO order, each only once `_clear_to_send`
  agrees. While someone types it re-checks every `MESSAGE_RECHECK_S` (5 s,
  against a dunk's 60 s, because a message is waiting to be delivered rather
  than scheduled for later). `wait_s` lets a caller block for delivery;
  `hold_while_typing=false` bypasses the queue. `fire_dunk` uses the same
  outbox when its dunk holds while typing, with the dunk id as the message's
  `source`, so a test send no longer bypasses the protection. `list_messages`,
  `get_message` and `cancel_message` expose the queue plus a bounded history
  (`MESSAGE_HISTORY`, 50). The queue is in memory only: a daemon restart drops
  what is waiting, which is the right trade for "deliver this now-ish".
- Typing settle window (2.3.0). `_clear_to_send` replaces a single typing
  check: the box must read clear, then still read clear `TYPING_SETTLE_S`
  (1 s) later. A pause between thoughts no longer looks like being finished.
  Dunks pass their `stop_event` so a stop during the settle wait returns at
  once. `_typing_in` is the pane-level form of the old `_human_typing`, which
  now delegates to it.
- herdr layout control (2.2.0). The same registry that schedules dunks can
  build the herd: `list_workspaces`, `create_workspace`, `create_tab`,
  `split_pane`, `start_agent`, `rename`, `focus`, `close`, and a generic
  `herdr` passthrough plus `herdr_help`. `HerdrClient` gained thin wrappers
  (`_run_result` parses herdr's JSON, including the JSON errors herdr prints
  on stderr, into `(ok, result-or-message)`); `Flock` resolves workspace and
  tab targets with the same id / exact label / unique substring / `self` rules
  as panes (`_pick`), runs an optional command in a freshly created pane via
  `herdr pane run` (verified to work immediately after creation), and turns
  herdr refusals into `DunkError`s. `start_agent` uses `herdr agent start` so
  herdr registers the process under the given name and tracks it. `close`
  refuses the caller's own pane / tab / workspace (a process closing its own
  terminal is almost always a mistake) and stops dunks whose target pane was
  inside. The passthrough shell-splits the command, maps `self` to the
  caller's pane id, returns parsed JSON or usage text (a bare group's usage is
  an answer, not an error) and refuses only what would stop, replace or attach
  herdr itself (`server stop`, `update`, `channel set`, `session`,
  `completion`, `agent attach`, `integration`, `--session/--remote`). Nothing
  here is persisted; herdr is the source of truth for layout.
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
wait=false)`, `i/e/n/o/u/m`→`update_dunk`). `q` closes the view; `Q` sends
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
