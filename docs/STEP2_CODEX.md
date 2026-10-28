# Step 2, the codex half: why it is scoped and not started

Codex is the one harness whose mail depends on the hold being short: it accepts text
into its composer during a turn and **does not submit it** (measured: 20 failed nudges
on wR:p6, mail intact 14/14). So while the hold protects its mail, every held nudge is
a delay, and the owner feels delays.

## What is available

`codex --help` (2026-10-24) exposes `agents` and `app-server`:

- `codex agents` is a **TUI browser** ("Browse all agent sessions on the shared local
  app-server daemon") with no `--json`, so it is not a scripting surface.
- `codex app-server` is `[experimental]` and its generated v2 schema is what our own
  `docs/TERMINAL_ADAPTER.md` already records: `turn/start`, `turn/steer`,
  `turn/interrupt`, `thread/resume`, `thread/turns/list`, and the
  `turn/started` / `turn/completed` notifications we need. `expectedTurnId` makes a
  stale steer fail rather than interrupt another turn.

## What the work is

**Updated 2026-10-28 (t264): the app-server client below was the original plan and
is NOT the implementation.**  A client of the shared app-server daemon is detached
from the pane (`3639953 -> 3493002 -> 1` with the pane root absent), so
`require_turn_caller` refuses it, correctly.  A detached adapter asserting a
pane/session/cwd label would need a new ownership proof and is not wanted.

The implementation lives in `integrations/codex/`: Codex's `UserPromptSubmit` /
`Stop` hooks call a token-aware producer, and Codex is launched with
`codex --no-daemon`.  With that flag the app-server runs in the pane's own Codex
process, so the hook is a child of the pane root and the existing ancestry check
authenticates it.  Measured with an isolated `CODEX_HOME`: hook -> codex TUI ->
... -> pane root.  Existing lanes need a respawn with `--no-daemon`; a lane on the
shared daemon keeps reporting `unknown` and the daemon refuses the event rather
than guessing.  See `integrations/codex/README.md` for the contract and the
refusals (stale/replayed turn, replaced root, expired token, missing start).

OpenCode is the other half: its plugins run inside the OpenCode server process,
so `integrations/opencode/dunkingsheep-turn.js` starts on `chat.message` and ends
on `session.idle` for the exact root session.  A shared/detached server is
refused by the same ancestry check; disconnect/reconnect is covered by the TTL,
not by an idle guess.  See `integrations/opencode/README.md`.

### Original app-server plan (kept for the record; superseded)

A small JSON-RPC-over-stdio client: start `codex app-server`, list threads, subscribe
to `turn/started`/`turn/completed`, map a thread to a pane (cwd + session id from
`launches.json`), and feed `dunk_activity.activity()` a codex branch. Then the hold can
be released the instant a turn completes.

## Acceptance test (written now, not later)

1. A nudge to a **mid-turn** codex lane is held (it is today).
2. When that lane's `turn/completed` arrives, the held nudge is delivered within **N
   seconds** (N = 15) and the hold reason is gone from `dunkingsheep delivery`.
3. With the app-server unavailable, activity stays **unknown** and the hold remains -
   the existing behaviour, never a guess.
4. Proof by counter: zero `message.failed` events for codex lanes in the hour after
   the change, against twenty in the hour before.

## Thread-to-pane mapping: the two gaps must be BLOCKED, never guessed

Thread ids and panes are the name-vs-id mismatch that has cost us five incidents in one
day, so both halves are decided here, before any code:

| situation | answer | never |
|---|---|---|
| a **thread exists** with no pane matching its cwd/session | `unknown` + "codex thread <id> has no pane in this process's pane list" - the holder is not ours to report on | assume it belongs to the nearest codex pane |
| a **pane exists** with no thread (starting, resumed elsewhere, app-server restarted) | `unknown` + "no codex thread matches pane <id> (session <sid>)" - and the HOLD stays, because a hold is diagnosable | assume idle |
| one thread, two panes (a duplicate lane) | both `unknown`, naming both panes: the ambiguity is the finding | pick the newest |
| two threads, one pane | `unknown` naming both threads | merge them |

The mapping key is `(cwd, session id)` from `launches.json` - the same pair a restore
would use, so a wrong mapping shows up here first rather than after a reboot.

## Turn RPC authentication is a separate requirement

An exact thread-to-pane mapping does not authenticate a turn producer. The current
`turn_start`/`turn_end` RPC guard resolves a stable caller from the kernel socket
peer, checks its PID/start time, and walks its parents toward the backend's current
`pane_pid`. The pane root must occur in that ancestry. A detached central adapter
cannot satisfy that requirement merely by supplying a pane label, cwd or session
id. A producer using this RPC needs a caller within the pane ancestry (for example
an in-pane bridge), or a separately reviewed authentication design.

Refusals keep the existing client error and emit one daemon warning with
`reason=...`, pane id, stable caller PID/source and backend pane PID. The reason
distinguishes missing pane PID, missing/dead caller, start-time mismatch, and
missing/dead/cyclic/too-deep ancestry. `pane_not_in_ancestry` means the walk reached
PID 1 without the pane root; it does not guess why the process was reparented.
Turn tokens and message contents are not logged. These diagnostics do not relax
authentication or establish that a live activity producer has been implemented.

## Why it waits for a fresh session

The interface is marked experimental by its own authors, it requires a live busy codex
lane to test against, and a half-wired socket would replace a *known* hold with an
*unknown* signal - the exact trade this project has been refusing all night. The hold
(rule 15) is the safe default in the meantime, and it reports its own failures.

## t264 acceptance map (2026-10-28)

| # | requirement | evidence |
|---|---|---|
| a | real start/completed updates only the exact bound pane, dispatch once | isolated test panes: Codex `%73` and OpenCode `%76` each showed `working` then one accepted end; daemon log had exactly one `turn.end` for each; dispatch returned `skip: no_open_work` (the pane held no task) |
| b | detached or forged callers refuse | `dunkingsheep --self-pane %15 --json turn start --pane %15` from a pi pane refused `pane_not_in_ancestry`; the Codex producer refuses `no_codex_ancestor`; existing `test_turn_auth_diagnostics` covers every sub-condition |
| c | ambiguous sessions refuse | OpenCode plugin ignores child sessions and refuses a second non-child root while active (`tests/test_opencode_turn_generation.mjs`); Codex binds the exact hook `session_id` + `turn_id` |
| d | stale or replayed turns refuse | Codex token is matched to the hook `turn_id` and consumed before the end RPC; stale/replaced/expired/replayed cases are in `tests/test_codex_turn_generation.py`; the daemon rejects duplicate/foreign turns in `tests/test_dunk_turn_end_dispatch.py` |
| e | a replaced pane root refuses, never falls back to a label | Codex producer stores starter pid/start and refuses `replaced_root` without touching the new token (`test_replaced_root_refuses...`); the daemon is the final gate (`pane_not_in_ancestry`) |
| f | disconnect behaviour stated | Codex token is on local state and the TTL expires a crashed turn; OpenCode token is in memory, and `opencode run`/reconnect loses it, so the TTL expires the turn; neither path reports idle on expiry (`turn_state` in `dunk_activity.py`) |

**Deployment limits (not claimed):** the Codex producer is in-pane only with
`codex --no-daemon`; existing shared-daemon lanes need a respawn.  `--no-daemon`
also gives each lane its own app-server, so cross-lane `codex agents` browsing of
those sessions is not shared; the per-lane memory cost was not measured.  The
installers were exercised on temporary Codex/OpenCode homes, not the live owner
homes.  No live lane was restarted, and no live task-bearing lane was used to
test dispatch delivery.  The Codex install trust approval for a *fresh*
hooks.json is an operator step.
