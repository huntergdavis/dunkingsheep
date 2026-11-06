# Guard shims (t3230, audit P0-8)

These are the typing guards that `dunkingsheep guard install` renders into the agent's PATH. They
route pane typing and destructive herdr and tmux commands through the daemon, so the human stays in
control. The Rust binary embeds the copies in this directory, so a build no longer reads the repo's
Python tree.

## What changed in the copies here

- The CLI is run directly: `[DUNKINGSHEEP, "--json", ...]`. It was `[sys.executable, DUNKINGSHEEP,
  ...]`, which runs a native binary as Python and fails.
- The rendered CLI path is the packaged launcher `~/.local/bin/dunkingsheep`, which the packaging
  points at the active release. It is never the repo checkout or a build tree.
- The fail-closed logic is unchanged. A refused or unreachable daemon refuses; only unguarded
  commands pass through to the real herdr. `DUNKINGSHEEP_GUARD` and `HERDR_GUARD_OFF` behave as
  before.

## Retained Python shim files (inventory)

| File | Role | Status |
| --- | --- | --- |
| `rust/crates/ds-cli/guard/dunk_guard_wrapper.py` | herdr guard shim, rendered with the launcher path | Temporary, packaged with the Rust release. `guard check` runs it with `python3`. A native rewrite is tracked separately. |
| `rust/crates/ds-cli/guard/dunk_tmux_guard.py` | tmux guard shim | Same as above. |
| `dunk_guard_wrapper.py` (repo root) | Python-era copy, used by the Python CLI | Unchanged. Retired with the Python tree (T63). |
| `dunk_tmux_guard.py` (repo root) | Python-era copy, used by the Python CLI | Unchanged. Retired with the Python tree (T63). |
| `dunk_guard.py` (repo root) | Python-era guard module; not embedded by the Rust binary | Unchanged. Retired with the Python tree (T63). |

The root copies still run the CLI through `sys.executable`, as the Python-era CLI expects. They are
not used by the Rust binary. The crate copies differ from them only in the CLI invocation and the
comment above it.

## Gated, not declared complete

Full Python retirement of the shims stays gated: the native shim rewrite, the `python3` dependency of
`guard check`, and deleting the root copies (T63). Deleting the root copies does not break the release
binary build, because the release build reads no repo-root file. Test builds do read the Python tree,
so these reads must move before the Python tree is deleted:

- `dunk_core.py`, `dunk_api.py`, and `dunk_roles.py`: `ds-core` `src/api/recovery_tests.rs`.
- `dunk_core.py`: `ds-core` `src/flock/recovery_commands_tests.rs`.
- `dunk_tasks.py` and `dunk_core.py`: `ds-core` `src/tasks/wire/tests.rs`.
- `dunk_poolrad.py`: `ds-core` `src/server_runtime/poolrad/tests.rs`.
- `dunk_watchdog_deadman.py`: `ds-core` `src/watchdog_deadman.rs`, `src/watchdog_deadman/check_tests.rs`,
  and `src/watchdog_deadman/main_tests.rs`. The last also imports `tests/test_dunk_watchdog.py`.
- `dunk_health.py`, `dunk_state.py`, `dunk_api.py`, and `tools/diff_harness.py`: `ds-core`
  `src/server_runtime/health_state.rs`.
- `dunk_http.py` and `dunk_terminal.py`: `ds-web` `src/python_http.rs`.
- The Python reference `dunkingsheep`: `ds-cli` `tests/command_routes.rs`.
- `tools/dunk_poll.py`: `ds-cli` `tests/dispatch.rs` and `tests/release_layout.rs`.

This list comes from a search of `rust/crates` at this head for `python3` runs, `include_str!` of
repo-root files, and root-relative paths. The crate-local copies under `guard/` are not root reads.

## Tests

`rust/crates/ds-cli/tests/guard_shims.rs` renders the shims with the shipped `guard install` and runs
them against a fake packaged launcher and a fake real herdr. It checks the allowed path, the refused
path, the unreachable-daemon path, passthrough of unguarded commands, and `guard check`. No real herdr
or tmux is started, and nothing is typed into a pane.
