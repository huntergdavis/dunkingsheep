#!/bin/sh
# Only a private helper stub is executed; no host services or Cargo.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-$ROOT}/release-route.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
mkdir "$work/packet"
cat > "$work/helper" <<'STUB'
#!/bin/sh
printf '%s\n' "$@" > "$RELEASE_TEST_ARGS"
STUB
chmod 700 "$work/helper"
export RUST_RELEASE_OPERATOR="$work/helper" RELEASE_TEST_ARGS="$work/args"
sha=1111111111111111111111111111111111111111111111111111111111111111
for action in plan activate verify rollback; do
  sh "$ROOT/tools/rust_rollout.sh" "$action" --packet "$work/packet" --packet-sha "$sha"
  printf '%s\n' "$action" --packet "$work/packet" --packet-sha "$sha" > "$work/want"
  cmp "$work/want" "$work/args"
done
sh "$ROOT/tools/rust_rollout.sh" plan --packet "$work/packet" --packet-sha "$sha" --apply
if grep -q '^--apply$' "$work/args"; then exit 1; fi
sh "$ROOT/tools/rust_rollout.sh" rollback --packet "$work/packet" --packet-sha "$sha" --apply
grep -q '^--apply$' "$work/args"
for flag in --sha --unit --artifact --artifact-sha256; do
  rm -f "$work/args"
  if sh "$ROOT/tools/rust_rollout.sh" activate --packet "$work/packet" --packet-sha "$sha" "$flag" daemon > "$work/output" 2>&1; then exit 1; fi
  test ! -e "$work/args"
done
if sh "$ROOT/tools/rust_rollout.sh" activate --packet "$work/packet" > "$work/output" 2>&1; then exit 1; fi
export RUST_RELEASE_OPERATOR="$work/missing"
if sh "$ROOT/tools/rust_rollout.sh" activate --packet "$work/packet" --packet-sha "$sha" --apply > "$work/output" 2>&1; then exit 1; fi
test ! -e "$work/args"
echo 'packet routing PASS (private helper only)'
