#!/bin/sh
# t514: staged rollout / rollback of an integrated Dunking Sheep release.
#
# OPERATOR-ONLY. Dry-run by default: every step PRINTS the commands it would run.
# Nothing is executed without --apply, and --apply is meant for the single runtime
# operator VP15 designates, after nonauthor review of the exact SHA. The author of
# the integration never runs --apply (no self-merge, no self-rollout).
#
# Production shape this script matches (read from the live host 2026-10-29):
#   launcher   ~/.local/bin/dunkingsheep -> exec python3 <release>/dunkingsheep "$@"
#   releases   ~/.local/share/dunkingsheep/releases/<full sha>/   (immutable trees)
#   service    systemd --user dunkingsheep.service, ExecStart pinned to a release in
#              ~/.config/systemd/user/dunkingsheep.service.d/30-reviewed-release.conf,
#              DUNKINGSHEEP_FEDERATED_INPUT=1 and DUNKINGSHEEP_STUCK_CONFIG set there.
#
# Usage:
#   tools/t514_rollout.sh plan      --sha SHA            # show the whole sequence
#   tools/t514_rollout.sh stage     --sha SHA [--apply]  # export tree, compile, no restart
#   tools/t514_rollout.sh activate  --sha SHA [--apply]  # drop-in + launcher + restart
#   tools/t514_rollout.sh verify                         # post-restart checks (read-only)
#   tools/t514_rollout.sh rollback  --sha PREV [--apply] # back to a previous staged sha
#
# Stage and activate are separate on purpose: staging is reversible and silent;
# activate restarts the daemon, which interrupts queued deliveries for a few
# seconds, so it is scheduled by the operator, never by this script's caller.
set -eu

REPO=${T514_REPO:-/home/hunter/workspace/ds-claude-integration}
RELEASES=${T514_RELEASES:-$HOME/.local/share/dunkingsheep/releases}
DROPIN_DIR=$HOME/.config/systemd/user/dunkingsheep.service.d
DROPIN=$DROPIN_DIR/40-t514-integrated-release.conf
PREVIOUS_DEFAULT=182e60299a95a256183e3e8d417063abd4b73dec   # main at PR114, live 2026-10-29

cmd=${1:-plan}; shift || true
SHA=""; APPLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --sha) SHA=$2; shift 2;;
    --apply) APPLY=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

run() {
  if [ "$APPLY" = 1 ]; then echo "+ $*" >&2; "$@"; else echo "DRY-RUN: $*"; fi
}

need_sha() {
  [ -n "$SHA" ] || { echo "--sha <full or short commit> is required" >&2; exit 2; }
  SHA=$(git -C "$REPO" rev-parse --verify "$SHA^{commit}")
}

stage() {
  need_sha
  dest=$RELEASES/$SHA
  if [ -d "$dest" ]; then
    echo "already staged: $dest"
  else
    run mkdir -p "$dest"
    if [ "$APPLY" = 1 ]; then
      git -C "$REPO" archive "$SHA" | tar -x -C "$dest"
    else
      echo "DRY-RUN: git -C $REPO archive $SHA | tar -x -C $dest"
    fi
  fi
  # Compile and version checks run against the STAGED tree, not the worktree.
  run sh -c "cd '$dest' && python3 -m py_compile dunkingsheep dunk_*.py"
  run python3 "$dest/dunkingsheep" --version
  echo "staged $SHA (no restart performed)"
}

# t1128: the desk is part of the release. Its unit runs <data>/current/bridges/run.sh, so
# a board merge only goes live when the staged tree has its venv and ui/dist built (done
# HERE, before the swap, so the restart is seconds not a pip/npm run) and `current` points
# at it. No POOLRAD_* drop-in is needed: bridges/poolrad_paths.py resolves the checkout.
web() {
  need_sha
  dest=$RELEASES/$SHA
  data=$(dirname "$RELEASES")
  run sh -c "cd '$dest' && { [ -x .venv/bin/uvicorn ] || { python3 -m venv .venv && .venv/bin/pip install -q -r bridges/requirements.txt; }; }"
  run sh -c "cd '$dest' && { [ -f ui/dist/index.html ] || { npm install --prefix ui && npm run build --prefix ui; }; }"
  run ln -sfn "releases/$SHA" "$data/current"
  run systemctl --user restart dunkingsheep-web.service
  echo "web deployed from $SHA"
}

activate() {
  need_sha
  dest=$RELEASES/$SHA
  [ -d "$dest" ] || [ "$APPLY" = 0 ] || { echo "stage $SHA first" >&2; exit 1; }
  # A LATER drop-in overrides ExecStart; 30-reviewed-release.conf keeps the
  # environment (VP pane, stuck config, federated input) untouched.
  run mkdir -p "$DROPIN_DIR"
  if [ "$APPLY" = 1 ]; then
    cat > "$DROPIN" <<EOF
[Service]
# t514 integrated release (PR110 + PR111 + PR112 on main@PR114), nonauthor-reviewed.
ExecStart=
ExecStart=/usr/bin/python3 $dest/dunkingsheep serve
EOF
    echo "+ wrote $DROPIN" >&2
  else
    echo "DRY-RUN: write $DROPIN with ExecStart=/usr/bin/python3 $dest/dunkingsheep serve"
  fi
  # The launcher must follow the daemon so CLI and daemon share one tree.
  # The drop-in above is already written: if anything below fails, the override
  # points at a tree the daemon is not running. Say so and name the remedy.
  trap 'echo "activate FAILED after writing $DROPIN; run: $0 rollback --apply" >&2' EXIT
  run python3 "$dest/dunkingsheep" --no-start-daemon install
  run systemctl --user daemon-reload
  run systemctl --user restart dunkingsheep.service
  trap - EXIT
  web
  echo "activated $SHA; now run: $0 verify"
}

verify() {
  echo "## service"; systemctl --user is-active dunkingsheep.service || true
  echo "## launcher"; sed -n '3p' "$HOME/.local/bin/dunkingsheep"
  echo "## web"; systemctl --user is-active dunkingsheep-web.service || true
  readlink "$(dirname "$RELEASES")/current" || true
  echo "## daemon"; dunkingsheep status 2>&1 | sed -n '1,6p'
  echo "## guard"; dunkingsheep guard check 2>&1 | tail -3 || true
  echo "## allocation (empty registry = allocator disabled)"
  dunkingsheep allocation status --json 2>&1 | head -40 || true
  echo "## last daemon log lines"; tail -n 15 "$HOME/.config/dunkingsheep/daemon.log" 2>/dev/null || true
}

rollback() {
  [ -n "$SHA" ] || SHA=$PREVIOUS_DEFAULT
  SHA=$(git -C "$REPO" rev-parse --verify "$SHA^{commit}")
  dest=$RELEASES/$SHA
  [ -d "$dest" ] || { echo "previous release $dest is not staged; stage it first" >&2; exit 1; }
  # Removing the override returns ExecStart to 30-reviewed-release.conf (the
  # PR114 release). If rolling back to a different sha, point the override there.
  if [ "$SHA" = "$PREVIOUS_DEFAULT" ]; then
    run rm -f "$DROPIN"
  else
    if [ "$APPLY" = 1 ]; then
      printf '[Service]\nExecStart=\nExecStart=/usr/bin/python3 %s/dunkingsheep serve\n' "$dest" > "$DROPIN"
    else
      echo "DRY-RUN: write $DROPIN with ExecStart -> $dest"
    fi
  fi
  run python3 "$dest/dunkingsheep" --no-start-daemon install
  run systemctl --user daemon-reload
  run systemctl --user restart dunkingsheep.service
  data=$(dirname "$RELEASES")
  run ln -sfn "releases/$SHA" "$data/current"
  run systemctl --user restart dunkingsheep-web.service
  echo "rolled back to $SHA; now run: $0 verify"
}

case "$cmd" in
  plan)
    need_sha
    echo "1. stage:    $0 stage --sha $SHA --apply       (no restart)"
    echo "2. activate: $0 activate --sha $SHA --apply    (restart, operator-scheduled)"
    echo "3. verify:   $0 verify"
    echo "4. rollback: $0 rollback --apply               (-> $PREVIOUS_DEFAULT)"
    echo "   If activate fails at any step, run rollback immediately: the drop-in is"
    echo "   written before the restart and would otherwise outlive a failed restart."
    echo "Allocation stays OFF until an operator registers a worker (docs/ALLOCATION.md)."
    echo "Legacy CoS native recovery stays OFF until DUNKINGSHEEP_RECOVERY_BINDINGS is"
    echo "independently verified and set in a drop-in (docs/DELIVERY.md, t508)."
    ;;
  stage) stage;;
  activate) activate;;
  web) web;;
  verify) verify;;
  rollback) rollback;;
  *) echo "usage: $0 plan|stage|activate|web|verify|rollback [--sha SHA] [--apply]" >&2; exit 2;;
esac
