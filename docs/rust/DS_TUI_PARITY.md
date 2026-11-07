# ds-tui parity list (T61)

Oracle: `dunking_sheep_tui.py` and `text_editor.py` at the repo root. Each Python
symbol is mapped to its Rust counterpart under `rust/crates/ds-tui/src/`.

Status key: **ported** = behaviour matches and is pinned by a test; **drift** = ported
but a known render or key difference is open (the PR that fixes it is named);
**not ported** = no Rust counterpart is wired into the terminal entry.

## Screens and key handling

| Python | Rust | Status |
|---|---|---|
| `DunkingSheepTui.draw` | `view.rs` `draw_dunks` | ported; row width and `...` clipping fixed in PR 1 (`main_screen_oracle_tests.rs`) |
| `DunkingSheepTui.draw_tasks` | `view.rs` `draw_tasks` | ported; row width and clipping fixed in PR 1 |
| `format_row`, `format_task_row` | `rows.rs` `format_row`, `format_task_row` | ported, tested against Python arithmetic |
| `_format_target_row` | `rows.rs` `format_target_row` | ported, tested |
| `clip` | `rows.rs` `clip` | ported, tested |
| `safe_addstr`, `safe_hline` | `view.rs` `draw_line` | ported; PR 1 adds Python's `clip()` with `...` |
| `handle_key` (dunk view, every binding) | `model.rs` `handle_key`, `app.rs` `control_key` | ported; `v` flash fixed in PR 1 |
| `handle_task_key` (task view, every binding) | `model.rs` `handle_task_key` | ported |
| `run` (200 ms poll, refresh each loop) | `app.rs` `run`, `REFRESH_INTERVAL` | ported |
| `refresh`, `refresh_tasks` | `model.rs` `refresh`, `refresh_tasks` | ported, including the `[in-process, not shared]` label |
| `rpc` (flash on refusal, reconnect after 2 s) | `model.rs` `rpc`, `RECONNECT_INTERVAL` | ported |
| `flash`, `current`, `current_id`, `current_task` | `model.rs` | ported |
| `quit_and_shutdown`, `shutdown` | `model.rs` `quit_and_shutdown`, `shutdown` | ported |
| `edit_interval`, `edit_text`, `edit_name`, `edit_max_sends` | `model.rs` | ported |
| `toggle_only_idle`, `toggle_skip_if_unconsumed`, `toggle_hold_while_typing` | `model.rs` | ported |
| `pick_target` | `model.rs` `pick_target` | ported |
| `main` | `ds-cli` `dispatch.rs` `tui`, `runtime.rs` `run_terminal` | ported on the daemon path; see not-ported item 1 |

## Modals

| Python | Rust | Status |
|---|---|---|
| `line_modal` | `model.rs` `LineModal`, `runtime.rs` `TerminalModals::line_modal` | key map ported; label bold and `...` clipping in PR 3 |
| `target_modal` | `model.rs` `TargetPicker` | row content ported; layout columns and reverse-video highlight in PR 2 |
| `text_modal` | `model.rs` `TextModal` | key map ported; viewport height (`h - 5` rows) and bold title in PR 3 |
| `text_editor.py` `TextEditorBuffer` (all methods) | `text_editor.rs` | ported, tested |

## Not ported

1. **In-process fallback when the daemon is unreachable.** Python's `connect()` falls
   back to `LocalFlock` and flashes `Daemon unavailable (...); running in-process`.
   The Rust model supports this (`TuiModel::local_fallback`), but `dunkingsheep tui`
   returns the daemon connect error and exits. Needs an in-process `RpcClient` over
   ds-core; not in this series.
2. **Python's crash on an unknown priority.** `handle_task_key` `p` raises
   `ValueError` in Python when the current priority is not in the cycle. Rust ignores
   the key. This is a deliberate divergence, not a missing feature.

## Not verified

- No live curses comparison: the lane has no TTY. Python output comes from running the
  real `DunkingSheepTui` methods under a fake `curses` module (the harness is not in
  the repo). Colour, bold and reverse are checked as attributes, not on a real
  terminal.
- Keys arriving as raw terminal bytes (crossterm vs curses key codes) are checked at
  the `KeyEvent` level only.
