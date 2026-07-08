# Design: Dunking Sheep

## Goal

Reproduce the Dunking Bird TUI feature-for-feature, but deliver text through
herdr's socket API instead of the OS input layer. Run as a process inside a
herdr tab.

## Components

```
dunking_sheep_tui.py     curses UI + dunker timers (port of dunking_bird_tui.py)
herdr_client.py          thin wrapper around the `herdr` CLI (the transport)
run_dunking_sheep.sh      launcher (ensures ~/.local/bin on PATH)
```

### `herdr_client.HerdrClient`

The single seam between the app and herdr. Every method shells out to `herdr`
and parses JSON. It never raises to the UI; it returns data or `(ok, message)`.

- `is_available()` / `server_error_hint()` — health + a human hint.
- `list_panes()` — the pool of targets, from `herdr pane list`.
- `get_pane(pane_id)` — refresh a single target (detect if it disappeared).
- `send_text_and_enter(pane_id, text)` — the core action:
  `herdr pane send-text` then `herdr pane send-keys <pane> Enter`.
- `notify(title, body)` — optional herdr toast.

Shelling out to the CLI (rather than speaking the raw socket protocol) mirrors
how Dunking Bird shelled out to `ydotool`/`kdotool`, and it insulates us from
protocol-version churn: the CLI is the stable, documented contract.

### `DunkerTuiRow`

Unchanged in spirit from Dunking Bird. The window fields
(`captured_window_id/name/class/compositor`) are replaced by target fields
(`target_pane_id`, `target_agent`, `target_cwd`). `_do_send()` is now two CLI
calls instead of focus + ydotool. `start()` refuses to run without a target.

### `DunkingSheepTui`

Same key handling, same table layout, same modals. Differences:

- `c` opens **`pick_target`** → `target_modal`, a scrollable picker instead of a
  2-second active-window grab. This is strictly better in a herdr world: you see
  every pane and choose precisely. The picker is **grouped by workspace**: each
  workspace name is a header and its tabs are indented beneath it, shown as
  columns `Tab · Agent · Status · Directory`. The raw pane id is not displayed
  (it means nothing to a human); it is still what gets stored as the send
  target. Labels/ordering come from `herdr workspace list` + `herdr tab list`
  joined onto `herdr pane list` by id (`HerdrClient.list_panes_grouped`), since
  `pane list` alone only carries raw ids. Panes are sorted by (workspace number,
  tab number) so the grouping is stable.
- `runtime_checks()` checks the herdr server instead of ydotool.
- The `suppress_typed_input` machinery is removed — sends never reach the OS
  keyboard, so there is nothing to suppress.

## Why a picker instead of "capture active window"

Dunking Bird captured whatever window was active because it had no inventory of
windows. herdr *does* have an inventory (`pane list`), including which agent
runs where and its live status. A picker is more precise and removes the
fragile 2-second "switch to your window now" dance. The keybinding stays `c`.

## Preserved behavior details

- Interval parsing, `Bad interval!` on non-positive/non-numeric.
- Live `Next: MM:SS` countdown driven by a per-dunker daemon thread.
- Test send with a 2-second countdown.
- Full-screen text editor (`textpad.Textbox`, Ctrl+G save / Esc cancel).
- `send_lock` serializes sends across dunkers.
- Defensive `safe_addstr` / `clip` so a small terminal never crashes curses.

## Non-goals (kept out to preserve parity/scope)

- Gating sends on `agent_status` (e.g. "only send when idle"). herdr makes this
  trivial via `herdr agent wait`, and it is an obvious future enhancement, but
  Dunking Bird has no equivalent so it is left out of the initial port.
- Persisting dunker config to disk (Dunking Bird's TUI does not either).
