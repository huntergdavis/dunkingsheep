#!/usr/bin/env bash
# Bake-off: the same decode task across several models, judged mechanically.
#
# Each run gets its own report, the citation gate, and the byte judge (which takes the file
# offset the BODY implies, so a model cannot pass by printing a plausible but wrong offset).
# The output is one table: model x body -> bytes verified yes/no, with wall time.
#
#   tools/free_bakeoff.sh "model1 model2" "OVL_0025_06AF:31618:36 OVL_0025_0136:30217:128" [parallel]
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="${POOLRAD_REPO:-/home/hunter/workspace/poolrad-decomp}"
PI="${FREE_PI:-pi}"                 # stand-in binary seam used by the isolated controls
RETRIES="${FREE_RETRIES:-2}"        # bounded 429 retries (attempts after the first)
BACKOFF="${FREE_BACKOFF:-20}"       # seconds, multiplied by the attempt number
TIMEOUT="${FREE_TIMEOUT:-600}"      # per-attempt wall clock

# Validate the retry/backoff/timeout bounds BEFORE any work: a non-integer or an unbounded value
# must stop the run rather than steer the retry loop (an unvalidated bound once turned a bad value
# into an eight-attempt loop with no diagnostic).
fail() { printf 'free_bakeoff: %s\n' "$*" >&2; exit 2; }
is_uint() { case "$1" in ''|*[!0-9]*) return 1 ;; esac; return 0; }
is_uint "$RETRIES" || fail "FREE_RETRIES must be a non-negative integer, got '$RETRIES'"
[ "$RETRIES" -le 10 ] || fail "FREE_RETRIES must be <= 10, got '$RETRIES'"
is_uint "$BACKOFF" || fail "FREE_BACKOFF must be a non-negative integer, got '$BACKOFF'"
[ "$BACKOFF" -le 3600 ] || fail "FREE_BACKOFF must be <= 3600, got '$BACKOFF'"
is_uint "$TIMEOUT" || fail "FREE_TIMEOUT must be a positive integer, got '$TIMEOUT'"
[ "$TIMEOUT" -ge 1 ] && [ "$TIMEOUT" -le 7200 ] || fail "FREE_TIMEOUT must be in 1..7200, got '$TIMEOUT'"

# A rate limit is a PROVIDER DIAGNOSTIC, not the substring "429" anywhere in the output: a file
# name such as file_429.asm is not a rate limit. Only these structured forms count.
is_rate_limit() {
  [ -s "$1" ] || return 1
  grep -qiE 'HTTP[[:space:]/]+[0-9.]*[[:space:]]*429\b|"?(code|status)"?[[:space:]]*[:=][[:space:]]*"?429\b|\b429[[:space:]]+Too[[:space:]]+Many[[:space:]]+Requests\b|\bToo[[:space:]]+Many[[:space:]]+Requests\b|\brate[[:space:]-]?limited[[:space:]]+upstream\b' "$1"
}

MODELS="${1:?usage: free_bakeoff.sh \"models\" \"bodies\" [parallel]}"
BODIES="${2:?bodies required}"
PAR="${3:-4}"
STAMP="$(date '+%Y%m%d-%H%M%S')"
OUT="$HERE/data/bakeoff/$STAMP"
mkdir -p "$OUT"

PROMPT_FOR() { cat <<EOF
You are decoding one function in a DOS game decompilation repo at $REPO. The subject is $1 ($2).
Use the repo's own tools and files. Read-only: do not create, modify or delete any file.
Give me, in this order, and nothing else:
1. FILE OFFSET: <decimal> (0x<hex>) on a line by itself, with both numbers equal to the byte offset inside resources/DOS/POOLRAD/GAME.OVR. Show the arithmetic on the following line.
2. BYTE DUMP: run a hexdump at that offset and paste its output verbatim (or an objdump-style listing whose leading column is the instruction bytes in hex), then the disassembly beside it.
3. SUMMARY: what it does, in one sentence.
4. EVIDENCE: repository paths you actually opened, only paths that exist.
If you cannot confirm something, write 'cannot confirm' and why. Keep it under 300 words.
EOF
}

run_one() {
  local model="$1" body="$2" off="$3" length="$4" desc="$5"
  local safe; safe=$(echo "$model" | tr '/:' '__')
  local dir="$OUT/$safe"; mkdir -p "$dir"
  local rep="$dir/$body.md"
  local t0 t1 rc attempt=1
  t0=$(date +%s)
  while :; do
    ( cd "$REPO" && nice -n 15 timeout "$TIMEOUT" env POOLRAD_REPO="$REPO" "$HERE/tools/free_sandbox.sh" "$dir" "$PI" -p -ne --model "$model" "$(PROMPT_FOR "$body" "$desc")" ) > "$rep" 2>"$dir/$body.try$attempt.err"
    rc=$?
    if [ "$attempt" -gt "$RETRIES" ] || ! is_rate_limit "$dir/$body.try$attempt.err"; then break; fi
    sleep $(( BACKOFF * attempt + RANDOM % 5 ))
    attempt=$((attempt+1))
  done
  cp -f "$dir/$body.try$attempt.err" "$dir/$body.err" 2>/dev/null || true
  t1=$(date +%s)
  local verdict
  # Delivery failures are classified BEFORE any non-empty output can reach the judge: a partial
  # print followed by a timeout or a 429 is a failed delivery, not a candidate decode.
  if [ "$rc" -eq 124 ]; then
    verdict="TIMEOUT (rc=124, attempts=$attempt, not judged)"
  elif is_rate_limit "$dir/$body.err"; then
    verdict="RATE_LIMITED (delivery failure, attempts=$attempt, not judged)"
  elif [ "$rc" -ne 0 ]; then
    # A non-zero exit is a failed delivery even when it printed something: only success may be judged.
    verdict="FAILED (rc=$rc, attempts=$attempt, not judged)"
  elif [ -s "$rep" ]; then
    local bv gate gate_rc
    gate=$(cd "$REPO" && python3 "$HERE/tools/free_body_check.py" "$rep" --expect-offset "$off" --expect-length "$length" 2>&1)
    gate_rc=$?
    printf '%s\n' "$gate" > "$dir/$body.check"
    bv=$(printf '%s\n' "$gate" | sed -nE '/^[[:space:]]*(BYTES MATCH|BYTES DIFFER|PREFIX ONLY|NO DUMP|OFFSET UNRESOLVED):/p' | head -1)
    case "$gate_rc:$bv" in
      0:*"BYTES MATCH"*) verdict="VERIFIED ($bv)" ;;
      *"PREFIX ONLY"*) verdict="SHORT ($bv)" ;;
      *"OFFSET UNRESOLVED"*) verdict="WRONG OR UNRESOLVED OFFSET ($bv)" ;;
      *"BYTES DIFFER"*) verdict="DIFFERS ($bv)" ;;
      *"NO DUMP"*) verdict="NO DUMP" ;;
      *) verdict="UNCHECKED ($bv)" ;;
    esac
  else
    verdict="EMPTY (rc=$rc, attempts=$attempt, not judged)"
  fi
  printf '%s\t%s\t%s\t%ss\n' "$model" "$body" "$verdict" "$((t1-t0))" >> "$OUT/results.tsv"
  echo "done $model $body -> $verdict"
}

# Refuse missing or malformed body extents before starting any API process.
for spec in $BODIES; do
  if [[ ! "$spec" =~ ^OVL_[0-9A-Fa-f]{4}_[0-9A-Fa-f]{4}:[0-9]+:[1-9][0-9]*$ ]]; then
    echo "expected BODY:DECIMAL_FILE_OFFSET:POSITIVE_BODY_LENGTH, got $spec" >&2
    exit 2
  fi
done
: > "$OUT/results.tsv"
jobs_run=0
for model in $MODELS; do
  for spec in $BODIES; do
    IFS=: read -r body off length <<< "$spec"
    run_one "$model" "$body" "$off" "$length" "overlay body $body" &
    jobs_run=$((jobs_run+1))
    if [ $((jobs_run % PAR)) -eq 0 ]; then wait; fi
  done
done
wait

echo
echo "== bake-off $STAMP"
column -t -s $'\t' "$OUT/results.tsv" 2>/dev/null || cat "$OUT/results.tsv"
echo
echo "verified runs / total by model:"
awk -F'\t' '{tot[$1]++; if ($3 ~ /^VERIFIED/) ok[$1]++} END {for (m in tot) printf "  %-46s %d/%d\n", m, ok[m]+0, tot[m]}' "$OUT/results.tsv" | sort
echo "results in $OUT/results.tsv"
