#!/usr/bin/env bash
# t1605 slice 3, CI job "production replay (send)".
#
#   tools/ci_production_replay.sh                 the job step: builds the throwaway sandbox,
#                                                 replays from its owner pane, prints the output
#   tools/ci_production_replay.sh pane DIR NAME   the owner pane's script, run inside pane %2
#
# Everything the job creates lives in one temp directory: a tmux server on a private socket name
# (TMUX_TMPDIR inside the directory), a throwaway sandbox daemon with its own state and socket, and
# an owner pane. The owner pane registers itself with `superuser recover` from its own process
# tree, then runs the replay from that tree. That is the same identity path as the t1605 sandbox.
# No token, no secret, no live socket, and nothing outside the directory.
#
# The recorded traffic names pane %2, so the owner pane is made %2: two placeholder panes come
# first, and the job checks it. The owner pane title is pinned to "cell" because the recorded
# mailbox label is host-derived: tmux defaults a pane title to the host name, and the recording
# ran on a machine named cell. This pin is temporary. Remove it once the fixture is re-recorded
# with an explicit owner pane title (the t1605 follow-up).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMMANDS="send_text,send_keys,flush_box,clear_box,delivery,delivery_capability"
PY=/usr/bin/python3
TMUX_BIN=/usr/bin/tmux
OWNER_PANE="%2"
# Host-derived label: the recording's mailbox name is this title. See the header.
PIN_TITLE="cell"

# Runs inside the owner pane. The daemon binds a request to the pane whose root process is an
# ancestor of the caller, so every process below is a child of this pane's shell.
pane_main() {
  local temp="$1" name="$2"
  local sock="$temp/ds.sock" state="$temp/state" home="$temp/home" status="$temp/replay.status"
  local daemon="" code=1 up=""

  finish() {
    local rc=$?
    if [ -n "$daemon" ]; then
      kill "$daemon" 2>/dev/null || true
      wait "$daemon" 2>/dev/null || true
    fi
    echo "$rc" > "$status"
  }
  trap finish EXIT

  if [ "${TMUX_PANE:-}" != "$OWNER_PANE" ]; then
    echo "owner pane is '${TMUX_PANE:-unset}', expected $OWNER_PANE: the recorded traffic names it" >&2
    exit 3
  fi
  mkdir -p "$home" "$state" "$temp/no-guard"

  local -a sandbox=(env -i "PATH=/usr/bin:/bin" "HOME=$home" "LANG=C.UTF-8"
    "TMUX_TMPDIR=$temp/tmux" "DUNKINGSHEEP_DIR=$state" "DUNKINGSHEEP_SOCKET=$sock"
    "DUNKINGSHEEP_TERMINAL_BACKEND=tmux" "DUNKINGSHEEP_TMUX_SOCKET=$name"
    "DUNKINGSHEEP_TMUX_BIN=$TMUX_BIN" "DUNKINGSHEEP_TMUX_REGISTRY=$state/tmux-panes.json"
    "DUNKINGSHEEP_TMUX_GUARD_DIR=$temp/no-guard" "DUNKINGSHEEP_CLI=$ROOT/dunkingsheep"
    "DUNKINGSHEEP_NO_START_DAEMON=1")

  "${sandbox[@]}" "$PY" "$ROOT/dunkingsheep" serve > "$temp/daemon.log" 2>&1 &
  daemon=$!
  for _ in $(seq 1 150); do
    if "$PY" -I -c 'import sys
sys.path.insert(0, sys.argv[1])
import diff_harness as harness
reply, _latency = harness.call(sys.argv[2], {"id": 0, "cmd": "status", "args": {}})
sys.exit(0 if reply and reply.get("ok") else 1)' "$ROOT/tools" "$sock" 2>/dev/null; then
      up=yes
      break
    fi
    if ! kill -0 "$daemon" 2>/dev/null; then
      cat "$temp/daemon.log" >&2
      echo "the sandbox daemon exited before it answered" >&2
      exit 4
    fi
    sleep 0.2
  done
  if [ -z "$up" ]; then
    cat "$temp/daemon.log" >&2
    echo "the sandbox daemon did not answer status" >&2
    exit 4
  fi

  # The owner pane is the caller, so it registers itself. This is the bootstrap: no owner exists yet.
  "${sandbox[@]}" "TMUX=${TMUX:-}" "TMUX_PANE=$TMUX_PANE" "$PY" "$ROOT/dunkingsheep" \
    superuser recover > "$temp/recover.out" 2>&1

  set +e
  "$PY" "$ROOT/tools/production_replay.py" --traffic "$ROOT/rust/fixtures/send_replay.jsonl" \
    --against "$sock" --commands "$COMMANDS" --volatile "$ROOT/rust/fixtures/volatile.json" \
    > "$temp/replay.out" 2>&1
  code=$?
  set -e
  exit "$code"
}

# The job's own tmux server, on a private socket inside TEMP_DIR. The environment is cleared, so
# no runner variable reaches the sandbox.
TEMP_DIR=""
TMUX_NAME=""
sandbox_tmux() {
  env -i "PATH=/usr/bin:/bin" "HOME=$TEMP_DIR/home" "LANG=C.UTF-8" "TMUX_TMPDIR=$TEMP_DIR/tmux" \
    "$TMUX_BIN" -L "$TMUX_NAME" "$@"
}

job_main() {
  TEMP_DIR="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dsci.XXXXXX")"
  TMUX_NAME="ds-ci-$$"
  mkdir -p "$TEMP_DIR/tmux" "$TEMP_DIR/home" "$TEMP_DIR/state" "$TEMP_DIR/no-guard"
  trap 'sandbox_tmux kill-server >/dev/null 2>&1 || true' EXIT

  # %0 and %1 are placeholders, so the owner pane is %2, the pane the recording names.
  sandbox_tmux new-session -d -s w0 -x 200 -y 50 -c "$TEMP_DIR" 'exec sleep 3600'
  sandbox_tmux new-window -d -t w0 -c "$TEMP_DIR" 'exec sleep 3600'
  local owner
  owner="$(sandbox_tmux new-window -t w0 -n owner -P -F '#{pane_id}' -c "$ROOT" \
    'bash --noprofile --norc -i')"
  if [ "$owner" != "$OWNER_PANE" ]; then
    echo "the owner pane is $owner, expected $OWNER_PANE: the recording names $OWNER_PANE" >&2
    sandbox_tmux list-panes -a -F '#{pane_id} #{session_name} #{window_name}' >&2 || true
    exit 1
  fi
  sandbox_tmux select-pane -t "$owner" -T "$PIN_TITLE"

  # The owner pane runs this same script in its pane mode, as its own child process.
  sandbox_tmux send-keys -t "$owner" -l \
    "bash '$ROOT/tools/ci_production_replay.sh' pane '$TEMP_DIR' '$TMUX_NAME'"
  sandbox_tmux send-keys -t "$owner" Enter

  local code
  for _ in $(seq 1 900); do
    [ -s "$TEMP_DIR/replay.status" ] && break
    sleep 1
  done
  if [ -s "$TEMP_DIR/replay.status" ]; then
    code="$(cat "$TEMP_DIR/replay.status")"
  else
    echo "the owner pane did not finish in 900 s; its screen:" >&2
    sandbox_tmux capture-pane -p -t "$owner" | tail -n 60 >&2 || true
    code=1
  fi

  local name
  for name in recover.out replay.out daemon.log; do
    echo "== $name =="
    cat "$TEMP_DIR/$name" 2>/dev/null || echo "(no $name)"
  done
  echo "replay exit=$code"
  exit "$code"
}

if [ "${1:-}" = "pane" ]; then
  pane_main "${2:?the temp directory}" "${3:?the tmux socket name}"
fi
job_main
