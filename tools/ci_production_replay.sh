#!/usr/bin/env bash
# t1605 slice 3, CI job "production replay (send, task follow)".
#
#   tools/ci_production_replay.sh                 the job step: builds the throwaway sandbox,
#                                                 replays from its owner pane, prints the output
#   tools/ci_production_replay.sh pane DIR NAME [BIN]   the owner pane's script, run inside pane %2
#
# t1605 slice 4 adds a second replay on the same sandbox daemon, from the same owner pane: the
# recorded task-follow traffic (task_add seeds a task, then task_follow, task_followers,
# task_unfollow, task_followers). Each replay keeps its own output and exit code, and the job
# fails when either fails.
#
# Everything the job creates lives in one temp directory: a tmux server on a private socket name
# (TMUX_TMPDIR inside the directory), a throwaway sandbox daemon with its own state and socket, and
# an owner pane. The owner pane registers itself with `superuser recover` from its own process
# tree, then runs the replay from that tree. That is the same identity path as the t1605 sandbox.
# No token, no secret, no live socket, and nothing outside the directory.
#
# DS_BIN (optional) names a built dunkingsheep binary. When it is set, the sandbox daemon and the
# owner's `superuser recover` run that binary instead of the Python launcher. The replay is the same
# either way. The Rust job sets DS_BIN; the Python job leaves it unset.
#
# The recorded traffic names pane %2, so the owner pane is made %2: two placeholder panes come
# first, and the job checks it. The recorded mailbox label is "cat". A pane's label comes from
# its title, or from its window name when the title is empty, so the owner pane title is pinned
# to "cat" to match the recording. The pin is temporary: remove it once the fixture is
# re-recorded with an explicit owner pane title (the t1605 follow-up, after cutover).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMMANDS="send_text,send_keys,flush_box,clear_box,delivery,delivery_capability"
TASK_FOLLOW_COMMANDS="task_add,task_follow,task_followers,task_unfollow"
PY=/usr/bin/python3
TMUX_BIN=/usr/bin/tmux
OWNER_PANE="%2"
# The recorded mailbox label. See the header.
PIN_TITLE="cat"

# Runs inside the owner pane. The daemon binds a request to the pane whose root process is an
# ancestor of the caller, so every process below is a child of this pane's shell.
pane_main() {
  local temp="$1" name="$2" bin="${3:-}"
  local sock="$temp/ds.sock" state="$temp/state" home="$temp/home" status="$temp/replay.status"
  local daemon="" code=1 up=""
  local task_follow_code=1
  # The CLI the sandbox runs: the built binary when DS_BIN names one, else the Python launcher.
  local launcher="$ROOT/dunkingsheep"
  local -a cli=("$PY" "$launcher")
  if [ -n "$bin" ]; then
    launcher="$bin"
    cli=("$bin")
  fi

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
    "DUNKINGSHEEP_TMUX_GUARD_DIR=$temp/no-guard" "DUNKINGSHEEP_CLI=$launcher"
    "DUNKINGSHEEP_NO_START_DAEMON=1")

  "${sandbox[@]}" "$PY" -I -c 'import sys; sys.path.insert(0, sys.argv[1]); import dunk_terminal as t; c, o, e = t.get_backend()._run(["list-panes", "-a", "-F", t.TmuxBackend.PANE_FORMAT]); print("list-panes exit", c, "stdout", repr(o), "stderr", repr(e))' "$ROOT" || true
  "${sandbox[@]}" "${cli[@]}" serve > "$temp/daemon.log" 2>&1 &
  daemon=$!
  # 600 ticks of 0.2 s, 120 s in all. A cold start of the Rust daemon on a loaded runner can take
  # longer than 30 s. A daemon that is still starting is not stopped by the finish trap until this
  # budget has run out. The owner pane's 200 s wait below still bounds the whole run.
  local start=$SECONDS
  for _ in $(seq 1 600); do
    if "$PY" -I -c 'import sys
sys.path.insert(0, sys.argv[1])
import diff_harness as harness
reply, _latency = harness.call(sys.argv[2], {"id": 0, "cmd": "status", "args": {}})
sys.exit(0 if reply and reply.get("ok") else 1)' "$ROOT/tools" "$sock" 2>/dev/null; then
      up=yes
      # The job prints daemon.log, so the measured start time lands in the job log on every run.
      echo "sandbox daemon answered status after $((SECONDS - start)) s" >> "$temp/daemon.log"
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
  "${sandbox[@]}" "TMUX=${TMUX:-}" "TMUX_PANE=$TMUX_PANE" "${cli[@]}" \
    superuser recover > "$temp/recover.out" 2>&1

  set +e
  "$PY" "$ROOT/tools/production_replay.py" --traffic "$ROOT/rust/fixtures/send_replay.jsonl" \
    --against "$sock" --commands "$COMMANDS" --volatile "$ROOT/rust/fixtures/volatile.json" \
    > "$temp/replay.out" 2>&1
  code=$?
  # t1605 slice 4: the task-follow replay, on the same daemon from the same owner pane, after the
  # send replay. Its exit code is written apart from the send replay's so the job output names
  # which replay failed; the pane exits non-zero when either did.
  "$PY" "$ROOT/tools/production_replay.py" --traffic "$ROOT/rust/fixtures/task_follow_replay.jsonl" \
    --against "$sock" --commands "$TASK_FOLLOW_COMMANDS" --volatile "$ROOT/rust/fixtures/volatile.json" \
    > "$temp/replay-task-follow.out" 2>&1
  task_follow_code=$?
  set -e
  echo "$code" > "$temp/replay-send.status"
  echo "$task_follow_code" > "$temp/replay-task-follow.status"
  if [ "$code" -eq 0 ]; then
    code="$task_follow_code"
  fi
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
  if [ -n "${DS_BIN:-}" ] && [ ! -x "$DS_BIN" ]; then
    echo "DS_BIN is not an executable file: $DS_BIN" >&2
    exit 1
  fi
  TEMP_DIR="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/dsci.XXXXXX")"
  TMUX_NAME="ds-ci-$$"
  mkdir -p "$TEMP_DIR/tmux" "$TEMP_DIR/home" "$TEMP_DIR/state" "$TEMP_DIR/no-guard"
  trap 'sandbox_tmux kill-server >/dev/null 2>&1 || true' EXIT

  # %0 and %1 are placeholders, so the owner pane is %2, the pane the recording names.
  sandbox_tmux new-session -d -s w0 -x 200 -y 50 -c "$TEMP_DIR" 'exec sleep 3600'
  # The owner window keeps the name bash, as recorded: no automatic renaming, so tmux cannot rename
  # it to whatever command is running later.
  sandbox_tmux set-option -g automatic-rename off
  sandbox_tmux new-window -d -t w0 -c "$TEMP_DIR" 'exec sleep 3600'
  local owner
  owner="$(sandbox_tmux new-window -t w0 -n bash -P -F '#{pane_id}' -c "$ROOT" \
    'bash --noprofile --norc -i')"
  if [ "$owner" != "$OWNER_PANE" ]; then
    echo "the owner pane is $owner, expected $OWNER_PANE: the recording names $OWNER_PANE" >&2
    sandbox_tmux list-panes -a -F '#{pane_id} #{session_name} #{window_name}' >&2 || true
    exit 1
  fi
  sandbox_tmux select-pane -t "$owner" -T "$PIN_TITLE"

  # The owner pane runs this same script in its pane mode, as its own child process.
  sandbox_tmux send-keys -t "$owner" -l \
    "bash '$ROOT/tools/ci_production_replay.sh' pane '$TEMP_DIR' '$TMUX_NAME' '${DS_BIN:-}'"
  sandbox_tmux send-keys -t "$owner" Enter

  local code
  # 200 one-second ticks, well under the job's five-minute timeout, so the screen dump below prints
  # before GitHub cancels the job.
  for _ in $(seq 1 200); do
    [ -s "$TEMP_DIR/replay.status" ] && break
    sleep 1
  done
  if [ -s "$TEMP_DIR/replay.status" ]; then
    code="$(cat "$TEMP_DIR/replay.status")"
  else
    echo "the owner pane did not finish in 200 s; its screen:" >&2
    sandbox_tmux capture-pane -p -t "$owner" | tail -n 60 >&2 || true
    code=1
  fi

  local name
  for name in recover.out replay.out replay-task-follow.out daemon.log; do
    echo "== $name =="
    cat "$TEMP_DIR/$name" 2>/dev/null || echo "(no $name)"
  done
  echo "== the daemon log (state/daemon.log) =="
  cat "$TEMP_DIR/state/daemon.log" 2>/dev/null || echo "(no daemon log)"
  echo "== the owner pane screen =="
  sandbox_tmux capture-pane -p -t "$owner" 2>&1 | tail -n 80 || echo "(no capture)"
  echo "== the tmux pane list =="
  sandbox_tmux list-panes -a -F '#{pane_id} #{session_name} #{window_name} #{pane_pid}' 2>&1 || echo "(no pane list)"
  echo "send replay exit=$(cat "$TEMP_DIR/replay-send.status" 2>/dev/null || echo unknown)"
  echo "task-follow replay exit=$(cat "$TEMP_DIR/replay-task-follow.status" 2>/dev/null || echo unknown)"
  echo "replay exit=$code"
  exit "$code"
}

if [ "${1:-}" = "pane" ]; then
  pane_main "${2:?the temp directory}" "${3:?the tmux socket name}" "${4:-}"
fi
job_main
