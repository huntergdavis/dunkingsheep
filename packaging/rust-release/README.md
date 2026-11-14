# Rust release packaging (t3229, audit P0-1)

Preparatory packaging for the Rust `dunkingsheep` CLI and daemon. It does not
activate anything. It makes an immutable release layout, launchers and a systemd
drop-in that point at that layout, and rollback and uninstall for both. Activation
is a separate, owner-approved cutover step.

## Unit artifact staging (t4699, slice 1)

`tools/rust_rollout.sh` now accepts an explicit `--unit daemon|web|telegram`
for `plan` and `stage`. It takes an already-built artifact, resolves the named
commit through the selected Git checkout, and checks an expected SHA256 before
copying and again before publication. It never executes the artifact or builds it.

```sh
tools/rust_rollout.sh stage --unit web --sha COMMIT \
    --artifact /absolute/path/to/ds-desk --artifact-sha256 LOWERCASE_SHA256
# Operator only, after review of the exact tooling head and artifact:
tools/rust_rollout.sh stage --unit web --sha COMMIT \
    --artifact /absolute/path/to/ds-desk --artifact-sha256 LOWERCASE_SHA256 --apply
```

| Unit | Installed artifact |
| --- | --- |
| daemon | `dunkingsheep` |
| web | `ds-desk` |
| telegram | `dunkingsheep-telegram` |

The default root is `~/.local/share/dunkingsheep/releases`; `RUST_RELEASES`
selects an absolute prefix, and `RUST_REPO` selects the checkout containing the
commit. Each release is `rust-<full-commit>-<unit>/`, containing the executable
(0555) and a `MANIFEST` (0444); the directory is 0555. The manifest records format
version 1, unit, full revision, binary name and binary SHA256. Commit/artifact
provenance is supplied by the operator's build receipt and independent review;
this command verifies the supplied bytes and Git revision, not their build origin.

Publication uses a private staging directory and one rename, serialized by a
per-unit lock. An interrupted/failed copy is not published. Reusing the same unit
and commit checks the manifest, installed bytes and modes; different content is
refused instead of replacing an immutable release. There is no artifact execution,
launcher, drop-in, service, timer, API or rollback-state change during this stage.

Dry-run is the default and creates nothing, including no release root or lock.
`plan --unit ...` stays read-only even with `--apply`. A dry-run validates its
inputs and any existing release before printing its destination and manifest.
Only an operator may use `--apply`, after exact-head review and owner approval;
the author must not apply their own tooling to production. The automated tests
apply only inside temporary prefixes with fake tools and fake HOME.

This first slice deliberately refuses `--unit` with `activate`, `verify` or
`rollback`, so a web/Telegram request cannot enter the legacy daemon cutover.
Without `--unit`, the existing daemon cutover interface is unchanged. Single-drop-in
migration, state-based switch/rollback, fixed per-unit postchecks, release listing,
retention, and real operator releases of all three units remain follow-up work on
t4699; this staging slice alone does not satisfy the full release acceptance gate.

## Layout

```
<prefix>/releases/<id>/dunkingsheep   Rust binary (0555)
<prefix>/releases/<id>/dunk-poll      native Rust poll client (0555)
<prefix>/releases/<id>/MANIFEST       release_id, revision, sha256 of both files (0444)
<bin-dir>/dunkingsheep                launcher: exec the release binary
<bin-dir>/dunk-poll                   poll wrapper: exec the release's native client
<unit-dir>/dunkingsheep.service.d/99-rust-release.conf
                                      drop-in: ExecStart reset to the release binary, serve;
                                      DUNKINGSHEEP_SUPERVISED=1 kept
```

Launchers and the drop-in never point at a Cargo build tree or at the dev
checkout's Python. The Rust `dunkingsheep install` verb refuses to run from anything
but an installed release (`release_layout::release_dir_of`).

## Commands

Every path is required. Nothing defaults to `$HOME`. None of the scripts runs `systemctl`.

```sh
packaging/rust-release/install-release.sh --binary FILE --poll NATIVE_POLL_BINARY \
    --release-id ID --prefix DIR --bin-dir DIR --unit-dir DIR [--revision TEXT] [--dry-run]
packaging/rust-release/rollback-release.sh --prefix DIR --bin-dir DIR --unit-dir DIR \
    [--to ID] [--dry-run]
packaging/rust-release/uninstall-release.sh --prefix DIR --bin-dir DIR --unit-dir DIR \
    [--purge] [--dry-run]
```

- **install** creates `releases/<id>/` atomically. Re-running with identical bytes is
  a no-op. Different bytes under an existing id are refused, because releases are
  immutable. Before changing anything, the drop-in and both launchers are saved as one
  pair under `<prefix>/rollback/`: `original` (the pre-Rust state, saved once) and
  `previous` (the state before this install, saved per release).
- **rollback** restores one saved pair, so the drop-in and the launchers always agree.
  With no option, it restores `previous`. With `--original`, it restores the pre-Rust
  state. With `--to ID`, it repoints both files at an installed release. Each rollback
  saves the pair it replaces as `previous`, so running it again undoes it. A file that
  was absent when the pair was saved is removed on restore.
- **uninstall** restores the pre-Rust pair from `original`. It keeps release
  directories and the rollback history unless `--purge` is given.

## Safety rules the scripts enforce

- `release id` is letters, digits, `.`, `_` and `-`, starting with a letter or digit.
- Paths that go into unit files must be absolute and use only `A-Za-z0-9._/-`.
- Install, rollback and uninstall refuse to change a drop-in or launcher that differs from
  the last state this layout wrote (`rollback/owned`, or `original` before the first
  install). They print `drift` and change nothing, so an operator edit is never overwritten.
- Install, rollback and uninstall are transactions. The live pair and the rollback history
  (`previous` and `owned`) are saved first. A failed step, or a HUP, INT or TERM signal,
  restores both, so such a run never leaves a mixed drop-in and launcher pair, and never changes
  what a rollback would restore. `previous` and `owned` are replaced only after the new state is
  complete.
- Uninstall refuses to change a drop-in it did not write (no `release_id` line). A
  rollback never destroys the pair it replaces, because that pair is saved as `previous`.
- New poll wrappers pin the native client in the immutable release. Older releases can
  still use their original Python wrapper when restored for rollback.
- The drop-in is `99-rust-release.conf`, so it sorts after the operator drop-ins. systemd
  applies drop-ins in name order and the last `ExecStart` wins. Install and rollback `--to`
  refuse when a drop-in in the user unit load paths sorts after ours and sets `ExecStart`, or
  when any drop-in with the same name sits in another load path, because that file replaces
  ours in systemd's precedence order. The load paths are those of `systemd.unit(5)`, Table 2:
  the `user.control`, `transient`, `generator.early`, `generator` and `generator.late`
  directories under `$XDG_RUNTIME_DIR`; `$XDG_CONFIG_HOME` (or `~/.config`) and each
  `$XDG_CONFIG_DIRS` entry; `/etc/systemd/user`; `/run/systemd/user`; `$XDG_DATA_HOME` (or
  `~/.local/share`) and each `$XDG_DATA_DIRS` entry; `/usr/local/lib/systemd/user`; and
  `/usr/lib/systemd/user`. Each contributes `<unit>.d` and the type-wide `service.d`. The check
  reads each drop-in the way systemd does: whitespace around `=` and before a key is ignored,
  comment lines are skipped, and symlinked drop-ins are read too. `--scan-dir DIR` adds a root.
  `--no-standard-scan` limits the scan to `--unit-dir`, and is accepted only when
  `DUNKINGSHEEP_RELEASE_TEST_FIXTURES=1`, which the test fixtures set. Manager environment
  changes, such as `SYSTEMD_UNIT_PATH`, are not read here. A drop-in created after install is
  not seen, so the cutover must confirm the effective drop-ins with
  `systemctl --user show -p DropInPaths dunkingsheep.service`.
- A symbolic link at the drop-in or at a launcher path is refused before anything is written,
  and a restore refuses a link too. Writing through a link would change another file.

## Activation gate (not part of this change)

Switching the live units or launchers to a release requires, before the cutover:
the owner's authorization, a cutover data check on copies (SQLite backup API or
quiesced copies, the schema, and the state files), and a tested rollback. Those
steps are in the t3229 task notes. These scripts only prepare the artifacts.

## Known limits

- This changes the poll client only; harness hooks and older rollback releases may still
  require Python and need their own inventory and migration.
- The guard shims still compile in the Python wrappers (audit P0-8, t3230). They are
  not changed here.
- The drop-in template targets the base unit `dunkingsheep.service`. The live
  Python drop-in is not read or changed.
- A SIGKILL or a power loss during a run can leave the live state mixed. The next run then
  refuses with `drift`. The state saved before the run is in `<prefix>/rollback/.txn` (the drop-in
  and launchers) and `<prefix>/rollback/.txn-meta` (`previous` and `owned`). Restore each saved
  file or directory by hand, or remove the live one whose `.absent` marker is present, then run
  the command again. A handled failure restores the state and removes both directories. A
  `*.bad` directory in the rollback directory is a replaced copy and is safe to remove.
- A failed install can leave a `releases/.staging-*` directory. It is never used and can be
  removed.
- Six operator drop-ins on the reviewed host (numbered 30 to 96) set `ExecStart` to Python
  releases. They are left unchanged here, and `99-rust-release.conf` overrides them while it is
  installed. Retiring them belongs to the owner-authorized cutover. Read the effective value with
  `systemctl --user show -p ExecStart dunkingsheep.service`; this change never runs that.

## NOT VERIFIED

- Production activation, live units, live launchers, and daemon reload or restart.
- The cutover data check: no live store was read or copied.
- Rollback under a live Python-to-Rust switch.
