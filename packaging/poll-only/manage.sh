#!/bin/sh
# Install or restore only dunk-poll; never changes the daemon, CLI or systemd.
set -eu
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
. "$SCRIPT_DIR/../rust-release/lib.sh"
usage() { echo 'usage: manage.sh install|rollback --bin-dir DIR --state-dir DIR --paired-rollback DIR [--binary FILE --expected-sha256 HASH]' >&2; exit 2; }
[ "$#" -gt 0 ] || usage
action=$1; shift
bin_dir= state_dir= paired= binary= expected=
while [ "$#" -gt 0 ]; do
    [ "$#" -ge 2 ] || usage
    case "$1" in
        --bin-dir) bin_dir=$2 ;; --state-dir) state_dir=$2 ;;
        --paired-rollback) paired=$2 ;; --binary) binary=$2 ;; --expected-sha256) expected=$2 ;; *) usage ;;
    esac
    shift 2
done
case "$bin_dir:$state_dir:$paired" in /*:/*:/*) ;; *) usage ;; esac
case "$bin_dir$state_dir$paired" in *[!A-Za-z0-9._/-]*) echo 'unsupported path' >&2; exit 1 ;; esac
# Directories must already exist, be real paths, and be writable only by their owner.
for dir in "$bin_dir" "$state_dir"; do
    [ -d "$dir" ] && [ "$(realpath -e "$dir")" = "$dir" ] || { echo "directory must be an existing canonical path: $dir" >&2; exit 1; }
    [ "$(stat -c %u "$dir")" = "$(id -u)" ] || { echo "directory not owned by caller: $dir" >&2; exit 1; }
    mode=$(stat -c %a "$dir")
    [ $((0$mode & 0002)) -eq 0 ] || { echo "directory is world writable: $dir" >&2; exit 1; }
    if [ $((0$mode & 0020)) -ne 0 ]; then
        [ "$(stat -c %g "$dir")" = "$(id -u)" ] && [ -z "$(getent group "$(stat -c %g "$dir")" | cut -d: -f4)" ] || { echo "directory group is not private: $dir" >&2; exit 1; }
    fi
done
lock_release_bin "$bin_dir"
[ ! -e "$paired/owned" ] && [ ! -e "$paired/original" ] || { echo 'paired release owns launchers; poll-only operation refused' >&2; exit 1; }
for path in "$state_dir/lock" "$state_dir/previous" "$state_dir/installed"; do
    [ ! -L "$path" ] || { echo "refusing symlink: $path" >&2; exit 1; }
done
exec 9>"$state_dir/lock"
flock -n 9 || { echo 'poll operation already running' >&2; exit 1; }
launcher=$bin_dir/dunk-poll
[ ! -L "$launcher" ] || { echo 'refusing symlink launcher' >&2; exit 1; }
case "$action" in
install)
    [ -f "$launcher" ] && [ -x "$launcher" ] && [ -f "$binary" ] && [ -x "$binary" ] || { echo 'executable launcher and binary required' >&2; exit 1; }
    [ ! -e "$poll_marker" ] || { echo "poll-only marker exists" >&2; exit 1; }
    [ ! -e "$state_dir/previous" ] || { echo 'existing poll snapshot: rollback first' >&2; exit 1; }
    actual=$(sha256sum "$binary" | cut -d ' ' -f1)
    [ "$actual" = "$expected" ] || { echo 'binary hash mismatch' >&2; exit 1; }
    # Immutable by content; refuse any unexpected existing object.
    artifact=$state_dir/poll-$actual
    if [ -e "$artifact" ]; then
        [ -f "$artifact" ] && [ ! -L "$artifact" ] && [ "$(sha256sum "$artifact" | cut -d ' ' -f1)" = "$actual" ] || { echo 'artifact conflict' >&2; exit 1; }
    else
        tmp=$(mktemp "$state_dir/.artifact.XXXXXXXX"); cp "$binary" "$tmp"; chmod 0555 "$tmp"; mv "$tmp" "$artifact"
    fi
    printf "%s\n" "$state_dir" > "$poll_marker"
    tmp=$(mktemp "$state_dir/.previous.XXXXXXXX")
    cp -p "$launcher" "$tmp"
    mv "$tmp" "$state_dir/previous"
    tmp=$(mktemp "$state_dir/.installed.XXXXXXXX")
    printf '#!/bin/sh\nexec %s "$@"\n' "$artifact" > "$tmp"
    chmod 0755 "$tmp"
    mv "$tmp" "$state_dir/installed"
    tmp=$(mktemp "$bin_dir/.dunk-poll.XXXXXXXX")
    cp -p "$state_dir/installed" "$tmp"
    mv -f "$tmp" "$launcher"
    echo "installed poll $actual; daemon and CLI unchanged"
    ;;
rollback)
    if [ ! -f "$state_dir/previous" ]; then
        [ -f "$poll_marker" ] && [ "$(cat "$poll_marker")" = "$state_dir" ] || { echo "no poll snapshot" >&2; exit 1; }
        rm "$poll_marker"; echo "cleared interrupted pre-snapshot operation"; exit 0
    fi
    [ -f "$poll_marker" ] && [ "$(cat "$poll_marker")" = "$state_dir" ] || { echo "poll marker mismatch" >&2; exit 1; }
    if cmp -s "$launcher" "$state_dir/previous"; then
        rm -f "$state_dir/previous" "$state_dir/installed" "$poll_marker"
        echo 'previous poll already restored; snapshot cleared'
        exit 0
    fi
    [ -f "$state_dir/installed" ] && cmp -s "$launcher" "$state_dir/installed" || { echo 'launcher drift; refusing rollback' >&2; exit 1; }
    tmp=$(mktemp "$bin_dir/.dunk-poll.XXXXXXXX")
    cp -p "$state_dir/previous" "$tmp"
    mv -f "$tmp" "$launcher"
    rm "$state_dir/previous" "$state_dir/installed" "$poll_marker"
    echo 'restored previous poll launcher; daemon and CLI unchanged'
    ;;
*) usage ;;
esac
