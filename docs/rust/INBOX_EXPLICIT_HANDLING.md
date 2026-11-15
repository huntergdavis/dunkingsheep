# Read consumption and explicit handling (t5152)

A default non-peek read consumes mail (`state=acked`) and records the delivery
receipt atomically, but does not claim the human's question was handled.
`messages.implicit_ack=1` distinguishes that consumption from an explicit ack.
An explicit ack, including an ack of an already-consumed row, or a newly linked
non-human reply clears the flag. Repeated explicit ack remains idempotent and
recovery still clears only explicitly named IDs.

The migration adds `implicit_ack INTEGER NOT NULL DEFAULT 0` under an immediate
transaction. It does not update historical rows or timestamps. Legacy rows retain
explicit meaning. Status adds `implicit_ack: true` only for implicit consumption;
legacy explicit response shapes remain unchanged. Read-only status also accepts
pre-column databases. Conversation/reply rows omit the internal flag.

## Human-visible consumers audited

- Owner conversation `answered`: a linked reply or ack timestamp with implicit
  flag clear. Implicit owner-mailbox reads keep the question unanswered/waiting.
- Desk lane delivery refresh: implicit ack maps to Read by agent; explicit ack or
  linked reply maps to Handled by agent. A send response lacks provenance, so an
  already-acked send result conservatively shows Read pending status refresh.
- Desk owner thread: unanswered rows with `read_at` show Read by agent; answered
  rows show answered. A timestamp alone no longer claims handling.
- `message_state` seen cell: retain `read_at`, suppress `seen.acked_at` for an
  implicit ack. Raw `inbox_status.acked_at` remains a storage/consumption timestamp.
- Telegram `/status`: the owner-conversation row with acked state and
  `answered=false` says read; answered/legacy rows retain their existing state
  wording. Delivery receipts/outbox routing use `delivered_at`, not `acked_at`.
- Digest: `digest_waiting` requires both read and ack timestamps to be absent;
  it denotes unconsumed mail, not human handling. No handled/answered wording is
  derived from `acked_at` there. Replacing a digest resets the provenance flag.
- Retention, watchdog/unread counts and wait gates remain consumption predicates.
  They do not claim a reply or explicit human handling.

## Release and rollback

Deploy the corrected desk consumer first, then the implicit-read daemon. Roll
back in reverse: daemon first, retaining the corrected desk and additive column.
Do not drop the column or rewrite mail history. An old desk ignores the flag and
would again mislabel implicit reads; it is not a compatible desk rollback.

Rollback target a47168cd uses named-column INSERTs that omit the new flag and
named-field reads; the column's DEFAULT 0 preserves those writes. The regression
runs that target's main send INSERT shape against the migrated database, checks
its default and reads the row, alongside reopen/history timestamp preservation.
The unchanged installed a47168cd daemon was also run on an isolated database
with the additive column: boot, history read, owner ingest, linked reply and status
all passed. Schema, historical timestamps and the column were retained; legacy
inserts defaulted to zero. Evidence: `311-t5152-evidence/rollback-result.json`,
SHA91b23f4b27f2edca997c4b8c04eb9546799cb2b3f9db31ca79c599c4e3e3d87a.
This used fresh private auth/config/home/socket and no production credentials,
mail, panes or services; no live rollback was performed.
An old daemon cannot promote an already-implicitly-acked row on explicit ack;
its linked reply still counts as an answer. The fixed desk conservatively shows
Read for such a row until a reply or the corrected daemon records explicit ack.

Tests cover real stored owner/VP-web shapes: lane reads/no reply, lane reads then
linked reply, explicit ack after consumption, privileged owner-mailbox read,
owner answer, legacy insertion, migration history and repeated explicit ack.
No public MCP inputs or size CEILING change.
