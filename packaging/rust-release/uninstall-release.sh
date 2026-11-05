#!/bin/sh
# Removes the Rust release from the unit and the launchers (t3229, audit P0-1). It restores
# the pre-Rust pair saved by the first install-release.sh run, so the drop-in and both
# launchers return together. It refuses when no such pair was saved. Release directories
# and the rollback history are kept unless --purge is given. Never runs systemctl.
#
# usage: uninstall-release.sh --prefix DIR --bin-dir DIR --unit-dir DIR [--purge] [--dry-run]

set -eu
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/lib.sh"

prefix=""
bin_dir=""
unit_dir=""
purge=0
dry_run=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix) prefix=${2:-}; shift 2 ;;
        --bin-dir) bin_dir=${2:-}; shift 2 ;;
        --unit-dir) unit_dir=${2:-}; shift 2 ;;
        --purge) purge=1; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$prefix" ] || die "--prefix is required"
[ -n "$bin_dir" ] || die "--bin-dir is required"
[ -n "$unit_dir" ] || die "--unit-dir is required"
require_plain_path --prefix "$prefix"
require_plain_path --bin-dir "$bin_dir"
require_plain_path --unit-dir "$unit_dir"

dropin="$unit_dir/dunkingsheep.service.d/10-rust-release.conf"
original="$prefix/rollback/original"
[ -d "$original" ] || die "no pre-Rust state saved at $original: nothing to restore"
if [ "$dry_run" -eq 1 ]; then
    echo "dry run: restore the pre-Rust drop-in and launchers from $original"
    [ "$purge" -eq 1 ] && echo "dry run: purge $prefix/releases and $prefix/rollback"
    exit 0
fi

# A drop-in without our release_id line was not written by install-release.sh. Leave it.
if [ -f "$dropin" ] && [ -z "$(dropin_release_id "$dropin")" ]; then
    die "refusing to change $dropin: it was not written by install-release.sh"
fi
guard_against_drift "$prefix/rollback" "$dropin" "$bin_dir"
restore_pair "$original" "$dropin" "$bin_dir"
snapshot_pair "$prefix/rollback/owned" "$dropin" "$bin_dir"
if [ "$purge" -eq 1 ]; then
    rm -rf "$prefix/releases" "$prefix/rollback"
fi
echo "restored the pre-Rust drop-in and launchers"
echo "not activated: no systemctl was run."
