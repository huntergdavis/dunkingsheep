# Codex turn producer (in-pane)

This is the Codex half of t264.  It uses Codex's `UserPromptSubmit` and `Stop`
hooks, which are the only turn boundary Codex exposes, and it must run with:

```sh
codex --no-daemon ...
```

## Why `--no-daemon` is load-bearing

The daemon's turn RPC authenticates the caller by kernel PID / start-time
ancestry under the backend's pane process (`Flock.require_turn_caller`).
Measured on this host (2026-10-28, isolated `CODEX_HOME`, no live lane touched):

| launch | hook's process ancestry | result |
|---|---|---|
| default TUI (shared app-server daemon) | hook -> shared app-server -> `pid 1`; pane root absent | daemon refuses `pane_not_in_ancestry` (correct) |
| `codex --no-daemon` TUI | hook -> codex TUI -> ... -> pane root | daemon authenticates |
| `codex exec` | hook -> codex process -> ... -> pane root | daemon authenticates |

With `--no-daemon` the app-server runs in the pane's own Codex process, so the
hook is a child of the pane root.  A detached adapter that asserts a pane label,
cwd or session id is **not** a substitute and is not provided here.

## Deployment trade-off (owner decision)

`--no-daemon` gives each Codex lane its own app-server inside the pane instead
of the shared background daemon.  That is what makes the ancestry check pass,
but it also means `codex agents` in another lane cannot browse that lane's
sessions through the shared daemon.  The per-lane memory cost was **not**
measured here.  This is a fleet policy choice, not something this producer
should decide silently; existing lanes stay on the shared daemon (and `unknown`)
until the owner/VP chooses to respawn them.

## Install

```sh
python3 integrations/codex/install.py --check
python3 integrations/codex/install.py
```

The installer copies `dunkingsheep-turn.py` and `dunkingsheep-turn.sh` into the
Codex home.  If `hooks.json` already points at `dunkingsheep-turn.sh` it is left
byte-for-byte unchanged, preserving Codex's persisted hook trust.  A fresh
`hooks.json` needs one approval in the Codex TUI.  The installer never edits a
launch command or restarts anything.

For a fresh installation the two hook entries are:

- `UserPromptSubmit` -> `dunkingsheep-turn.sh start`
- `Stop` -> `dunkingsheep-turn.sh end`

## Behaviour and refusals

- The producer never passes `--pane`; the CLI resolves the caller's terminal
  identity and the daemon re-checks ancestry.
- `turn start` is called with `--json`; only a successful response containing a
  `turn_id` is stored.  A failed or tokenless start never produces an end.
- The token is matched to Codex's own hook `turn_id`, so a stale, replayed or
  delayed `Stop` cannot end the next turn.
- The token is bound to the Codex pid/start that started it.  If the pane root
  is replaced, `end` refuses `replaced_root` and leaves the old token untouched;
  it does not fall back to a label or to the new process.
- A hook failure is logged and swallowed (`exit 0`).  Codex treats a nonzero
  `UserPromptSubmit` exit as "block this prompt", which would stop the lane.
- A crashed Codex process loses the token; the daemon TTL (default 1800 s)
  expires the turn.  Expiry is not reported as idle.

State and diagnostics live under `~/.local/state/dunkingsheep/` (override with
`DUNKINGSHEEP_CODEX_STATE_DIR`).  Turn tokens and prompt text are not logged.

## Isolated live acceptance (2026-10-28)

A test pane launched with an isolated `CODEX_HOME`, `codex exec`, and
`--no-daemon` reported `agent_status: null -> working` while the turn ran, then
one accepted `turn end` (`ended=true`, `dispatch=skip reason=no_open_work`).
The daemon log had exactly one `turn.end` for that pane.  A forged call from a
pi pane, `dunkingsheep --self-pane %15 --json turn start --pane %15`, was refused
`pane_not_in_ancestry`; the pane's real turn was untouched.  The test panes were
created for the measurement and removed; no live lane was restarted.

## Verify

```sh
python3 -m unittest tests.test_codex_turn_generation -v
```

The tests exercise the producer with a fake CLI and a fake process tree:
delayed start / fast end, consumed token, failed start, stale `turn_id`,
replaced root, expired token, and no `--pane` argument.
