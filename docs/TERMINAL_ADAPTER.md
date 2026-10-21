# Terminal adapter: herdr and tmux

Status: 2.26.3. The tmux backend is implemented and live-tested behind the
seam. **DECISION (t35, 2026-10-21): herdr remains the default and nothing is
removed.** The live tmux trial failed gate 2 (idle hold): on tmux the input box
is empty while an agent is mid-turn, the backend honestly reports
`agent_status=unknown`, and a send was typed into a working claude pane and
submitted (`status=sent, queued=false, holds=0`). That is the exact corruption
the hold exists to prevent, so tmux is not a supported default. Identity,
screen, typing, the three-way close rail, and recovery passed or passed with
notes; details below.

## Why a seam

Everything above the terminal (Flock, tasks, inbox, memory, the CLI, MCP) asks
for *placements*, *panes*, *screen text* and *sends*. It must not know how a
particular terminal tool spells those. Three costs made this concrete:

1. `herdr agent start` changed grammar in 0.9.5 (`NAME --kind KIND --pane ID`,
   in an existing shell pane). The client still sent `--workspace/--tab/--split`
   and **every agent start failed**. The grammar has to live in one place.
2. `herdr pane current` costs 1-2 s, even when `HERDR_PANE_ID` already says
   which pane we are in. Identity must come from the environment the terminal
   exports, not from a server round trip.
3. The sidebar started the full CLI three times per poll. A small client plus
   one daemon batch is enough.

## The seam

`dunk_terminal.py`:

```python
class TerminalBackend:
    name = "base"
    def capabilities(self): ...
    # discovery, layout, send, read, close, agent start, annotations
class HerdrBackend(TerminalBackend):   # default; wraps HerdrClient unchanged
class TmuxBackend(TerminalBackend):    # sessions=workspaces, windows=tabs
def get_backend(herdr=None): ...       # existing client/fake -> HerdrBackend
```

Selection:

```
DUNKINGSHEEP_TERMINAL_BACKEND=herdr   # default
DUNKINGSHEEP_TERMINAL_BACKEND=tmux
```

An unknown value is a named refusal, never a silent fallback. The Flock wraps
whatever client it is given (`Flock(herdr=FakeHerdr())` still works), so the
existing tests and the herdr path are unchanged.

### Capabilities are honest

| capability | herdr | tmux |
|---|---|---|
| screen / scrollback | yes | `capture-pane -p [-S -N]` |
| literal send + Enter | yes | `send-keys -l` + settle + Enter |
| pane identity | `w8:p8` | `%N` (`TMUX_PANE`) |
| pane pid (liveness hook) | not exposed | `#{pane_pid}` |
| agent status / session | exact | **unknown / none** (no API) |
| close guard | wrapper + daemon | backend registry + wrapper + daemon |

### Status is our data, not the terminal's

tmux has no agent-status/session API. That is a design position, not a gap to
be guessed around: working/idle/blocked and session identity are **our** data,
maintained by the daemon (the store, the lease, the recorded launch), not the
terminal tool's opinion. The backend reports only what it can observe (the
foreground command and pane title) and says `unknown` otherwise. Policy never
treats `unknown` as `idle`; the backend is never asked to invent a status it
does not have.

### Grammar that now lives in the backend

`HerdrClient.start_agent` creates/locates a shell pane first and then calls:

```
herdr agent start <NAME> --kind <KIND> --pane <ID> [--timeout MS] [-- ARG...]
```

It never sends `--workspace/--tab/--split/--cwd` to `agent start`. On failure
it closes the shell it created, so a failed start does not leave an orphan.
`tests/test_herdr_client.py` pins both halves: the installed help text must
contain `--kind`/`--pane` and not the old flags, and the argv the client builds
must match.

The tmux backend starts the agent by creating a window (`new-window`) or split
(`split-window`) whose command is the agent; it labels the pane with the agent
name so inbox targets resolve.

## tmux close protection

Two layers, because either alone is weak:

1. **Backend registry** (`~/.config/dunkingsheep/tmux-panes.json`, 0600):
   `create_workspace`, `create_tab`, `split_pane` and `start_agent` record the
   panes/windows/sessions they create. `close()` refuses anything not recorded,
   and refuses to close a window/session that still contains an unmanaged pane
   (someone may have split it by hand). This is "never target a pane we did not
   create" enforced below the policy.
2. **Guard wrapper** (`dunk_tmux_guard.py`):
   * read-only commands (`list-*`, `capture-pane`, `display-message`,
     `has-session`, `show-*`) exec the real tmux unchanged;
   * `kill-pane`/`kill-window`/`kill-session` are mapped to
     `dunkingsheep close <kind> <target>`, so the daemon's superuser/self
     policy and the registry check run; a refused or unreachable daemon is a
     refusal (no fallback to real tmux);
   * `kill-server`, `kill-all` (`-a`), raw typing (`send-keys`, `paste-buffer`,
     `run-shell`, ...), `respawn-*` and layout mutation are refused with a
     pointer at the daemon commands.

Install it with:

```
dunkingsheep guard install-tmux
```

This does **not** touch `~/.bashrc`. `TmuxBackend.start_agent` puts the guard
directory first on the PATH of the agent panes it starts (both `-e PATH=` and a
`PATH=... exec` prefix, because a login shell can reset PATH before running the
command). The user's own shell keeps the real tmux. The guard's real binary is
beside it as `tmux-real`.

The guard's decision is a **pure function**,
`close_decision(kind, target, *, panes, protected, managed, caller_pane)`. It
answers "would this close be allowed?" without touching tmux or the daemon, so
it can be evaluated with the VP's real pane id. `main()` calls it before
`route_close()`; the daemon then applies its own superuser/self/registry checks
as defence in depth.

### The VP pane: policy evaluation + same-level decoy, never a live kill

The VP ruled against a live kill of their pane: the failure mode of that test
is the 2026-10-18 incident itself, and a test whose failure costs the thing it
protects is a gamble, not a test. Protection is proven two other ways:

1. **Decision-level test with the real VP id.** The test loads the live
   `superusers.json`, whose `panes` contains `w9:pH`, and asserts
   `close_decision` refuses the pane, its workspace `w9`, and its tab
   `w9:t9` — no tmux command is issued.
2. **Live decoy at the same protection level.** A throwaway pane is put in a
   temporary superuser state and registry; the real guard is invoked with the
   private tmux socket; the kill is refused and the decoy survives. The
   positive control kills a managed, non-protected decoy for real, proving the
   close path works and the refusal above is a policy decision, not a broken
   close.

Live tests (`tests/test_dunk_tmux_guard.py`) also start a real tmux server on a
private socket, start an agent whose PATH has the guard, and have that agent run
`tmux kill-server`, `tmux kill-pane -t $TMUX_PANE`, and `tmux send-keys`. All
three are refused; the server and pane survive.

## Identity and liveness

The owner's decision: the pane id is the registry key and the pid plus start
time is the liveness check.

* **Pane key**: `TMUX_PANE` (tmux exports it to the pane) or `HERDR_PANE_ID`.
  The CLI trusts it and does **not** call `pane current`; `pane current` is
  opt-in (`DUNKINGSHEEP_VERIFY_SELF=1`) for reconciling a stale post-restore
  env. This removes the 1-2 s cost from every CLI call.
* **PID + start (implemented, t32)**: `dunk_callers.py` walks from the socket
  peer (the short-lived CLI) to its nearest **agent ancestor** — claude, codex,
  pi, opencode, ... — and falls back to the session leader for a human shell.
  The daemon stores `caller_pid`, `caller_start` and `caller_name` on the task
  lease. `task_reap` and the daemon reaper check `/proc/<pid>` and its start
  time, so a crashed agent's lease is requeued **even when its shell pane still
  exists**. A vanished pane is **not** evidence on its own: only the lease
  deadline or a dead `/proc` caller requeues, so a live agent behind a lagging
  pane reading keeps its claim, and an unattributable/legacy lease is not
  guessed dead. The pane list carries a **source tag** (`live` / `fixture`);
  the store refuses an untagged or fixture list, so a test double can never
  move real work. The pane id remains the registry key; the name is only a
  label, so two panes cannot claim one task through a stale name / reused pid.

## Sidebar poll

`tools/dunk_poll.py` imports only the standard library and makes one socket
request (`cmd: "poll"`). The daemon's `h_poll` returns:

```
status (dunk counts, no terminal probe by default)
inbox_count
dunks + unread messages (read-only; does not mark read)
[panes]   # opt-in, startup only; listing panes is the slow part
[tasks]   # opt-in
```

`dunkingsheep install` writes `~/.local/bin/dunk-poll`; the pi extension
(`~/.pi/agent/extensions/dunkingsheep.ts`) calls it and keeps the full CLI only
as a fallback before it is installed. `dunkingsheep poll` is the human view.

The remaining ~2.4 s is **not** the inbox query: `messages_by_mailbox
(mailbox, state, created_at)` is a covering index, and the count plus attachment
count are 0.1-0.3 ms. It is `resolve_mailbox` asking herdr for the pane list to
map the self pane to its mailbox label. The next step is caching that mapping
for the poll (t34), not adding an index.

## Provider usage

`dunkingsheep --json usage` is the machine interface. The VP ops dashboard
(`ops_metrics.py`) stores age-labelled `provider_usage` snapshots every external
interval; a disabled/failed read stays missing, never zero. `VP_PROVIDER_USAGE=0`
disables the external read (tests do).

## Migration order (VP #1118)

1. Usage integration with the ops dashboard — done (`provider_usage` +
   `--json usage`); the watchdog itself is not a usage rule because usage is not
   a health failure.
2. tmux adapter behind the seam, herdr kept — done and live-tested.
3. Close protection with the VP's own pane as the subject — done by the VP's
   ruling without a live kill: the real `w9:pH` decision is REFUSED, a
   same-level live decoy is refused and survives, and a positive-control kill
   proves the live path. **The live tmux trial (t35) then failed gate 2 (idle
   hold), so herdr remains the default and nothing is removed.** Live tmux lanes
   and the backend switch are closed as a decision, not deferred.
4. Stdlib-only poll client — done.

## Daemon environment

The daemon must be started with the **full backend environment**. A client
auto-start may carry only `DUNKINGSHEEP_DIR` and `DUNKINGSHEEP_SOCKET`; it then
comes up on the default backend and silently sees the wrong pane set. This is
exactly what produced the first gate-5 PARTIAL in the t35 trial: the client
auto-started the trial daemon without `DUNKINGSHEEP_TERMINAL_BACKEND=tmux` /
`TMUX_SOCKET`, so it listed herdr panes and saw no tmux panes. Start
`dunkingsheep serve` explicitly with the backend environment, or make the client
refuse to auto-start when the backend environment is partial.

## Turn wiring price (claude, codex, pi)

The explicit route is the only one that survives the hold's requirements.

**Claude: YES — worth doing.** Claude Code exposes `UserPromptSubmit` (turn
start), `Stop` / `SubagentStop` (turn end), plus tool hooks; the current settings
only wire `SessionStart`. A `TurnStart`/`TurnEnd` hook can call a small
`dunkingsheep turn ...` command with `DUNKINGSHEEP_TMUX_PANE`/`HERDR_PANE_ID`.
Price: **~2-4 hours** (hook script, a turn-state field on the pane record, the
two settings entries, and tests with a real claude turn). Failure mode: a
missed `Stop` on crash/force-quit leaves the lane marked working forever and the
hold would hold mail until a lease/max-age or the caller-pid liveness clears it
— so the turn state must carry a TTL and be reconciled with the pane pid.

**Codex: NO today.** Codex 0.162.1's hook surface, as installed, exposes only
`SessionStart` (`~/.codex/hooks.json`); there is no documented turn-boundary
hook. It cannot be wired the same way without a Codex release that exposes one.
Price if it appears: comparable to Claude (~2-4 hours), same missed-Stop TTL
problem. Until then herdr is required for codex.

**Pi: YES today, cheapest.** The pi extension already receives
`agent_start`/`agent_end`; emitting turn state is **~1-2 hours** and needs no
external hook trust. Same TTL/reconciliation rule for a crashed turn.

**Cheapest honest path:** pi first (smallest, fully controlled), then Claude if
the hold on tmux is worth 2-4 hours; codex stays on herdr until its hook surface
gains a turn boundary. Do not build screen heuristics.

## Environment

| variable | meaning |
|---|---|
| `DUNKINGSHEEP_TERMINAL_BACKEND` | `herdr` (default) / `tmux` |
| `DUNKINGSHEEP_TMUX_SOCKET` | tmux `-L` socket (default: user's) |
| `DUNKINGSHEEP_TMUX_REGISTRY` | close-guard registry path |
| `DUNKINGSHEEP_TMUX_GUARD_DIR` | guard dir put on agent PATHs |
| `DUNKINGSHEEP_TMUX_BIN` | real tmux binary override |
| `DUNKINGSHEEP_VERIFY_SELF=1` | opt-in `pane current` reconciliation |
| `DUNKINGSHEEP_POLL_BIN` | sidebar poll client path |
| `VP_PROVIDER_USAGE=0` | disable the ops dashboard usage read |

## Busy/idle signal: why tmux cannot hold today

The tmux trial's gate-2 failure is a **missing signal**, not a backend bug:
herdr supplies an exact agent status; tmux does not. The three candidate routes,
as they stand for claude, codex and pi:

1. **Per-harness screen signature** (spinner/working line). Works today for all
   three in the narrow sense that their output is visible, but it is fragile:
   claude cycles spinners ("Hyperspacing…", "Unfurling…"), codex uses an
   approval/dialog line, pi has its own. Cost: hours per harness, breaks on any
   output change. Not acceptable for the hold.
2. **Process-side signal**. The tmux pane pid is the **agent process itself**
   (`claude`/`codex`/`pi`), not a shell — but the process shape does not encode
   a turn: claude holds a long-lived `dunkingsheep mcp` child even when idle, so
   children are false positives; `pane_current_command` is the agent name
   always. A turn can spawn a tool child, but the absence of children is not
   idle. Deterministic only if a harness exposes a lock/child shape; today it
   does not for all three.
3. **Explicit declaration** over the existing socket. **pi can do this today**
   via its extension events (`agent_start`/`agent_end`); claude currently has
   only a `SessionStart` hook configured, and codex has hooks enabled but no
   turn-start/end hook wired to dunkingsheep. This is the most reliable route,
   but it works only for harnesses we can wrap. Cost: pi is cheap (an existing
   extension event); claude/codex need hooks installed and tested.

Cheapest honest path: for pi, emit turn-start/turn-end from the extension and
carry it on the pane record; for claude/codex, either install explicit hooks or
keep herdr. Do not build screen heuristics into the hold. If only herdr can
supply the signal for a harness, that harness stays on herdr.

## Not done / next

* Switch a live DS daemon to tmux for real lanes; only then consider demoting
  herdr (the VP-pane refusal is already proven by the decision + decoy tests).
* Add live tmux lanes (the herdr lanes remain the production fleet).
* tmux has no agent-session API, so post-upgrade recovery there relies on the
  recorded argv/launch memory, not `codex resume`/`claude --resume` detection.
