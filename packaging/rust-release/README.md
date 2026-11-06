# Rust release packaging (t3229, audit P0-1)

Preparatory packaging for the Rust `dunkingsheep` CLI and daemon. It does not
activate anything. It makes an immutable release layout, launchers and a systemd
drop-in that point at that layout, and rollback and uninstall for both. Activation
is a separate, owner-approved cutover step.

## Layout

```
<prefix>/releases/<id>/dunkingsheep   Rust binary (0555)
<prefix>/releases/<id>/dunk-poll.py   copy of tools/dunk_poll.py (0444)
<prefix>/releases/<id>/MANIFEST       release_id, revision, sha256 of both files (0444)
<bin-dir>/dunkingsheep                launcher: exec the release binary
<bin-dir>/dunk-poll                   poll wrapper: pinned python3 + the release's poller copy
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
packaging/rust-release/install-release.sh --binary FILE --poll tools/dunk_poll.py \
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
- The poll wrapper pins an absolute `python3`. A `python3` found through a relative PATH
  entry is refused, so the wrapper never depends on the working directory.
- The drop-in is `99-rust-release.conf`, so it sorts after the operator drop-ins. systemd
  applies drop-ins in name order and the last `ExecStart` wins. Install refuses when a
  drop-in that sorts after it sets `ExecStart`, because that drop-in would silently override
  the release. The check reads each drop-in the way systemd does: whitespace around `=` and
  before a key is ignored, comment lines are skipped, and symlinked drop-ins are read too. It
  scans the user unit load paths from systemd.unit(5), including `~/.config/systemd/user`,
  `/etc/systemd/user`, `/run/systemd/user` and `/usr/lib/systemd/user`, and the type-wide
  `service.d` directory in each. A same-named drop-in elsewhere that sets `ExecStart` is refused
  too, because it can shadow ours. `--scan-dir DIR` adds a root. `--no-standard-scan` limits the
  scan to `--unit-dir` and exists for the test fixtures; the cutover must not use it. A drop-in
  created after install is not seen, so the cutover must confirm the effective drop-ins with
  `systemctl --user show -p DropInPaths dunkingsheep.service`.

## Activation gate (not part of this change)

Switching the live units or launchers to a release requires, before the cutover:
the owner's authorization, a cutover data check on copies (SQLite backup API or
quiesced copies, the schema, and the state files), and a tested rollback. Those
steps are in the t3229 task notes. These scripts only prepare the artifacts.

## Known limits

- `dunk-poll` still runs Python: a copy of `tools/dunk_poll.py` inside the release
  (audit P1-9 makes it a Rust verb).
- The guard shims still compile in the Python wrappers (audit P0-8, t3230). They are
  not changed here.
- The drop-in template targets the base unit `dunkingsheep.service`. The live
  Python drop-in is not read or changed.
- A SIGKILL or a power loss during a run can leave the live state mixed. The next run then
  refuses with `drift`. The state saved before the run is in `<prefix>/rollback/.txn` (the drop-in
  and launchers) and `<prefix>/rollback/.txn-meta` (`previous` and `owned`). Restore each saved
  file or directory by hand, or remove the live one whose `.absent` marker is present, then run
  the command again. A failed run keeps both directories until the next run replaces them. A
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
