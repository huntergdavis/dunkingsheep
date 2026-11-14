#!/usr/bin/env bash
# Dry-run unless --apply; run only after exact-head review and idle handoff.
set -euo pipefail
umask 077
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
apply=0
expected=''
backup=''
dest=$HOME/.local/bin
while (($#)); do
    case $1 in
        --apply) apply=1; shift ;;
        --expected-ds-sha) expected=${2:?missing SHA}; shift 2 ;;
        --backup-dir) backup=${2:?missing directory}; shift 2 ;;
        --bin-dir) dest=${2:?missing directory}; shift 2 ;;
        *) echo 'usage: install-cargo-wrappers.sh [--apply --expected-ds-sha SHA --backup-dir ABSENT_DIR]' >&2; exit 64 ;;
    esac
done
[[ $dest == /* && -d $dest ]] || { echo 'bin directory must exist and be absolute' >&2; exit 64; }
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
idle() {
    local running locks
    running=$(ps -u "$(id -u)" -o pid=,comm= | awk '$2 ~ /^(cargo|rustc|rustdoc)$/ {print}')
    locks=$(lslocks --noheadings --raw --output PID,PATH | awk '$2 ~ /\/compile.lock$/ || $2 ~ /\/compile-slots\/slot-[0-9]+$/ {print}')
    if [[ -n $running || -n $locks ]]; then
        printf 'Cargo wrapper install REFUSED: build/lock active\n%s\n%s\n' "$running" "$locks" >&2
        return 75
    fi
}
bash -n "$repo/tools/ds-cargo" "$repo/tools/proj-cargo"
if ((apply == 0)); then
    echo "DRY-RUN: backup installed ds-cargo/proj-cargo; atomically replace in $dest after independent review and idle handoff"
    sha256sum "$repo/tools/ds-cargo" "$repo/tools/proj-cargo"
    idle
    exit
fi
[[ $expected =~ ^[0-9a-f]{64}$ && $backup == /* && ! -e $backup ]] || { echo 'apply requires installed DS hash and a new absolute backup directory' >&2; exit 64; }
[[ -f $dest/ds-cargo && ! -L $dest/ds-cargo && $(sha256sum "$dest/ds-cargo" | cut -d' ' -f1) == "$expected" ]] || { echo 'installed ds-cargo differs from reviewed baseline' >&2; exit 1; }
mkdir -p "$runtime/compile-slots"
exec 7> "$runtime/compile-slots/maintenance.lock"
flock -n 7 || { echo 'wrapper launches/builds still active; refusing installation' >&2; exit 75; }
idle
mkdir -m 700 -- "$backup"
for name in ds-cargo proj-cargo; do
    if [[ -e $dest/$name || -L $dest/$name ]]; then cp -a -- "$dest/$name" "$backup/$name"; fi
done
(cd "$backup" && sha256sum ds-cargo > SHA256SUMS; if [[ -f proj-cargo ]]; then sha256sum proj-cargo >> SHA256SUMS; fi)
temporary=''
trap '[[ -z $temporary ]] || rm -f -- "$temporary"' EXIT
for name in proj-cargo ds-cargo; do
    idle
    temporary=$(mktemp "$dest/.$name-XXXXXXXX")
    cp -- "$repo/tools/$name" "$temporary"
    chmod 700 "$temporary"
    mv -fT -- "$temporary" "$dest/$name"
    temporary=''
done
echo "Installed reviewed wrappers; backups at $backup"
sha256sum "$dest/ds-cargo" "$dest/proj-cargo"
