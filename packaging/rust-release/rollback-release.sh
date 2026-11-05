#!/bin/sh
# Rolls the Rust CLI back (t3229, audit P0-1). Never runs systemctl.
#
# The drop-in and both launchers always move together as one pair, so a rollback never
# leaves a launcher that disagrees with the drop-in:
#   (no option)  restore the pair saved before the last install-release.sh run or rollback
#   --to ID      point the drop-in and the launchers at release ID, installed under
#                <prefix>/releases/ID
#   --original   restore the pair saved before the first install (the pre-Rust state)
# Each of these saves the pair being replaced as "previous". Running the same rollback
# again therefore undoes it.
#
# usage: rollback-release.sh --prefix DIR --bin-dir DIR --unit-dir DIR
#            [--to ID | --original] [--dry-run]

set -eu
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/lib.sh"

prefix=""
bin_dir=""
unit_dir=""
to=""
original=0
dry_run=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix) prefix=${2:-}; shift 2 ;;
        --bin-dir) bin_dir=${2:-}; shift 2 ;;
        --unit-dir) unit_dir=${2:-}; shift 2 ;;
        --to) to=${2:-}; shift 2 ;;
        --original) original=1; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$prefix" ] || die "--prefix is required"
[ -n "$bin_dir" ] || die "--bin-dir is required"
[ -n "$unit_dir" ] || die "--unit-dir is required"
require_plain_path --prefix "$prefix"
require_plain_path --bin-dir "$bin_dir"
require_plain_path --unit-dir "$unit_dir"
if [ -n "$to" ] && [ "$original" -eq 1 ]; then
    die "--to and --original are exclusive"
fi

dropin="$unit_dir/dunkingsheep.service.d/10-rust-release.conf"
rollback_dir="$prefix/rollback"

if [ -n "$to" ]; then
    require_release_id "$to"
    target="$prefix/releases/$to/dunkingsheep"
    [ -x "$target" ] || die "release $to is not installed: $target"
    guard_against_drift "$rollback_dir" "$dropin" "$bin_dir"
    if [ "$dry_run" -eq 1 ]; then
        echo "dry run: drop-in and launchers -> release $to"
        exit 0
    fi
    mkdir -p "$rollback_dir" "$unit_dir/dunkingsheep.service.d" "$bin_dir"
    swap="$rollback_dir/.swap.$$"
    snapshot_pair "$swap" "$dropin" "$bin_dir"
    "$target" install --bin-dir "$bin_dir" > /dev/null
    render_dropin "$SCRIPT_DIR/templates/10-rust-release.conf.in" "$to" "$target" "$dropin"
    snapshot_pair "$rollback_dir/owned" "$dropin" "$bin_dir"
    rm -rf "$rollback_dir/previous"
    mv "$swap" "$rollback_dir/previous"
    echo "drop-in and launchers now point at release $to"
    echo "not activated: no systemctl was run. Reload and restart only after the cutover gate."
    exit 0
fi

if [ "$original" -eq 1 ]; then
    source_dir="$rollback_dir/original"
    what="the pre-Rust state"
else
    source_dir="$rollback_dir/previous"
    what="the state before the last install or rollback"
fi
[ -d "$source_dir" ] || die "no saved state to restore ($what): run install-release.sh first, or pass --to ID"
guard_against_drift "$rollback_dir" "$dropin" "$bin_dir"
if [ "$dry_run" -eq 1 ]; then
    echo "dry run: restore $what from $source_dir"
    exit 0
fi

# Save the pair being replaced before restoring, because the source may be "previous".
swap="$rollback_dir/.swap.$$"
snapshot_pair "$swap" "$dropin" "$bin_dir"
restore_pair "$source_dir" "$dropin" "$bin_dir"
snapshot_pair "$rollback_dir/owned" "$dropin" "$bin_dir"
rm -rf "$rollback_dir/previous"
mv "$swap" "$rollback_dir/previous"
echo "restored $what (drop-in and launchers as one pair)"
echo "not activated: no systemctl was run. Reload and restart only after the cutover gate."
