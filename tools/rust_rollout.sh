#!/bin/sh
# t3471: staged rollout and rollback of the Rust daemon (dunkingsheep, ds-cli). It mirrors
# tools/t514_rollout.sh for the Rust binary, and implements sections 8 and 9 of the cutover runbook
# (CUTOVER_READINESS_RECEIPT.md; t808, t814, t1634).
#
# OPERATOR-ONLY. Dry-run by default: every step PRINTS the commands it would run and changes nothing.
# --apply executes. Only the operator runs --apply, and only after the human owner's explicit go.
# The author of a change never runs --apply on it.
#
#   tools/rust_rollout.sh plan      --sha SHA            the whole sequence, no changes
#   tools/rust_rollout.sh stage     --sha SHA [--apply]  build the pinned revision, install the binary, no restart
#   tools/rust_rollout.sh activate  --sha SHA [--apply]  stop timers, backup, record state, drop-in, launcher, restart
#   tools/rust_rollout.sh verify    [--apply]            read-only checks; with --apply also start the timers and tick each once
#   tools/rust_rollout.sh rollback  [--apply]            reverse the activation, from the state record it wrote
#
# Stage and activate are separate on purpose. Stage is reversible and silent. Activate restarts the
# daemon, which interrupts queued deliveries for a few seconds, so the operator schedules it.
# Rollback takes no --sha. It works from the state record that activate writes before its first switch
# step, so it also works after an activate that died part way through.
#
# Environment (tests and non-default hosts; the defaults are the live host's):
# Unit staging (t4699): plan|stage --unit daemon|web|telegram --sha SHA
#   --artifact FILE --artifact-sha256 LOWERCASE_SHA256 [--apply]
# This slice has no unit activation/verification/rollback or retention commands.
#
#   RUST_REPO      the checkout the pinned sha lives in         (default: this script's repo)
#   RUST_WORKTREE  the detached build worktree                   (default: <workspace>/.dunk-port-work/owner-release-wt)
#   RUST_RELEASES  the immutable release trees                   (default: $HOME/.local/share/dunkingsheep/releases)
#   RUST_CARGO     the cargo to build with                       (default: cargo)
#   RUST_OWNER     the owner's mailbox for the inbox check       (default: %1)
set -eu

TOOL_ROOT=$(cd "$(dirname "$0")/.." && pwd)
REPO=${RUST_REPO:-$TOOL_ROOT}
# Reuse the packaging path validation and private staging cleanup helpers.
. "$TOOL_ROOT/packaging/rust-release/lib.sh"
# The git common dir is the main checkout's .git, from any checkout or worktree. Its grandparent is
# the workspace that holds .dunk-port-work, where the runbook puts the owner-release worktree.
COMMON_DIR=$(cd "$REPO" && cd "$(git rev-parse --git-common-dir)" && pwd)
WORKTREE=${RUST_WORKTREE:-$(dirname "$(dirname "$COMMON_DIR")")/.dunk-port-work/owner-release-wt}
RELEASES=${RUST_RELEASES:-$HOME/.local/share/dunkingsheep/releases}
CARGO=${RUST_CARGO:-cargo}
OWNER=${RUST_OWNER:-%1}
DATA=$(dirname "$RELEASES")
BACKUPS=$DATA/backups
STATE=$DATA/rust-cutover.state
CONFIG_DIR=$HOME/.config/dunkingsheep
LAUNCHER=$HOME/.local/bin/dunkingsheep
DROPIN_DIR=$HOME/.config/systemd/user/dunkingsheep.service.d
DROPIN=$DROPIN_DIR/99-rust-cutover.conf
SQLITE_FILES="tasks.db inbox.db context-ledger.db dispatch.db agent-state.db"

cmd=${1:-plan}; shift || true
SHA=""; APPLY=0; UNIT=""; ARTIFACT=""; ARTIFACT_SHA=""
while [ $# -gt 0 ]; do
  case "$1" in
    --sha) [ $# -ge 2 ] || { echo "--sha needs a value" >&2; exit 2; }; SHA=$2; shift 2;;
    --unit|--artifact|--artifact-sha256)
      [ $# -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }
      case "$1" in
        --unit) UNIT=$2;;
        --artifact) ARTIFACT=$2;;
        --artifact-sha256) ARTIFACT_SHA=$2;;
      esac
      shift 2;;
    --apply) APPLY=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
if [ -n "$SHA" ]; then
  case "$cmd" in
    plan|stage|activate) ;;
    *) echo "$cmd takes no --sha: it reverses or checks what is recorded" >&2; exit 2;;
  esac
fi

# Unit-aware staging is opt-in. Never route a web/Telegram request into the
# legacy daemon cutover while the later switch/postcheck slice is not implemented.
if [ -n "$UNIT" ]; then
  case "$UNIT" in
    daemon) UNIT_BINARY=dunkingsheep;;
    web) UNIT_BINARY=ds-desk;;
    telegram) UNIT_BINARY=dunkingsheep-telegram;;
    *) die "--unit must be daemon, web or telegram";;
  esac
  case "$cmd" in
    plan|stage) ;;
    *) die "--unit currently supports plan/stage only; unit switching is not implemented";;
  esac
else
  [ -z "$ARTIFACT$ARTIFACT_SHA" ] || die "--artifact and --artifact-sha256 require --unit"
fi

# Shows the command in dry-run, runs it with --apply.
run() {
  if [ "$APPLY" = 1 ]; then echo "+ $*" >&2; "$@"; else echo "DRY-RUN: $*"; fi
}

# Writes stdin to the file $1 with --apply; in dry-run shows the content instead.
put() {
  if [ "$APPLY" = 1 ]; then
    echo "+ write $1" >&2
    cat > "$1"
  else
    echo "DRY-RUN: write $1:"
    sed 's/^/    /'
  fi
}

need_sha() {
  [ -n "$SHA" ] || { echo "--sha <full or short commit> is required" >&2; exit 2; }
  SHA=$(git -C "$REPO" rev-parse --verify "$SHA^{commit}")
}

# The value of KEY in the state file; empty when no cutover is recorded.
state_value() {
  [ -f "$STATE" ] || return 0
  sed -n "s/^$1=//p" "$STATE"
}

# The dunkingsheep timer units on this host. Dry-run may find none, and the plan then says so.
timer_units() {
  systemctl --user list-unit-files 'dunkingsheep-*.timer' --no-legend --plain 2>/dev/null | awk '{print $1}'
}

stage() {
  need_sha
  dest=$RELEASES/rust-$SHA
  if [ -f "$dest/dunkingsheep" ]; then
    echo "already staged: $dest"
    if [ "$APPLY" = 1 ]; then
      (cd "$dest" && sha256sum -c SHA256SUM) || { echo "$dest fails its SHA256SUM record" >&2; exit 1; }
    fi
    exit 0
  fi
  # The pinned revision is built in a detached worktree of its own, never in the operator's checkout.
  if [ -e "$WORKTREE" ]; then
    if [ "$APPLY" = 1 ]; then
      at=$(git -C "$WORKTREE" rev-parse HEAD)
      [ "$at" = "$SHA" ] || { echo "$WORKTREE is at $at, not $SHA; use another RUST_WORKTREE" >&2; exit 1; }
    fi
  else
    run git -C "$REPO" worktree add --detach "$WORKTREE" "$SHA"
  fi
  run sh -c "cd '$WORKTREE' && '$CARGO' build --release --manifest-path rust/Cargo.toml -p ds-cli --bin dunkingsheep"
  run mkdir -p "$dest"
  run cp "$WORKTREE/rust/target/release/dunkingsheep" "$dest/dunkingsheep"
  run sh -c "cd '$dest' && sha256sum dunkingsheep > SHA256SUM && '$dest/dunkingsheep' --version > VERSION"
  # Read-only last: the binary, then the directory, so nothing can be written into the release.
  run chmod 0555 "$dest/dunkingsheep"
  run chmod 0555 "$dest"
  echo "staged $SHA into $dest (no restart performed)"
}

# t4699 slice 1: stage an operator-pinned artifact without executing it. This
# never builds, writes launchers/drop-ins/state, or invokes a service/API.
unit_manifest() {
  printf 'format=1\nunit=%s\nrevision=%s\nbinary=%s\nbinary_sha256=%s\n' \
    "$UNIT" "$SHA" "$UNIT_BINARY" "$ARTIFACT_SHA"
}

check_unit_release() {
  [ ! -L "$unit_dest" ] && [ -d "$unit_dest" ] || die "invalid release directory: $unit_dest"
  for entry in "$unit_dest/$UNIT_BINARY" "$unit_dest/MANIFEST"; do
    [ ! -L "$entry" ] && [ -f "$entry" ] || die "invalid release file: $entry"
  done
  [ "$(cat "$unit_dest/MANIFEST")" = "$(unit_manifest)" ] || die "release MANIFEST differs; immutable release refused"
  [ "$(sha256sum "$unit_dest/$UNIT_BINARY" | cut -d' ' -f1)" = "$ARTIFACT_SHA" ] || die "release binary differs; immutable release refused"
  [ "$(stat -c %a "$unit_dest")" = 555 ] &&
    [ "$(stat -c %a "$unit_dest/$UNIT_BINARY")" = 555 ] &&
    [ "$(stat -c %a "$unit_dest/MANIFEST")" = 444 ] || die "release modes differ; immutable release refused"
}

cleanup_unit_stage() {
  unit_status=$?
  trap - EXIT HUP INT TERM
  if [ -n "${unit_staging:-}" ]; then
    clear_tree "$unit_staging" || echo "could not remove private staging tree: $unit_staging" >&2
  fi
  exit "$unit_status"
}

stage_unit() {
  need_sha
  require_plain_path RUST_RELEASES "$RELEASES"
  [ ! -L "$RELEASES" ] || die "release root must not be a symbolic link"
  [ -n "$ARTIFACT" ] && [ ! -L "$ARTIFACT" ] && [ -f "$ARTIFACT" ] && [ -x "$ARTIFACT" ] ||
    die "--artifact must be a regular, non-symlink executable file"
  [ "${#ARTIFACT_SHA}" = 64 ] || die "--artifact-sha256 must be a full SHA256"
  case "$ARTIFACT_SHA" in *[!a-f0-9]*) die "--artifact-sha256 must use lowercase hex";; esac
  [ "$(sha256sum "$ARTIFACT" | cut -d' ' -f1)" = "$ARTIFACT_SHA" ] || die "artifact SHA256 mismatch"
  unit_dest=$RELEASES/rust-$SHA-$UNIT
  if [ "$APPLY" = 0 ]; then
    if [ -e "$unit_dest" ] || [ -L "$unit_dest" ]; then check_unit_release; fi
    echo "DRY-RUN: stage $ARTIFACT -> $unit_dest/$UNIT_BINARY (0555)"
    echo "DRY-RUN: write $unit_dest/MANIFEST (0444), directory (0555):"
    unit_manifest
    echo "No services, launchers, drop-ins or state changed. Unit switching is not implemented."
    return
  fi
  mkdir -p "$RELEASES"
  # Stage writers serialize per unit; dry-run does not create even this lock.
  unit_lock=$RELEASES/.stage-$UNIT.lock
  [ ! -L "$unit_lock" ] || die "stage lock must not be a symbolic link"
  exec 9>"$unit_lock"
  flock -n 9 || die "another stage for $UNIT is running"
  if [ -e "$unit_dest" ] || [ -L "$unit_dest" ]; then
    check_unit_release
    echo "already staged: $unit_dest"
    return
  fi
  unit_staging=$(mktemp -d "$RELEASES/.stage-$UNIT-XXXXXXXX")
  trap cleanup_unit_stage EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  cp "$ARTIFACT" "$unit_staging/$UNIT_BINARY"
  # Detect a source that changed between the initial hash and the copy.
  [ "$(sha256sum "$unit_staging/$UNIT_BINARY" | cut -d' ' -f1)" = "$ARTIFACT_SHA" ] || die "copied artifact SHA256 mismatch"
  unit_manifest > "$unit_staging/MANIFEST"
  chmod 0555 "$unit_staging/$UNIT_BINARY"
  chmod 0444 "$unit_staging/MANIFEST"
  chmod 0555 "$unit_staging"
  mv -T "$unit_staging" "$unit_dest"
  unit_staging=""
  trap - EXIT HUP INT TERM
  echo "staged $UNIT at $SHA into $unit_dest (no activation performed)"
}

# Runbook 8.3: the backup is taken while the python daemon still serves, from a copy that holds a
# consistent snapshot of each sqlite database, so the live WAL files are never copied. The config holds
# tokens (github-tokens.json is written 0600 by the daemon), so the archive and its directory are private
# whatever umask the caller has.
backup() {
  ts=$1
  target=$BACKUPS/pre-rust-$ts.tar
  work=$BACKUPS/.pre-rust-$ts.work
  excludes=""
  for db in $SQLITE_FILES; do
    excludes="$excludes --exclude=$db --exclude=$db-wal --exclude=$db-shm"
  done
  run mkdir -p "$BACKUPS"
  if [ "$APPLY" = 1 ]; then
    old_umask=$(umask)
    umask 077
    chmod 0700 "$BACKUPS"
    mkdir "$work"
    # shellcheck disable=SC2086
    tar -C "$CONFIG_DIR" $excludes -cf - . | tar -C "$work" -xf -
    for db in $SQLITE_FILES; do
      if [ -f "$CONFIG_DIR/$db" ]; then
        sqlite3 "$CONFIG_DIR/$db" ".backup '$work/$db'"
        check=$(sqlite3 "$work/$db" "PRAGMA integrity_check")
        [ "$check" = ok ] || { echo "backup of $db failed integrity_check: $check" >&2; exit 1; }
      fi
    done
    tar -cf "$target" -C "$work" .
    chmod 0600 "$target"
    tar -tf "$target" >/dev/null
    chmod -R u+w "$work" 2>/dev/null; rm -rf "$work"
    umask "$old_umask"
  else
    echo "DRY-RUN: umask 077; chmod 0700 $BACKUPS"
    echo "DRY-RUN: tar -C $CONFIG_DIR (all but $SQLITE_FILES and their -wal/-shm) | tar -C $work -xf -"
    echo "DRY-RUN: sqlite3 <db> \".backup '$work/<db>'\" and PRAGMA integrity_check, for each of: $SQLITE_FILES"
    echo "DRY-RUN: tar -cf $target -C $work . ; chmod 0600 $target"
  fi
  echo "backup: $target"
}

# A failure after the timers stop and before the state record leaves the python daemon serving, so the
# timers go back on. The trap is replaced once the record exists.
restart_timers_on_failure() {
  echo "activate FAILED before the switch; the python daemon still serves; restarting the timers" >&2
  if [ "$APPLY" = 1 ]; then systemctl --user start dunkingsheep-deepcheck.timer dunkingsheep-dispatch.timer dunkingsheep-health.timer dunkingsheep-review.timer dunkingsheep-state.timer || true; fi
}

activate() {
  need_sha
  dest=$RELEASES/rust-$SHA
  bin=$dest/dunkingsheep
  ts=$(date -u +%Y%m%dT%H%M%SZ)
  if [ "$APPLY" = 1 ]; then
    [ -x "$bin" ] || { echo "stage $SHA first: $bin is not there" >&2; exit 1; }
    (cd "$dest" && sha256sum -c SHA256SUM >/dev/null) || { echo "$dest fails its SHA256SUM record; refusing" >&2; exit 1; }
    [ -f "$LAUNCHER" ] || { echo "no launcher at $LAUNCHER to keep" >&2; exit 1; }
    [ ! -e "$STATE" ] || { echo "a cutover is already recorded in $STATE; roll it back first" >&2; exit 1; }
  fi
  echo "pinned: $SHA   binary: $bin"
  # 1. Stop the timers so no tick writes during the backup, then back up while python still serves.
  run systemctl --user stop 'dunkingsheep-*.timer'
  trap restart_timers_on_failure EXIT
  backup "$ts"
  # 2. Record before the first switch step, so every later step is reversible from this record even if
  #    activate dies part way through. The drop-in that is about to be replaced is copied first.
  dropin_before=absent
  if [ -f "$DROPIN" ]; then
    dropin_before=$BACKUPS/dropin-before-$ts.conf
    run cp -p "$DROPIN" "$dropin_before"
  fi
  put "$STATE" <<EOF
sha=$SHA
ts=$ts
status=switching
binary=$bin
launcher=$LAUNCHER
launcher_backup=$LAUNCHER.python-$ts
dropin=$DROPIN
dropin_before=$dropin_before
backup=$BACKUPS/pre-rust-$ts.tar
EOF
  # 3. The switch. A failure from here on names the rollback, which works from the record above.
  copy_tmp=$LAUNCHER.python-$ts.tmp
  trap 'rm -f "$copy_tmp"; echo "activate FAILED during the switch; run: $0 rollback --apply (it works from $STATE)" >&2' EXIT
  run mkdir -p "$DROPIN_DIR"
  put "$DROPIN" <<EOF
[Service]
# t3471 Rust cutover (runbook section 8 step 4). A later drop-in overrides ExecStart.
ExecStart=
ExecStart=$bin serve
EOF
  # The python launcher is kept as dunkingsheep.python-<ts>. It is copied under a temporary name and moved
  # into place only when the copy equals the original, so a copy that fails midway never sits under the
  # name that rollback restores from. The new launcher then execs the Rust binary.
  run cp -p "$LAUNCHER" "$copy_tmp"
  run cmp "$LAUNCHER" "$copy_tmp"
  run mv "$copy_tmp" "$LAUNCHER.python-$ts"
  put "$LAUNCHER" <<EOF
#!/bin/sh
# t3471: this launcher execs the Rust daemon release $SHA. The python launcher is $LAUNCHER.python-$ts
exec $bin "\$@"
EOF
  run chmod 0755 "$LAUNCHER"
  run systemctl --user daemon-reload
  run systemctl --user restart dunkingsheep.service
  # 4. Done: the record says so.
  put "$STATE" <<EOF
sha=$SHA
ts=$ts
status=active
binary=$bin
launcher=$LAUNCHER
launcher_backup=$LAUNCHER.python-$ts
dropin=$DROPIN
dropin_before=$dropin_before
backup=$BACKUPS/pre-rust-$ts.tar
EOF
  trap - EXIT
  echo "activated $SHA. The timers stay stopped until: $0 verify --apply (nothing ticks before that)."
  echo "Run $0 verify first."
}

verify() {
  echo "## service"; systemctl --user is-active dunkingsheep.service 2>&1 || true
  echo "## launcher"; sed -n '1,3p' "$LAUNCHER" 2>&1 || true
  echo "## drop-in"; sed -n '1,4p' "$DROPIN" 2>&1 || true
  echo "## status"; dunkingsheep status 2>&1 | sed -n '1,8p' || true
  echo "## whoami"; dunkingsheep whoami 2>&1 | sed -n '1,6p' || true
  echo "## inbox count ($OWNER)"; dunkingsheep inbox count "$OWNER" 2>&1 | head -5 || true
  echo "## task list (in_progress)"; dunkingsheep task list --status in_progress 2>&1 | head -20 || true
  echo "## health"; dunkingsheep health 2>&1 | head -20 || true
  echo "## timers"
  units=$(timer_units)
  if [ "$APPLY" = 1 ]; then
    run systemctl --user start dunkingsheep-deepcheck.timer dunkingsheep-dispatch.timer dunkingsheep-health.timer dunkingsheep-review.timer dunkingsheep-state.timer
    for t in $units; do
      s=${t%.timer}.service
      run systemctl --user start "$s"
      run journalctl --user -u "$s" -n 5 --no-pager
    done
  else
    echo "DRY-RUN: start the timers, then one tick of each timer's service and its journal, with --apply"
    for t in $units; do echo "DRY-RUN: tick ${t%.timer}.service"; done
  fi
}

# Runbook 9, worked from the state record. The record exists from before the first switch step, so this
# also undoes an activate that died part way. The state is not restored here (9.4): the operator checks
# the python release first.
rollback() {
  if [ "$APPLY" = 1 ]; then
    [ -f "$STATE" ] || { echo "no cutover recorded at $STATE; nothing to roll back" >&2; exit 1; }
  fi
  if [ -f "$STATE" ]; then
    rb_sha=$(state_value sha)
    rb_status=$(state_value status)
    rb_launcher=$(state_value launcher)
    rb_launcher_backup=$(state_value launcher_backup)
    rb_dropin=$(state_value dropin)
    rb_dropin_before=$(state_value dropin_before)
    rb_backup=$(state_value backup)
  else
    rb_sha="<sha recorded at activate>"
    rb_status="<status recorded at activate>"
    rb_launcher="<launcher recorded at activate>"
    rb_launcher_backup="<launcher backup recorded at activate>"
    rb_dropin="<drop-in recorded at activate>"
    rb_dropin_before="<drop-in backup recorded at activate>"
    rb_backup="<backup recorded at activate>"
  fi
  echo "cutover $rb_sha, recorded status: $rb_status"
  # 1. Stop the timers and the daemon.
  run systemctl --user stop 'dunkingsheep-*.timer' dunkingsheep.service
  # 2. The drop-in goes back to what it was before activate: removed if there was none, restored if there was.
  if [ "$rb_dropin_before" = absent ]; then
    run rm -f "$rb_dropin"
  else
    run cp -p "$rb_dropin_before" "$rb_dropin"
  fi
  # 3. The launcher: restore the python copy if activate made it. If activate died before that copy,
  #    the launcher was never changed, so there is nothing to restore.
  if [ "$APPLY" = 1 ] && [ ! -f "$rb_launcher_backup" ]; then
    echo "no python copy at $rb_launcher_backup: the launcher was not changed by this cutover"
  else
    run cp -p "$rb_launcher_backup" "$rb_launcher"
  fi
  # 4. The state is not restored here. This run restarts the daemon and starts the timers in one go, so the
  #    check has to come BEFORE --apply. Restore only if the python release cannot read the state.
  echo "state: not restored automatically. Check BEFORE --apply, because this run restarts the daemon and"
  echo "  starts the timers in one go:  python3 <python release>/dunkingsheep --no-start-daemon status"
  echo "  if the python release cannot read the state, restore it from $rb_backup by hand, then --apply"
  # 5. Reload, restart the daemon, start the timers, and close the record.
  run systemctl --user daemon-reload
  run systemctl --user restart dunkingsheep.service
  run systemctl --user start dunkingsheep-deepcheck.timer dunkingsheep-dispatch.timer dunkingsheep-health.timer dunkingsheep-review.timer dunkingsheep-state.timer
  run mv "$STATE" "$STATE.rolled-back-$(date -u +%Y%m%dT%H%M%SZ)"
  echo "rolled back from $rb_sha; now run: $0 verify   (dunkingsheep status must show the python version)"
}

plan() {
  need_sha
  dest=$RELEASES/rust-$SHA
  echo "pinned revision: $SHA"
  echo "binary:          $dest/dunkingsheep (0555, SHA256SUM recorded at stage)"
  echo "1. stage:    $0 stage --sha $SHA --apply      build in $WORKTREE, install the binary, no restart"
  echo "2. activate: $0 activate --sha $SHA --apply   stop timers, backup state to $BACKUPS (private),"
  echo "                                              record the state, drop-in 99-rust-cutover.conf,"
  echo "                                              launcher exec, daemon-reload, restart (interrupts deliveries for seconds)"
  echo "3. verify:   $0 verify                        read-only checks"
  echo "             $0 verify --apply                start the timers, then one tick of each and its journal"
  echo "4. rollback: $0 rollback --apply              stop, drop-in restored, python launcher restored, restart, timers"
  echo "             (check the python release can read the state BEFORE --apply: its dry-run output says how)"
  echo "Dry-run by default: nothing above changes anything without --apply. Operator only."
}

if [ -n "$UNIT" ]; then
  # A plan always stays read-only, including when --apply was accidentally supplied.
  if [ "$cmd" = plan ]; then APPLY=0; fi
  stage_unit
  exit 0
fi

case "$cmd" in
  plan) plan;;
  stage) stage;;
  activate) activate;;
  verify) verify;;
  rollback) rollback;;
  *) echo "usage: $0 plan|stage|activate|verify|rollback [--sha SHA] [--apply]" >&2; exit 2;;
esac
