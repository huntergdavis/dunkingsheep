# Project Cargo locks

Use `ds-cargo` for Dunking Sheep. It selects Rust 1.99.1, preserves existing
registry/target settings, defaults to two jobs and warnings as errors, and delegates
locking to `proj-cargo dunkingsheep`. Other projects use `proj-cargo lookingglass
build ...` (or their own project slug) with their normal Cargo toolchain on PATH.
Both scripts live together in `~/.local/bin` after installation.

Never flock another project's lock. DS keeps
`$XDG_RUNTIME_DIR/dunkingsheep/compile.lock`; LookingGlass uses
`$XDG_RUNTIME_DIR/lookingglass/compile.lock`. Without XDG_RUNTIME_DIR, the runtime
root is `/run/user/$(id -u)`. Lock paths are a same-user convention, not a security
boundary. Direct Cargo/direct flock outside the wrappers cannot be host-bounded.
All participating projects must adopt the shared slots; migrate only while idle.

Each compile first locks its project, then one of
`$XDG_RUNTIME_DIR/compile-slots/slot-{1..N}`. Project waiters therefore cannot consume
extra host slots. `N` defaults to **2**; the owner may put one integer from 1 to 16
in `${XDG_CONFIG_HOME:-$HOME/.config}/compile-slots/limit`. Invalid configuration
fails closed. Change that file only in a coordinated build-free window, especially
when reducing the limit: running wrappers retain their original slot assignment.
Do not rename, delete or replace lock files while participants run.

The lock budget is at most 30 seconds across maintenance, project and host-slot
waits. `PROJ_CARGO_WAIT_SECONDS` may shorten the wait
to 1..30 seconds. Exit 75 means local NOT RUN; record it and use exact-head CI,
without retry loops. Waiting/busy diagnostics show holder PID, cwd and project.
Legacy direct-flock holders are marked `project=unmanaged`; their cwd identifies
the foreign project. Holder files are diagnostic only and checked against a live
descriptor; they confer no authority. Only PID/cwd/project are logged, no arguments
or environment. Process visibility can limit identification.

Commands default to a 3600-second timeout. DS accepts its existing
`DS_CARGO_TIMEOUT_SECONDS`; generic callers use `PROJ_CARGO_TIMEOUT_SECONDS`,
1..3600. Release builds always receive 3600 seconds. Exit 124 means timed out.
`fmt`, version and metadata reads retain the existing exemption from compile locks.
Each successful compile launch logs its assigned host slot. A shared maintenance
lock stays held through the compiler so a cooperating install cannot race builds.

## Reviewed migration

Keep running builds untouched. Obtain independent review of the exact tooling
commit and a coordinated idle handoff from all projects; pause new build launches.
The initial old wrapper has no maintenance gate, so its users must explicitly
honor that handoff. Do not install while any Cargo/rustc/rustdoc process or project
compile/host-slot lock is active. Keep the handoff closed until both replacements
are complete. A wrapper update is not permission to alter a project's toolchain.

```
bash tests/test_project_cargo.sh
bash tools/install-cargo-wrappers.sh                 # dry-run, no filesystem writes
sha256sum ~/.local/bin/ds-cargo                     # review this baseline
# After review and idle handoff, use a new task-specific workspace backup directory:
bash tools/install-cargo-wrappers.sh --apply --expected-ds-sha BASELINE_SHA \
  --backup-dir /absolute/workspace/task/wrapper-backup
```

The installer backs up existing scripts/modes before atomically replacing each
file, installing the generic wrapper first. It refuses active builds and baseline
drift, takes the exclusive maintenance gate, and never signals a build. Config is
not overwritten. Record old/new hashes and the backup path on the task. If a
partial install fails, retain the idle window and restore both backed-up wrappers
by atomic rename before reopening builds. If `proj-cargo` was previously absent,
its unused new file may remain; `ds-cargo` must match the recorded baseline.
Rollback an installed wrapper only under the same reviewed, idle handoff.
`--bin-dir ABSOLUTE_DIRECTORY` selects an existing alternative installation prefix
for private tests; it defaults to `~/.local/bin`.

The integration test uses real flock/timeout with fake Cargo in a private workspace
runtime/config. It checks concurrency, same-project exclusion, diagnostics,
timeouts/exit status, unlocked reads and configured limits, without running Rust.
