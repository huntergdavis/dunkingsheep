#!/usr/bin/env bash
# t594 live acceptance: "reply on the transport the question arrived on".
#
# Run this AFTER PR128 + PR135 are in the deployed build (the running daemon must
# answer `owner ingest` / `owner outbox` / `owner delivered`). It proves the four
# lines the task names, against the live daemon, and prints PASS/FAIL per line:
#
#   1. a Telegram question becomes ONE owner-mailbox row with transport=telegram
#   2. the desk's answer to it is delivered on telegram, with a receipt
#   3. a WEB question produces NOTHING for telegram
#   4. the Telegram exchange is visible in the web conversation (one mailbox)
#
# It writes only to the owner mailbox (a test question on each transport) and
# never touches the bot: delivery is observed through the outbox, not by sending.
# That is deliberate - this harness must be safe to run while the owner is using
# the desk. Anything it cannot observe is reported as NOT OBSERVED, never as a pass.
set -uo pipefail

DS=${DS:-dunkingsheep}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
TG_ID="telegram:acceptance-${STAMP}"
FAILED=0

say() { printf '%s\n' "$*"; }
pass() { say "PASS  $*"; }
fail() { say "FAIL  $*"; FAILED=$((FAILED + 1)); }
skip() { say "NOT OBSERVED  $*"; }

# -- precondition: the deployed daemon must know the verbs ---------------------
# Probe the VERB, not the help text: `help` does not list subcommands, so grepping it
# reported "not deployed" against a build that had the verb (measured 2026-10-29 23:57
# on the deployed release d722815, which has owner_ingest/outbox/delivered).
if ! $DS --json owner outbox telegram --limit 1 >/dev/null 2>&1; then
  say "REFUSING: this daemon does not expose owner ingest/outbox/delivered yet."
  say "Deploy PR128 (+ PR135) first; running now would measure the old build."
  exit 2
fi

say "== t594 live acceptance ${STAMP} =="

# -- 1. a telegram question lands once, on its own transport -------------------
$DS owner ingest telegram "acceptance question ${STAMP}" \
    --external-id "$TG_ID" --subject "t594 acceptance" >/tmp/t594-accept-1.json 2>&1
if [ $? -ne 0 ]; then
  fail "1. owner ingest telegram: $(tail -1 /tmp/t594-accept-1.json)"
else
  pass "1. telegram question ingested ($TG_ID)"
fi

# The transport's own retry must not create a second question.
$DS owner ingest telegram "acceptance question ${STAMP}" \
    --external-id "$TG_ID" --subject "t594 acceptance" >/tmp/t594-accept-2.json 2>&1
if python3 -c 'import json,sys; sys.exit(0 if json.load(open("/tmp/t594-accept-2.json")).get("duplicate") else 1)' 2>/dev/null; then
  pass "1b. a redelivered update did not become a second question"
else
  fail "1b. redelivery created a second question (expected duplicate=true)"
fi

# -- 2. the answer goes out on telegram, and only there ------------------------
# The id comes from the INGEST RESPONSE the script already wrote, not from a second
# query. Measured 2026-10-30: the owner ran this as the owner role and line 2 reported
# NOT OBSERVED because the read-back went through `--json owner conversation` and
# found nothing - a harness gap that made a working behaviour look unmeasured (the
# reply had already been proven by hand: i34486 transport=telegram reply_to=i34484
# with a delivery receipt). One call, one source of truth, no flag-order dependency.
QUESTION=$(python3 - "$TG_ID" <<'PY_ID'
import json, sys
tg = sys.argv[1]
for path in ("/tmp/t594-accept-1.json", "/tmp/t594-accept-2.json"):
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        continue
    message = data.get("message") or {}
    if message.get("external_id") in (None, tg):
        print(message.get("message_id") or "")
        break
PY_ID
)
if [ -z "$QUESTION" ]; then
  skip "2. could not read the question id back; answer routing not measured"
else
  $DS owner reply "$QUESTION" "acceptance answer ${STAMP}" >/dev/null 2>&1 \
    && pass "2a. answered the telegram question" \
    || fail "2a. owner reply failed"
  OUT=$($DS --json owner outbox telegram --limit 20 2>/dev/null)
  if printf '%s' "$OUT" | grep -q "acceptance answer ${STAMP}"; then
    pass "2b. the answer is waiting on the TELEGRAM outbox"
  else
    fail "2b. the answer is not on the telegram outbox"
  fi
  WEB_OUT=$($DS --json owner outbox web --limit 20 2>/dev/null)
  if printf '%s' "$WEB_OUT" | grep -q "acceptance answer ${STAMP}"; then
    fail "2c. the telegram answer also appears on the WEB outbox"
  else
    pass "2c. the telegram answer is not on the web outbox"
  fi
fi

# -- 3. a web question produces nothing for telegram ---------------------------
WEB_Q=$($DS --json owner ingest web "web acceptance question ${STAMP}" 2>/dev/null \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["message"]["message_id"])' 2>/dev/null)
if [ -z "$WEB_Q" ]; then
  skip "3. could not ingest a web question; not measured"
else
  $DS owner reply "$WEB_Q" "web acceptance answer ${STAMP}" >/dev/null 2>&1
  OUT=$($DS --json owner outbox telegram --limit 50 2>/dev/null)
  if printf '%s' "$OUT" | grep -q "web acceptance answer ${STAMP}"; then
    fail "3. a WEB answer reached the TELEGRAM outbox"
  else
    pass "3. a web question produced nothing for telegram"
  fi
fi

# -- 4. the web sees the whole conversation ------------------------------------
CONV=$($DS --json owner conversation --limit 50 2>/dev/null)
if printf '%s' "$CONV" | grep -q "acceptance answer ${STAMP}" \
   && printf '%s' "$CONV" | grep -q "web acceptance answer ${STAMP}"; then
  pass "4. the web conversation shows both transports"
else
  fail "4. the web conversation is missing one of the two exchanges"
fi

say "== ${FAILED} failed line(s) =="
[ "$FAILED" -eq 0 ] || exit 1
