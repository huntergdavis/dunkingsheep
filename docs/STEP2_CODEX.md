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

## Why it waits for a fresh session

The interface is marked experimental by its own authors, it requires a live busy codex
lane to test against, and a half-wired socket would replace a *known* hold with an
*unknown* signal - the exact trade this project has been refusing all night. The hold
(rule 15) is the safe default in the meantime, and it reports its own failures.
