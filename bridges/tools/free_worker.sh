#!/usr/bin/env bash
# One headless free-model worker on one Poolrad body, with two mechanical gates in front
# of a human read. Never a credit authority; never pane control; never writes a ledger.
#
#   tools/free_worker.sh OVL_0025_06AF "overlay 0025 offset 0x06AF"
#
# Writes: data/free_runs/<body>/report.md, citations.txt, bytes.txt, verdict.txt
# Exit:   0 both gates passed, 1 a gate failed (draft is not usable), 2 setup error.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO="${POOLRAD_REPO:-/home/hunter/workspace/poolrad-decomp}"
MODEL="${FREE_MODEL:-openrouter/nvidia/nemotron-3-ultra-550b-a55b:free}"
BODY="${1:?usage: free_worker.sh <BODY_ID> <description>}"
DESC="${2:-$BODY}"
OUT="$HERE/data/free_runs/$BODY"
mkdir -p "$OUT"

PROMPT="You are decoding one function in a DOS game's decompilation repo at $REPO. The subject is $BODY ($DESC).
Use the repo's own tools and files. Read-only: do NOT create, modify or delete any file, and do not run any command that writes.
Give me, in this order, and nothing else:
1. FILE OFFSET: the byte offset of this function inside resources/DOS/POOLRAD/GAME.OVR, showing the arithmetic (overlay base + body offset).
2. BYTE DUMP, AND THIS IS THE PART THAT IS CHECKED: run a hexdump of the file at that offset whose output you can quote verbatim - e.g. 'od -A d -t x1 -j OFFSET -N LENGTH resources/DOS/POOLRAD/GAME.OVR', or xxd with the right -s/-l - and paste that output, then give the disassembly beside it. QUOTE the tool's output; do not transcribe bytes from memory. Two of four workers before you failed here by reordering bytes they had recalled, and a quoted dump cannot fail that way.
3. SUMMARY: what the function does, in one sentence.
4. EVIDENCE: repository paths you actually opened. Only paths that exist.
If you cannot confirm something, write 'cannot confirm' and say why. A confident guess is worth less than an honest gap.
Keep it under 400 words."

echo "worker $BODY on $MODEL at $(date '+%F %T %Z')" >"$OUT/run.log"
( cd "$REPO" && nice -n 15 timeout 900 env POOLRAD_REPO="$REPO" "$HERE/tools/free_sandbox.sh" "$OUT" pi -p -ne --model "$MODEL" "$PROMPT" ) >"$OUT/report.md" 2>>"$OUT/run.log"
rc=$?
if [ ! -s "$OUT/report.md" ]; then
  echo "FAILED: model produced nothing (exit $rc)" | tee -a "$OUT/run.log"
  exit 2
fi

POOLRAD_REPO="$REPO" python3 "$HERE/tools/free_verify_citations.py" "$BODY" "$OUT/report.md" >"$OUT/citations.txt" 2>&1
cits=$?
EXPECT=$(cd "$REPO" && python3 - "$BODY" <<'PYEOF'
import json,sys
body=sys.argv[1]; seg=body.split('_')[1]; off=int(body.split('_')[2],16)
try:
    inv=json.load(open('resources/review-pr276-accounting/overlay_inventory.json'))
    want=int(seg,16)
    for u in inv['units']:
        if int(str(u.get('stub_para')),0)==want:
            print(u['ovr_offset']+off); break
except Exception:
    pass
PYEOF
)
EXPLEN=$(cd "$REPO" && python3 - "$BODY" <<'PYEOF2'
import json,sys
body=sys.argv[1]; seg=body.split('_')[1]; off=int(body.split('_')[2],16)
try:
    inv=json.load(open('resources/review-pr276-accounting/overlay_inventory.json'))
    want=int(seg,16)
    for u in inv['units']:
        if int(str(u.get('stub_para')),0)==want:
            for f in (u.get('functions') or []):
                if f.get('rel')==off:
                    print(f.get('size_upper_bound') or 0); break
            break
except Exception:
    pass
PYEOF2
)
if [ -n "${EXPECT:-}" ]; then
  ( cd "$REPO" && python3 "$HERE/tools/free_body_check.py" "$OUT/report.md" --expect-offset "$EXPECT" ${EXPLEN:+--expect-length "$EXPLEN"} ) >"$OUT/bytes.txt" 2>&1
else
  ( cd "$REPO" && python3 "$HERE/tools/free_body_check.py" "$OUT/report.md" ) >"$OUT/bytes.txt" 2>&1
fi
bytes=$?
{
  echo "body: $BODY"
  echo "model: $MODEL"
  echo "citations gate: exit $cits"
  grep -E 'VERDICT' "$OUT/citations.txt" || true
  echo "bytes gate: exit $bytes"
  grep -E 'BYTES|NO DUMP|OFFSET' "$OUT/bytes.txt" || true
} >"$OUT/verdict.txt"
cat "$OUT/verdict.txt" >>"$OUT/run.log"

# Both gates must pass for the draft to be worth a human's time.
if [ $bytes -ne 0 ]; then exit 1; fi
if [ $cits -ne 0 ] && ! grep -q 'NO CHECKABLE CITATIONS' "$OUT/citations.txt"; then exit 1; fi
exit 0
