#!/bin/sh
# Removes the Rust release drop-in and launchers (t3229, audit P0-1). Never runs systemctl.
# Launchers that are not ours are left alone. Release directories are kept as immutable
# history unless --purge is given.
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
        -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
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
if [ "$dry_run" -eq 1 ]; then
    echo "dry run: remove $dropin and $dropin.prev; restore or remove our launchers in $bin_dir"
    [ "$purge" -eq 1 ] && echo "dry run: purge $prefix/releases"
    exit 0
fi

rm -f "$dropin" "$dropin.prev"
for name in dunkingsheep dunk-poll; do
    launcher="$bin_dir/$name"
    if [ -f "$bin_dir/$name.pre-rust" ]; then
        mv "$bin_dir/$name.pre-rust" "$launcher"
    elif [ -f "$launcher" ] && grep -q -e "Dunking Sheep Rust CLI launcher" -e "Dunking Sheep poll wrapper" "$launcher"; then
        rm -f "$launcher"
    fi
done
if [ "$purge" -eq 1 ]; then
    rm -rf "$prefix/releases"
fi
echo "removed the Rust release drop-in and our launchers"
echo "not activated: no systemctl was run."
