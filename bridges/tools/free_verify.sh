#!/usr/bin/env bash
# Independent verification of accepted Poolrad bodies using FREE models.
#
# WHY: the reviewer lane is the single credit authority and is the bottleneck.
# Free models can independently re-derive an accepted body from raw GAME.OVR
# bytes and report AGREE / DISAGREE, catching documentation and reading errors
# at zero cost.
#
# SAFETY (learned 2026-10-18, docs/INCIDENT_20261018_VP_TAB_CLOSE.md):
#   * These workers are HEADLESS. They get no herdr pane and no pane-control
#     tools, so they cannot close a window or hold a terminal.
#   * This script never calls `herdr ... close` and never spawns a split.
#   * A host-pressure preflight refuses to add load to an already-overloaded
#     machine, and workers run under `nice`.
#
# WHAT THEY ARE NOT: a credit authority. They decode statically and do NOT
# re-run the Unicorn / DOSBox-X differential harness, so a wrong-but-
# self-consistent doc can still pass them. Never let them write accounting,
# classifier, ledger, caveat or credit files.
#
# USAGE:
#   tools/free_verify.sh OVL_0038_001F "U38 +001F, 57B/22insns"
#   tools/free_verify.sh --models big-pickle,longcat OVL_0038_001F "U38 +001F"
#   tools/free_verify.sh --count 4 OVL_0038_1472 "U38 +1472, 30B/15insns"
#
# Models:
#   big-pickle              opencode/* (OpenCode client, free tier)     via `opencode run`
#   deepseek-v4.1-flash-free, nemotron-3.5-lightning-free, beam-free,
#   mimo-v2.6-flash-free, glm-5.3-flash-free, space-bunny-free          via `opencode run`
#   longcat-2.5-preview-free, cat-jam-free                              via `pi -p` (opencode-go)
#
# Reports land in /tmp/free-verify/<BODY>/ and are printed at the end.
set -euo pipefail

export PATH="$HOME/pi/agent/bin:$HOME/.pi/agent/bin:$HOME/.config/dunkingsheep/bin:$PATH"

find_repo() {
  for d in "${POOLRAD_REPO:-}" /home/hunter/workspace/poolrad-decomp /home/hunter/workspace/poolrad-decomp-claude-lane; do
    [ -n "$d" ] && [ -d "$d/.git" ] && { printf '%s\n' "$d"; return 0; }
  done
  return 1
}

COUNT=1
MODELS_CSV="big-pickle,longcat-2.5-preview-free"
FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --count) COUNT="$2"; shift 2;;
    --models) MODELS_CSV="$2"; shift 2;;
    --force) FORCE=1; shift;;
    --) shift; break;;
    *) break;;
  esac
done
BODY="${1:?usage: free_verify.sh [--count N] [--models a,b] <BODY_ID> [description]}"
DESC="${2:-}"
REPO="$(find_repo)" || { echo "error: no Poolrad checkout found; set POOLRAD_REPO" >&2; exit 1; }
OUT="/tmp/free-verify/$BODY"
mkdir -p "$OUT"

# --- host-pressure preflight: do not pile more load on a struggling host ---
CPUS=$(nproc)
LOAD1=$(awk '{print $1}' /proc/loadavg)
PSI=$(awk -F'avg60=' '/some/{split($2,a," ");print a[1]+0}' /proc/pressure/cpu 2>/dev/null || echo 0)
if [ "$FORCE" -eq 0 ] && awk -v l="$LOAD1" -v c="$CPUS" -v p="$PSI" 'BEGIN{exit !((l > c*2) || (p > 50))}'; then
  echo "refusing to start: load1=$LOAD1 on $CPUS cpus, cpu-pressure60=$PSI% (use --force to override)" >&2
  exit 1
fi

IFS=',' read -r -a MODELS <<< "$MODELS_CSV"
mkdir -p "$OUT"
PIDS=()
for i in $(seq 1 "$COUNT"); do
  MODEL="${MODELS[$(( (i-1) % ${#MODELS[@]} ))]}"
  NAME="$(printf '%s' "$MODEL" | tr '/.' '__')"
  REPORT="$OUT/$NAME-$i.md"
  PROMPT="You are a reverse-engineering verifier on the Poolrad decompilation. Workspace: $REPO (objdump -D -b binary -m i8086 --adjust-vma=<addr> is available).

TASK - independent verification, NOT a credit job. Independently re-derive this accepted body and report AGREE or DISAGREE with evidence:
  Body: $BODY${DESC:+ ($DESC)}.
Find its doc by grepping docs/re for the hex id, and its model under src/recovered/.

Do: (1) read the doc and model; (2) locate the ORIGINAL bytes in GAME.OVR -- use the repo's own overlay parser/inventory to get the file offset rather than guessing -- and decode them YOURSELF with objdump; (3) verify the body SHA-256 against the doc and, where the doc records them, the GAME.OVR/START.EXE hashes; (4) derive the semantics independently and, for each conditional, say which way it goes; (5) compare instruction-by-instruction against the accepted model; (6) report the disassembly, your derived semantics, MATCH or MISMATCH, and EXPLICITLY what you could not confirm.

Rules that matter: do NOT edit accounting, classifier, ledger, caveat or credit files. Do NOT claim credit for anything. Never write a comparison mask or change a model to make something agree -- if it does not match, say so. 'I cannot confirm X' is worth more than a confident guess. Cite file paths and byte offsets. Keep the report under 400 words."

  if [ "${MODEL#opencode/}" != "$MODEL" ] || [ "$MODEL" = "big-pickle" ] || [[ "$MODEL" == *-free && "$MODEL" != longcat* && "$MODEL" != cat-jam* ]]; then
    # OpenCode client path (free tier is only usable from inside OpenCode).
    MODEL_REF="opencode/$MODEL"; [ "${MODEL#opencode/}" != "$MODEL" ] && MODEL_REF="$MODEL"
    ( cd "$REPO" && nice -n 10 timeout 900 opencode run --model "$MODEL_REF" "$PROMPT" >"$REPORT" 2>&1 ) &
  else
    ( cd "$REPO" && nice -n 10 timeout 900 pi -p --no-session --provider opencode-go --model "$MODEL" "$PROMPT" >"$REPORT" 2>&1 ) &
  fi
  PIDS+=("$!")
  echo "started $MODEL -> $REPORT (pid ${PIDS[-1]})"
done

FAIL=0
for p in "${PIDS[@]}"; do wait "$p" || FAIL=$((FAIL+1)); done
echo "reports in $OUT"
grep -lE 'MISMATCH|DISAGREE' "$OUT"/*.md 2>/dev/null || true
[ "$FAIL" -eq 0 ] || echo "warning: $FAIL worker(s) exited non-zero; inspect reports" >&2
