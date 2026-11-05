#!/bin/sh
# Installs one immutable Rust release of the Dunking Sheep CLI (t3229, audit P0-1).
#
#   <prefix>/releases/<id>/                  dunkingsheep (0555), dunk-poll.py (0444), MANIFEST
#   <bin-dir>/dunkingsheep, <bin-dir>/dunk-poll
#       launchers, rendered by the release binary's own `install` verb
#   <unit-dir>/dunkingsheep.service.d/10-rust-release.conf
#       drop-in that resets ExecStart to the release binary and keeps DUNKINGSHEEP_SUPERVISED=1
#
# Every path is required. Nothing defaults to $HOME, and systemctl is never run, so
# activation stays a separate, owner-approved step. Before changing anything, the pair of
# drop-in and launchers is snapshotted under <prefix>/rollback: once as "original", the
# pre-Rust state, and as "previous", the state before this install. Re-running with the
# same release id and identical bytes is a no-op. Different bytes under an existing id
# are refused, because releases are immutable.
#
# usage: install-release.sh --binary FILE --poll FILE --release-id ID --prefix DIR
#            --bin-dir DIR --unit-dir DIR [--revision TEXT] [--dry-run]

set -eu
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/lib.sh"

binary=""
poll=""
release_id=""
prefix=""
bin_dir=""
unit_dir=""
revision="unknown"
dry_run=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary) binary=${2:-}; shift 2 ;;
        --poll) poll=${2:-}; shift 2 ;;
        --release-id) release_id=${2:-}; shift 2 ;;
        --prefix) prefix=${2:-}; shift 2 ;;
        --bin-dir) bin_dir=${2:-}; shift 2 ;;
        --unit-dir) unit_dir=${2:-}; shift 2 ;;
        --revision) revision=${2:-}; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$binary" ] || die "--binary is required"
[ -n "$poll" ] || die "--poll is required"
[ -n "$release_id" ] || die "--release-id is required"
[ -n "$prefix" ] || die "--prefix is required"
[ -n "$bin_dir" ] || die "--bin-dir is required"
[ -n "$unit_dir" ] || die "--unit-dir is required"
require_release_id "$release_id"
require_plain_path --prefix "$prefix"
require_plain_path --bin-dir "$bin_dir"
require_plain_path --unit-dir "$unit_dir"
[ -f "$binary" ] && [ -x "$binary" ] || die "--binary is not an executable file: $binary"
[ -f "$poll" ] || die "--poll is not a file: $poll"

release_dir="$prefix/releases/$release_id"
reuse=0
if [ -e "$release_dir" ]; then
    if cmp -s "$binary" "$release_dir/dunkingsheep" && cmp -s "$poll" "$release_dir/dunk-poll.py"; then
        reuse=1
    else
        die "release $release_id already exists with different contents; releases are immutable"
    fi
fi

dropin_dir="$unit_dir/dunkingsheep.service.d"
dropin="$dropin_dir/10-rust-release.conf"

guard_against_drift "$prefix/rollback" "$dropin" "$bin_dir"
if [ "$dry_run" -eq 1 ]; then
    echo "dry run: release $release_id -> $release_dir (reuse=$reuse)"
    echo "dry run: launchers -> $bin_dir (dunkingsheep, dunk-poll)"
    echo "dry run: drop-in -> $dropin"
    echo "dry run: pair snapshots -> $prefix/rollback (original once; previous per release)"
    exit 0
fi

if [ "$reuse" -eq 0 ]; then
    mkdir -p "$prefix/releases"
    staging="$prefix/releases/.staging-$release_id-$$"
    rm -rf "$staging"
    mkdir "$staging"
    cp "$binary" "$staging/dunkingsheep"
    cp "$poll" "$staging/dunk-poll.py"
    chmod 0555 "$staging/dunkingsheep"
    chmod 0444 "$staging/dunk-poll.py"
    {
        echo "release_id=$release_id"
        echo "revision=$revision"
        echo "binary_sha256=$(sha256sum "$staging/dunkingsheep" | cut -d' ' -f1)"
        echo "poll_sha256=$(sha256sum "$staging/dunk-poll.py" | cut -d' ' -f1)"
    } > "$staging/MANIFEST"
    chmod 0444 "$staging/MANIFEST"
    mv "$staging" "$release_dir"
fi

rollback_dir="$prefix/rollback"
mkdir -p "$rollback_dir" "$bin_dir" "$dropin_dir"
if [ ! -d "$rollback_dir/original" ]; then
    snapshot_pair "$rollback_dir/original" "$dropin" "$bin_dir"
fi
guard_against_drift "$rollback_dir" "$dropin" "$bin_dir"
if [ "$(dropin_release_id "$dropin")" != "$release_id" ]; then
    snapshot_pair "$rollback_dir/previous" "$dropin" "$bin_dir"
fi
"$release_dir/dunkingsheep" install --bin-dir "$bin_dir" > /dev/null
render_dropin "$SCRIPT_DIR/templates/10-rust-release.conf.in" "$release_id" \
    "$release_dir/dunkingsheep" "$dropin"

snapshot_pair "$rollback_dir/owned" "$dropin" "$bin_dir"
echo "installed release $release_id"
echo "release:  $release_dir"
echo "launcher: $bin_dir/dunkingsheep"
echo "poll:     $bin_dir/dunk-poll"
echo "drop-in:  $dropin"
echo "rollback: $rollback_dir (original and previous pairs)"
echo "not activated: no systemctl was run. Reload and restart only after the cutover gate."
