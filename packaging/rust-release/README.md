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
<unit-dir>/dunkingsheep.service.d/10-rust-release.conf
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
- Uninstall and rollback write only the files the saved pair names. Anything the
  install did not write is left alone.

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

## NOT VERIFIED

- Production activation, live units, live launchers, and daemon reload or restart.
- The cutover data check: no live store was read or copied.
- Rollback under a live Python-to-Rust switch.
