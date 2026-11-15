# Read consumption and response-loss recovery

Normal CLI/MCP reads consume returned mail: the daemon atomically sets read and
acknowledgment timestamps/state and saves the recovery receipt. A separate ack
call is unnecessary. CLI `inbox read --no-ack` or MCP `inbox(action="read",
ack=false)` keeps the old workflow: mark read, display, explicitly acknowledge.
`--peek` / `peek=true` never changes messages or receipts. Messages-only remains
the default; task queues still require explicit opt-in.

The body/ID fields are unchanged. A normal returned message has state `acked`;
manual-mode results have state `read`. `marked_read` still counts committed
messages. The recovery buffer retains exactly the latest nonempty delivered
batch, even after implicit consumption. Empty reads preserve it, and recovery
can repeat after another lost response. A newer nonempty read replaces it;
existing retention/deletion bounds apply. This is bounded replay, not an
exactly-once or unlimited-history guarantee.

Explicit ack is still idempotent, including on already consumed mail. It clears
only its named IDs from the recovery receipt; partially confirming a recovered
batch must not discard other auto-acked bodies. Ordinary callers keep the read
mailbox boundary. Receipt cleanup also prunes missing/foreign legacy entries.
During a read, a preparation, acknowledgment-write or receipt-write failure leaves queued mail
and the previous receipt unchanged. Desk-clamped responses consume only IDs
whose bodies fit the returned response, with their final state sized before
selection. Both read and consumption telemetry are recorded after commit.

## Notification and escalation audit

| Consumer | Actual predicate | Effect of either successful read mode |
| --- | --- | --- |
| Inbox unread/oldest counters (`inbox::count`) | `state='queued'`, oldest queued `created_at` | Read and acked are both excluded; next-job uses this same unread count. |
| Watchdog inbox unread/live/dead (`watchdog/rules_inbox.rs`) | `state='queued'` at the time cutoff | A consumed or manually read message stops being unread, as before. |
| Watchdog lane activity (`watchdog/rules_lanes.rs`) | `MAX(read_at)` in its time window | Both modes retain a read timestamp. |
| Digest coalescing (`Store::digest_waiting`) | `read_at IS NULL AND acked_at IS NULL` plus no archive/cancel stamp | Either mode ends the old digest's waiting period. |
| Dunk `only_when=idle` | Pane `agent_status`, not inbox timestamps | No change to its idle gate. |
| Dunk `skip_if_unconsumed` | Terminal composer text via `input_pending`, not inbox timestamps | No change to its typing/consumption gate. |
| Metrics `queue.oldest_wait_s` | Queued outbox snapshot, not inbox `acked_at` | Unrelated to inbox implicit acknowledgment. |

No consumer predicate above is rewritten. Private native regression reads both
manual and implicit digest batches and checks real inbox counters, watchdog
queued selection, digest waiting and timestamp state. Existing timer/next-job
and watchdog suites continue to cover their own unchanged predicates.

## Harness compatibility

The inspected pi extension's sidebar uses read-only `dunk-poll`/count data and
its prompt tells the agent to invoke the CLI; it does not consume inbox bodies.
The repository Codex/opencode turn hooks signal turns and do not read inbox
bodies. Installed harness configuration is not modified. CLI and MCP adapter
fixtures keep message bodies/IDs intact and forward manual mode explicitly.
Existing explicit ack calls remain valid, including after an automatic read;
new guidance removes the redundant call from the normal workflow. Frozen Python
runtime sources are unchanged; Rust-only CLI, schemas and guide assets are
produced by their generators.

Reuses the t128 response-loss contract from shared memory and
`tests/test_dunk_response_loss.py`: one receipt per mailbox, empty-read retention,
repeatable recovery and explicit retirement. The new native tests also force
ack-write and receipt-write failures before replaying the retained batch.
