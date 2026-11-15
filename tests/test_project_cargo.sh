#!/usr/bin/env bash
# Real flock/timeout integration, fake Cargo; no compiler or production lock.
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
base=${TMPDIR:-$repo/.test-artifacts}
mkdir -p -- "$base"
fixture=$(mktemp -d "$base/project-cargo-XXXXXXXX")
pids=()
cleanup() { for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done; wait 2>/dev/null || true; }
trap cleanup EXIT
mkdir -p "$fixture/bin" "$fixture/runtime" "$fixture/config" "$fixture/release"
export XDG_RUNTIME_DIR=$fixture/runtime XDG_CONFIG_HOME=$fixture/config
export TRACE=$fixture/trace RELEASE=$fixture/release
export PATH="$fixture/bin:$PATH" PROJ_CARGO_WAIT_SECONDS=1
cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -eu
echo "start $1" >> "$TRACE"
trap 'echo "end $1" >> "$TRACE"' EXIT
while [[ ! -f $RELEASE/$1 ]]; do sleep 0.05; done
exit "${FAKE_EXIT:-0}"
SH
chmod +x "$fixture/bin/cargo"
wrapper=$repo/tools/proj-cargo
wait_start() {
    for ((i=0; i<100; i++)); do
        if [[ -f $TRACE ]] && grep -Eq "^start $1$" "$TRACE"; then return; fi
        sleep 0.05
    done
    echo "missing start $1" >&2; exit 1
}
expect_exit() {
    local expected=$1 actual=0; shift
    "$@" || actual=$?
    [[ $actual == "$expected" ]] || { echo "expected exit $expected, got $actual" >&2; exit 1; }
}
"$wrapper" alpha first 2> "$fixture/first.log" & a=$!; pids+=("$a")
"$wrapper" beta second 2> "$fixture/second.log" & b=$!; pids+=("$b")
wait_start first; wait_start second
expect_exit 75 "$wrapper" gamma third 2> "$fixture/host-busy.log"
if grep -Eq '^start third$' "$TRACE"; then exit 1; fi
grep -Eq 'holder pid=[0-9]+.*project=(alpha|beta)' "$fixture/host-busy.log"
expect_exit 75 "$wrapper" alpha duplicate 2> "$fixture/project-busy.log"
if grep -Eq '^start duplicate$' "$TRACE"; then exit 1; fi
grep -Eq 'holder pid=[0-9]+.*cwd=.*project=alpha' "$fixture/project-busy.log"
# Cargo reads remain available even with all compile slots held.
touch "$RELEASE/metadata"
"$wrapper" gamma metadata 2> "$fixture/read.log"
"$wrapper" gamma queued 2> "$fixture/queued.log" & c=$!; pids+=("$c")
touch "$RELEASE/first"; wait "$a"
wait_start queued
touch "$RELEASE/second" "$RELEASE/queued"; wait "$b"; wait "$c"
# Metadata is an intentionally unlocked read, excluded from compile counts.
awk '$2 != "metadata" {if ($1=="start") n++; else n--; if(n>max)max=n; if(n<0)exit 1} END {if(n!=0 || max!=2)exit 1}' "$TRACE"
touch "$RELEASE/failure"
expect_exit 42 env FAKE_EXIT=42 "$wrapper" delta failure 2> "$fixture/failure.log"
expect_exit 124 env PROJ_CARGO_TIMEOUT_SECONDS=1 "$wrapper" delta timedout 2> "$fixture/timeout.log"
touch "$RELEASE/after-timeout"
"$wrapper" delta after-timeout 2> "$fixture/after-timeout.log"
# A direct legacy flock has no holder record: diagnostics still name its PID/cwd.
mkdir -p "$XDG_RUNTIME_DIR/foreign"
flock "$XDG_RUNTIME_DIR/foreign/compile.lock" sleep 2 & foreign=$!; pids+=("$foreign")
for ((i=0; i<100; i++)); do
    [[ -e $XDG_RUNTIME_DIR/foreign/compile.lock ]] && ! flock -n "$XDG_RUNTIME_DIR/foreign/compile.lock" true && break
    sleep 0.01
done
expect_exit 75 "$wrapper" foreign blocked 2> "$fixture/foreign.log"
grep -Eq "holder pid=$foreign .*project=unmanaged" "$fixture/foreign.log"
wait "$foreign"
# Owner config rejects invalid limits, and lowering is only allowed while idle.
mkdir -p "$XDG_CONFIG_HOME/compile-slots"
echo invalid > "$XDG_CONFIG_HOME/compile-slots/limit"
expect_exit 64 "$wrapper" alpha config-error 2> "$fixture/config.log"
echo 1 > "$XDG_CONFIG_HOME/compile-slots/limit"
touch "$RELEASE/config-one"
"$wrapper" alpha config-one 2> "$fixture/config-one.log"
grep -Eq 'host-slot=1/1' "$fixture/config-one.log"
expect_exit 64 "$wrapper" ../escape build 2> "$fixture/slug.log"
# Exercise install mechanics only in a private prefix. Stub process/lock listings
# so unrelated real builds are neither inspected as test failures nor modified.
mkdir -p "$fixture/install/bin" "$fixture/install/helpers"
printf 'old wrapper\n' > "$fixture/install/bin/ds-cargo"
chmod 700 "$fixture/install/bin/ds-cargo"
baseline=$(sha256sum "$fixture/install/bin/ds-cargo" | cut -d' ' -f1)
cat > "$fixture/install/helpers/ps" <<'SH'
#!/usr/bin/env bash
[[ ${FAKE_BUSY:-0} == 0 ]] || echo '999999 cargo'
exit 0
SH
printf '#!/bin/sh\nexit 0\n' > "$fixture/install/helpers/lslocks"
chmod +x "$fixture/install/helpers/ps" "$fixture/install/helpers/lslocks"
installer=$repo/tools/install-cargo-wrappers.sh
install_path=$fixture/install/helpers:$PATH
env PATH="$install_path" bash "$installer" --bin-dir "$fixture/install/bin" > "$fixture/install-dry.log"
[[ ! -e $fixture/install/bin/proj-cargo ]]
expect_exit 75 env PATH="$install_path" FAKE_BUSY=1 bash "$installer" --apply --bin-dir "$fixture/install/bin" --expected-ds-sha "$baseline" --backup-dir "$fixture/install/backup" 2> "$fixture/install-busy.log"
[[ ! -e $fixture/install/backup ]]
env PATH="$install_path" bash "$installer" --apply --bin-dir "$fixture/install/bin" --expected-ds-sha "$baseline" --backup-dir "$fixture/install/backup" > "$fixture/install.log"
[[ $(sha256sum "$fixture/install/backup/ds-cargo" | cut -d' ' -f1) == "$baseline" ]]
cmp "$repo/tools/ds-cargo" "$fixture/install/bin/ds-cargo"
cmp "$repo/tools/proj-cargo" "$fixture/install/bin/proj-cargo"
expect_exit 1 env PATH="$install_path" bash "$installer" --apply --bin-dir "$fixture/install/bin" --expected-ds-sha "$baseline" --backup-dir "$fixture/install/drift-backup" 2> "$fixture/install-drift.log"
new_baseline=$(sha256sum "$fixture/install/bin/ds-cargo" | cut -d' ' -f1)
"$fixture/install/bin/proj-cargo" epsilon maintenance 2> "$fixture/maintenance.log" & maintenance=$!; pids+=("$maintenance")
wait_start maintenance
expect_exit 75 env PATH="$install_path" bash "$installer" --apply --bin-dir "$fixture/install/bin" --expected-ds-sha "$new_baseline" --backup-dir "$fixture/install/maintenance-backup" 2> "$fixture/install-maintenance.log"
[[ ! -e $fixture/install/maintenance-backup ]]
touch "$RELEASE/maintenance"; wait "$maintenance"
# t3912: ds-cargo gives each checkout a short scratch TMPDIR under ~/workspace/.dunk-scratch/c;
# a fake toolchain and a fake proj-cargo beside a copy of ds-cargo report what it exports.
mkdir -p "$fixture/dscargo/bin" "$fixture/dscargo/toolchain" "$fixture/dscargo/repo/rust" "$fixture/h"
cp "$repo/tools/ds-cargo" "$fixture/dscargo/bin/ds-cargo"
printf '#!/usr/bin/env bash\necho "TMPDIR=${TMPDIR:-unset}"\n' > "$fixture/dscargo/bin/proj-cargo"
printf '#!/bin/sh\nexit 0\n' > "$fixture/dscargo/toolchain/cargo"
cp "$fixture/dscargo/toolchain/cargo" "$fixture/dscargo/toolchain/rustc"
chmod +x "$fixture/dscargo/bin/ds-cargo" "$fixture/dscargo/bin/proj-cargo" "$fixture/dscargo/toolchain/cargo" "$fixture/dscargo/toolchain/rustc"
git init -q "$fixture/dscargo/repo"
# A short HOME stands in for the real one, so the length rule is tested on a short path.
short_home=$(mktemp -d /tmp/dsch.XXXX)
ds() { (cd "$1" && env -u CARGO_TARGET_DIR DS_CARGO_TOOLCHAIN="$fixture/dscargo/toolchain" HOME="$short_home" "${@:2}" "$fixture/dscargo/bin/ds-cargo" test); }
top=$(cd "$fixture/dscargo/repo" && git rev-parse --show-toplevel)
key=$(printf '%s' "$top" | sha256sum | cut -c1-8)
got=$(ds "$fixture/dscargo/repo/rust" TMPDIR=/tmp)
[[ $got == "TMPDIR=$short_home/workspace/.dunk-scratch/c/$key" ]] || { echo "default TMPDIR: $got" >&2; exit 1; }
[[ $(stat -c %a "$short_home/workspace/.dunk-scratch/c/$key") == 700 ]] || { echo "scratch is not 0700" >&2; exit 1; }
[[ $(stat -c %a "$short_home/workspace/.dunk-scratch") == 700 ]] || { echo "scratch parent is not 0700" >&2; exit 1; }
# The same checkout always gets the same directory, from any subdirectory.
[[ $(ds "$fixture/dscargo/repo" TMPDIR=/tmp) == "$got" ]] || { echo "not stable per checkout" >&2; exit 1; }
got=$(ds "$fixture/dscargo/repo/rust" TMPDIR="$fixture/dscargo/mine")
[[ $got == "TMPDIR=$fixture/dscargo/mine" ]] || { echo "an explicit TMPDIR was replaced: $got" >&2; exit 1; }
# The fixture may itself sit inside a checkout: stop git looking above it.
got=$(ds "$fixture/dscargo" TMPDIR=/tmp GIT_CEILING_DIRECTORIES="$fixture")
[[ $got == "TMPDIR=/tmp" ]] || { echo "outside a checkout: $got" >&2; exit 1; }
# Too long for socket paths: a clear refusal, not a silent long TMPDIR.
expect_exit 2 ds "$fixture/dscargo/repo/rust" TMPDIR=/tmp DS_CARGO_TMP_MAX=20 2> "$fixture/dscargo/too-long.log"
grep -q "Unix socket paths under it would not fit" "$fixture/dscargo/too-long.log"
rm -r -- "$short_home"
echo "project Cargo locking PASS; evidence: $fixture"
