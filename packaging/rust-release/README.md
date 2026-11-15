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

## Reviewed three-unit packet (t4699, second slice)

`tools/rust_rollout.sh` also accepts a coordinated, already-staged packet:

```sh
RUST_RELEASE_OPERATOR=/absolute/reviewed/bin/ds-release \
  tools/rust_rollout.sh plan --packet /absolute/packet --packet-sha SHA256SUMS_SHA
# Authorized operator only, after tooling and packet review and a current GO:
RUST_RELEASE_OPERATOR=/absolute/reviewed/bin/ds-release \
  tools/rust_rollout.sh activate --packet /absolute/packet --packet-sha SHA256SUMS_SHA --apply
```

Build `ds-release` from the independently reviewed revision with the managed
Cargo wrapper (`build --locked --release -p ds-cli --bin ds-release`, in `rust/`).
The entrypoint never builds or installs its helper automatically. Pin an absolute,
regular executable with `RUST_RELEASE_OPERATOR`, or install it as `ds-release` on
PATH. Do not mix packet flags with legacy `--sha`, `--unit` or artifact flags.
`plan`, `activate`, `verify` and `rollback` are dry-run by default: they validate
packet/artifact hashes without host commands, locks or writes. `plan --apply`
remains dry-run. `verify --apply` performs live checks without restarting units;
it writes only the private operation lock. No packet operation starts timers.

The packet contract follows the accepted t5007 procedure (`operate.py` SHA256
`487b4835f99df2770937e7ac1aff9d8f6b33b6d2df1293b6e7b6543e55553144`).
`STAGING-MANIFEST.json` records the full source commit, operator, exact three
units, immutable source/stage/release artifact hashes, baseline PIDs, executables,
unit-file hashes and `DropInPaths`, candidate drop-in/snippet paths, before/after
CLI hashes, unchanged poll hashes, dedicated Telegram token path and the explicit
`rollback_triggers` schema contract. `SHA256SUMS` covers the manifest, artifacts,
snippets, `launcher.before`, `launcher.pending` and `rollback-triggers.sql`.
The required `lan_authority` is the reviewed LAN Host header for terminal probes;
the TCP connection remains loopback. The digest supplied on the command line is
the SHA256 of `SHA256SUMS` itself.
The SQL file is review evidence, never executed as arbitrary SQL. Modern packets
set `rollback_triggers: []`: rollback snapshots the quiesced database and retains
every table, row, index and trigger, including additive mailbox retirement audit
objects. The original three t5007 sidecar trigger names remain accepted only as
an explicit historical contract; arbitrary or partial trigger lists are refused.
An empty list grants no downgrade DDL. The retained snapshot filename
`inbox-before-trigger-drop.db` is historical and also serves snapshot-only packets;
its first complete bytes are kept across recovery retries.

A packet still records baselines for exactly `daemon`, `web` and `telegram`.
Each unit can state `"switch": true` or `"switch": false`; omission retains the
historical all-three-selected behavior. At least one unit must be selected.
Selected units provide candidate artifacts, a `drop` and a relative `snippet`.
Untouched units omit `drop` and `snippet`, set `release` to the baseline executable's
directory, and provide no artifact rows. Their PID, executable hash and baseline
configuration are verified unchanged throughout switching, verification and
recovery. If daemon is untouched, both saved launcher hashes/files must describe
the same current CLI bytes. The plan reports only the effective selected order.
Selection is bound by the reviewed manifest digest; packet mode does not mix with
the legacy `--unit` artifact-staging flags.

Only selected units are switched in daemon/web/Telegram order, and only selected
units are controlled during reverse-order rollback. A daemon-only packet leaves
web and Telegram running; a web-only packet leaves daemon/CLI and Telegram running.
No-terminal confirmation and live tmux-control detachment checks apply when web
is selected, including immediately before its switch. They are not required for
a daemon- or Telegram-only switch. Web HTTP/Host/auth gates are still checked
when web remains at its baseline. Existing review, GO, one-attempt, rollback
snapshot and consumer-floor compatibility requirements remain in force.

The separate private `state/ACTIVATION-GO.json` must contain `packet_sha256`,
`authority_message`, RFC3339 `valid_from`/`valid_until`, `no_terminal_confirmed:
true` when web is selected, nonempty `rollback_triggers` and `observations_only` arrays, and the explicit
boolean `rollback_on_apply_failure: true`. `state/REVIEW-ACCEPTANCE.json` must
contain the same packet digest, `accepted: true`, a nonempty `reviewer` different
from the manifest operator, and `evidence_message`. These records express the
coordinator's authorization; same-user files are not a privilege boundary.
The tooling author never activates their own change. A different authorized
operator must execute the independently reviewed tooling and packet.

Activation records its one attempt before changing anything, then switches
selected daemon/CLI, web and Telegram units in that order. It checks live executable bytes,
restart counts, unchanged poll, daemon CLI reads, exact served UI bytes, terminal
authentication/Host gates and the bridge's dedicated token-file environment.
The token must be a same-user regular 0600 file registered as exactly
`telegram-bridge system`; the helper never reads its contents or issues tokens.
It checks actual attached tmux control clients before activation and immediately
before switching web, while ignoring usage-provider children.

An apply failure attempts rollback in reverse order. An incomplete rollback
returns failure with instructions to wait/detach as needed and rerun the same
`rollback --packet ... --packet-sha ... --apply` command. Recovery uses the GO
captured in `ACTIVATION-START.json`, including after its activation window expires;
it never opens a second activation attempt. Recovery validates its manifest,
snippets, saved old launcher, old executables, baseline configuration, poll and
snapshot rather than disposable build outputs or staged candidate artifacts.
Activation and verification retain full source/stage/release validation.
A completed first SQLite snapshot is
published without replacement and retained across retries. Legacy recovery accepts
already-dropped triggers; snapshot-only recovery changes no schema. Both restart
an old unit left inactive after overlay
removal. Drift causes refusal before removing that overlay. Inspect the private
state receipts and the reviewed release-specific observation window separately;
the executor does not infer extra rollback triggers from prose or monitor later
timer/delivery events.

This slice deliberately preserves legacy drop-ins and refuses a candidate that
does not sort after the recorded baseline drop-ins. A stable single drop-in with
reviewed backup/migration, release listing and retention, packet authoring, and a
real three-unit release by a different operator remain parent-task gates. There
is no live activation as part of this tooling PR.

## Read-only release inventory (t4699, third slice)

The native helper now reports references without changing releases, launchers,
units or operation state. It refuses `--apply` and packet operation flags before
reading host data. The shell entrypoint delegates to the same reviewed executable:

```sh
RUST_RELEASE_OPERATOR=/absolute/reviewed/bin/ds-release \
  tools/rust_rollout.sh inventory \
  --record-root /absolute/workspace/.dunk-port-work \
  --constraints /absolute/checkout/packaging/rust-release/examples/b24-consumer-floor.json
# Native invocation also accepts --releases /absolute/release/root:
ds-release inventory --record /absolute/packet/STAGING-MANIFEST.json --json
```

`RUST_RELEASES` selects the wrapper's release root; the native default is
`$HOME/.local/share/dunkingsheep/releases`. Repeat `--record`, `--record-root`
and `--launcher` to extend the observed inputs. The two default launchers are
`$HOME/.local/bin/dunkingsheep` and `dunk-poll`; additional launchers do not replace
them. A record root is inspected only for `STAGING-MANIFEST.json` at its root and
direct child directories. The inventory reads known manifest fields and the two
known result receipts, never workspace notes, stores, tokens or credentials.
It does not require disposable build outputs to remain present.

Every run reads all three units with `systemctl --user show/cat`, showing their
fragment/drop-in chains and the winning `ExecStart` when the two views agree.
Every observed overlay reference is retained, including overridden assignments.
The report unions those paths with same-user `/proc/*/exe` links (no argv or
environment), launchers, recorded baselines, staged candidates and poll references.
Text and JSON render the same collected report. No command evaluates launcher
shell text or writes a lock, index, snapshot or result receipt. Ordinary filesystem
reads can update access times; the fixtures compare content, modes and modification
times, not access times.

Each release entry reports logical bytes and reference roles. Unrecognized legacy
manifests, symbolic links, incomplete discovery, malformed records and uncertain
floor compatibility are `unknown: keep`. Recognized manifests have their binary
hash checked. `unreferenced in observed inputs` is scoped evidence, never permission
to remove anything. All dispositions retain the entry. Logical bytes do not
estimate physical reclaim; hashes and mtimes do not establish release order.
There is no removal action or registered global packet index. This read-only
snapshot cannot establish deletion safety against concurrent filesystem changes.

The checked-in constraint example records the coordinator's standing desk/Telegram
floor of `b24f6409`: implicit acknowledgement rows exist since t5168 on
2026-11-15 at 01:43 PST. It includes the reason and source receipt. Constraint data
is explicit, not inferred from history or enforced as authorization: revisions
other than the named floor remain uncertain because Git ancestry is not inferred.
Historical rollback references do not override that floor.

This follows the t4699 `RETENTION-LIST-DESIGN.txt` task evidence and reuses the
packet fields in `ds-core::release_packet`; the packet executor is unchanged.
Before another operator can perform a real native-path release, independently
review its exact tooling head and supported packet, rehearse its actual switching
and recovery order against the current consumer floor, and obtain current review
acceptance and GO. The executor switches selected units in daemon/web/Telegram order and rolls
those units back in reverse order with the packet's explicit schema contract;
switching order and rollback-floor compatibility still need release-specific
review or a separate fix.
Stable single-drop-in migration, packet authoring, retention/removal and a real
release through the native tool by a different operator remain parent-task gates.

## Legacy activation gate

Switching the live units or launchers to a release requires, before the cutover:
the owner's authorization, a cutover data check on copies (SQLite backup API or
quiesced copies, the schema, and the state files), and a tested rollback. Those
steps are in the t3229 task notes. These scripts only prepare the artifacts.

## Desk/daemon switch checklist

- [ ] Confirm whether the deployed health implementation includes PR 976 (t3912).
- [ ] Once PR 976 is deployed: health unit exit 2 on a disk-only problem (root >= 85% until < 80%, or since t3912 slice 1 /tmp >= 85% of blocks or inodes until < 80%) is an OBSERVATION, never a rollback trigger.

Confirm that the health report identifies only the root-disk condition before
applying this exception, and record the observation in the postcheck evidence.
Evaluate other failures against the release's reviewed rollback rules.

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
