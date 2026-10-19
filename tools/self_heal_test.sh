#!/usr/bin/env bash
# Deterministic kill/restart proof for t12.
#
# 1. Prove the daemon answers.
# 2. Kill -9 the main pid (the worst case: no clean shutdown, stale socket).
# 3. Wait for the supervisor to bring it back (systemd Restart=always, or the
#    client's own auto-start when run without systemd).
# 4. Prove a recall still works and `health` reports ok with no data loss.
#
# No model is consulted at any step. Exits non-zero on the first failure.
set -euo pipefail

BIN="${DUNKINGSHEEP_BIN:-/home/hunter/.local/bin/dunkingsheep}"
SELF_PANE="${DUNKINGSHEEP_SELF_PANE:-w9:pH}"
WAIT_S="${SELF_HEAL_WAIT_S:-15}"

fail() { echo "FAIL: $*" >&2; exit 1; }

json_pid() { "$BIN" --json status 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin)["pid"])'; }
status_ok() { "$BIN" --self-pane "$SELF_PANE" status >/dev/null 2>&1; }

echo "1. status"
status_ok || fail "daemon not answering before the test"
before="$(json_pid)"
echo "   pid=$before"
"$BIN" --self-pane "$SELF_PANE" memory stats >/dev/null || fail "memory stats failed"
chunks_before="$("$BIN" --json --self-pane "$SELF_PANE" memory stats | python3 -c 'import sys,json;print(json.load(sys.stdin)["chunks"])')"
echo "   chunks=$chunks_before"

echo "2. kill -9 $before"
kill -9 "$before" 2>/dev/null || true

echo "3. wait up to ${WAIT_S}s for restart"
deadline=$(( $(date +%s) + WAIT_S ))
after=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  sleep 0.5
  if status_ok; then
    candidate="$(json_pid 2>/dev/null || true)"
    if [ -n "$candidate" ] && [ "$candidate" != "$before" ]; then after="$candidate"; break; fi
  fi
done
[ -n "$after" ] || fail "daemon did not come back within ${WAIT_S}s"
echo "   pid=$after (new)"

echo "4. verify data and health"
chunks_after="$("$BIN" --json --self-pane "$SELF_PANE" memory stats | python3 -c 'import sys,json;print(json.load(sys.stdin)["chunks"])')"
[ "$chunks_after" = "$chunks_before" ] || fail "lost chunks: $chunks_before -> $chunks_after"
"$BIN" --self-pane "$SELF_PANE" health --repair | grep -q '^health ok' || fail "health is not ok after restart"
"$BIN" --self-pane "$SELF_PANE" memory context dunkingsheep --sessions 1 >/dev/null \
  || fail "recall failed after restart"
echo "   chunks=$chunks_after (unchanged), health ok, recall ok"
echo "PASS: kill -9 -> automatic restart -> no data loss"
