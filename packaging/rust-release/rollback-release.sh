#!/bin/sh
# Rolls the Rust CLI back (t3229, audit P0-1). Never runs systemctl.
#
#   no --to ID:  restore the drop-in saved by the last install-release.sh run. If that run
#                found no drop-in, remove it, so ExecStart returns to the base unit. Restore
#                the launchers saved as *.pre-rust, if present.
#   --to ID:     point the drop-in and the launchers at release ID, which must already be
#                installed under <prefix>/releases/ID. The drop-in state being replaced is
#                kept as *.prev, so a second rollback can undo this one.
#
# usage: rollback-release.sh --prefix DIR --bin-dir DIR --unit-dir DIR [--to ID] [--dry-run]

set -eu
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/lib.sh"

prefix=""
bin_dir=""
unit_dir=""
to=""
dry_run=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --prefix) prefix=${2:-}; shift 2 ;;
        --bin-dir) bin_dir=${2:-}; shift 2 ;;
        --unit-dir) unit_dir=${2:-}; shift 2 ;;
        --to) to=${2:-}; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ -n "$prefix" ] || die "--prefix is required"
[ -n "$bin_dir" ] || die "--bin-dir is required"
[ -n "$unit_dir" ] || die "--unit-dir is required"
require_plain_path --prefix "$prefix"
require_plain_path --bin-dir "$bin_dir"
require_plain_path --unit-dir "$unit_dir"

dropin_dir="$unit_dir/dunkingsheep.service.d"
dropin="$dropin_dir/10-rust-release.conf"

if [ -n "$to" ]; then
    require_release_id "$to"
    target="$prefix/releases/$to/dunkingsheep"
    [ -x "$target" ] || die "release $to is not installed: $target"
    if [ "$dry_run" -eq 1 ]; then
        echo "dry run: drop-in and launchers -> release $to"
        exit 0
    fi
    "$target" install --bin-dir "$bin_dir" > /dev/null
    if [ -f "$dropin" ]; then
        cp "$dropin" "$dropin.prev"
    else
        printf '# none\n' > "$dropin.prev"
    fi
    render_dropin "$SCRIPT_DIR/templates/10-rust-release.conf.in" "$to" "$target" "$dropin"
    echo "drop-in and launchers now point at release $to"
    echo "not activated: no systemctl was run. Reload and restart only after the cutover gate."
    exit 0
fi

[ -f "$dropin.prev" ] || die "no saved drop-in to restore: run install-release.sh first, or pass --to ID"
if [ "$dry_run" -eq 1 ]; then
    echo "dry run: restore the drop-in from $dropin.prev and the launchers from *.pre-rust"
    exit 0
fi
if [ "$(head -n 1 "$dropin.prev")" = "# none" ]; then
    rm -f "$dropin"
    echo "removed the Rust drop-in; ExecStart returns to the base unit"
else
    mv "$dropin.prev" "$dropin"
    echo "restored the previous drop-in"
fi
rm -f "$dropin.prev"
for name in dunkingsheep dunk-poll; do
    if [ -f "$bin_dir/$name.pre-rust" ]; then
        mv "$bin_dir/$name.pre-rust" "$bin_dir/$name"
        echo "restored launcher $bin_dir/$name"
    fi
done
echo "not activated: no systemctl was run. Reload and restart only after the cutover gate."
