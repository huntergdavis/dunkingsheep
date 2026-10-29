# Pi poll backoff (t399)

`poll-backoff.patch` is against the installed `~/.pi/agent/extensions/dunkingsheep.ts`
at input SHA-256 `4f971f4ba968241401f34d6acd110203e59a55e2dfe62b9973d398e0019a4ac4`
(the owner-managed extension; it is not in this repository). Apply it to a private
copy and deploy only as part of the reviewed rollout.

The change:

- `dunk-poll` remains the hot path.
- The legacy three-CLI fallback (`status` + `inbox count` + `list`) runs **only**
  when the helper itself did not spawn (a thrown `ENOENT`), or exits 127 (the
  established "helper absent" compatibility case). A **started** helper that
  returns nonzero — for example `dunk-poll --socket <absent>` printing "daemon
  socket not found" — is not a missing executable and never triggers the three
  CLI reads. It goes to backoff and a stale/unknown state.
- One poll is in flight; failures use bounded exponential backoff (base
  `POLL_MS`, capped at 5 minutes). The interval still calls the poller, but a
  poll inside the backoff window spawns nothing.
- The last good snapshot is retained but marked `stale`, with `stale_reason` and
  `last_good_at`; the context line says `Inbox: STALE ...` rather than showing
  the old counts as healthy. With no last-good snapshot, no synthetic snapshot
  is emitted.
- Message/task writes are unchanged; the fallback commands are reads only.

Tests: `python3 -m unittest tests.test_pi_poll_backoff`. The test extracts the
`makePoller` / `staleLine` functions from the patch and runs them against fake
executors, covering success, timeout, daemon-unavailable/EAGAIN, missing helper,
missing helper with fallback failure, one-inflight, backoff cap and stale UI.
