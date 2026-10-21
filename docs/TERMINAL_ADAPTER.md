# Terminal adapter: herdr and tmux

Status: 2.25.0. The tmux backend is implemented and live-tested behind the seam.
**herdr remains the default.** Nothing about herdr is removed until the tmux
close guard (below) has been demonstrated on real lanes with the VP's own pane
as the test subject — that is the 2026-10-18 lesson and it is not negotiable.

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

The tmux backend does not pretend to know working/idle: `agent_status` is
`"unknown"` and `agent_session` is `None`. Policies that need exact status
degrade rather than guess.

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

Live tests (`tests/test_dunk_tmux_guard.py`) start a real tmux server on a
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
* **PID**: the tmux backend exposes `pane_pid` (`#{pane_pid}`), so a caller's
  liveness can be checked against `/proc/<pid>` plus its start time without
  asking herdr or tmux.
* **Status**: task leases and the daemon's reaper currently key on the pane id
  and the backend's pane list. The next slice is to carry `caller_pid` +
  `caller_start` on the lease and let `task_reap` treat a dead `/proc` entry as
  a dead caller even if the shell pane still exists; a registered name is only
  a label, never the key, so two panes cannot claim one task through a stale
  name. `TmuxBackend.pane_pid` is the hook.

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
3. Close protection demonstrated on real lanes with the VP's pane as the test
   subject — the guard and backend are implemented and live-tested against an
   isolated server; the *VP-pane* live demonstration is still to do. Herdr is
   **not** demoted before it.
4. Stdlib-only poll client — done.

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

## Not done / next

* Carry `caller_pid`/`caller_start` on task leases and use `/proc` liveness;
  name is a label only.
* Switch a live DS daemon to tmux and demonstrate the VP-pane refusal on real
  lanes; only then consider demoting herdr.
* Add live tmux lanes (the herdr lanes remain the production fleet).
* tmux has no agent-session API, so post-upgrade recovery there relies on the
  recorded argv/launch memory, not `codex resume`/`claude --resume` detection.
