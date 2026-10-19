<!-- Historical spec, kept for the record. Implemented and fixed in
     add23ca; see docs/BACKPRESSURE.md for the final design. -->

# Bug: skip_if_unconsumed does not prevent stacking on Claude Code panes

The backpressure feature shipped in 0b8a113 did not prevent the exact scenario it was built for. Reproduced live, twice, on the control agent's own rounds dunk.

## Evidence

Dunk `d3`, `only_when="idle"`, `skip_if_unconsumed=true`, interval 120m, targeting Claude Code pane `w9:p3`.

Daemon log shows five consecutive sends, every one logged `sent`, none skipped:

```
2026-09-13 21:15:58  d3 -> w9:p3: sent (sent)
2026-09-13 23:15:58  d3 -> w9:p3: sent (sent)
2026-09-14 01:15:59  d3 -> w9:p3: sent (sent)
2026-09-14 03:16:00  d3 -> w9:p3: sent (sent)
2026-09-14 05:16:01  d3 -> w9:p3: sent (sent)
```

All five arrived in the target session as one stacked turn. Dunk state after: `skip_count: 0`, `last_skipped_at: null`, `awaiting_consumption: false`. The skip path never once executed. Dunk `d4` shows the same, `skip_count: 0` after 10 sends.

## Why both detection paths fail

`_previous_send_consumed` has two signals and both give the wrong answer here.

**1. The busy latch is far too eager.** `_observe_consumption` polls every `IDLE_POLL_S` for the entire countdown and clears `awaiting_consumption` the first time the target is seen in `BUSY_STATUSES`. Over a 120-minute window an active agent pane is busy at some point for reasons that have nothing to do with the pending send. Worse, when several sends are already queued, the pane goes busy processing an *earlier* one, which latches the *current* one as consumed. The signal proves the agent is alive, not that it took this specific message.

**2. The tail check is defeated by input collapsing.** `CONSUMPTION_TAIL_LINES = 15` and `CONSUMPTION_PREFIX_CHARS = 80`. Claude Code does not render long queued input verbatim at the bottom of the pane. It collapses it, showing placeholders such as `+75 lines (ctrl+o to expand)` and `paste again to expand`. A dunk prompt of any real length therefore never appears as its literal first 80 characters in the last 15 lines, the needle is not found, and the function concludes the text was consumed.

**3. The fallback biases toward sending.** No evidence is treated as consumed. Given that both signals above fail silently on exactly the long-prompt, unattended-pane case this feature targets, the default resolves the wrong way every time.

Note also that the busy latch pre-empts the tail check. Once the latch clears `awaiting_consumption`, `_previous_send_consumed` returns True at its first line and the more meaningful tail check never runs.

## Suggested direction

Do not just tune the constants. The core problem is that neither signal actually observes consumption.

- **Prefer a positive delivery marker over inference.** Consider appending a short unique token to each send, for example a zero-width or bracketed `[dunk d3 #16]` marker, then treating the send as consumed only when that specific marker has left the input area or when the agent's transcript shows it was received. This distinguishes *this* send from general pane activity, which is the distinction the current code cannot make.
- **If the busy latch is kept, scope it.** Only count a busy transition that begins after the send and that is separated from any other pending send, and never let it clear the flag while more than one send is outstanding.
- **Reverse the fallback for this feature.** When `skip_if_unconsumed` is on and no signal is conclusive, skipping is the safe outcome. A missed nudge is recoverable on the next interval; a stacked pile-up is not.
- **Handle collapsed input explicitly.** Detect the placeholder strings Claude Code emits for collapsed input and treat their presence as evidence that something is queued and unread.

## Tests to add

The current suite passes while the feature does not work in the field, so the fakes are not modelling the real pane. Add cases where:

- The pane goes busy for an unrelated reason during the countdown, then the send is still due and the previous text was never taken. Must skip.
- Several sends are outstanding and the pane goes busy processing an earlier one. Must skip, not treat the latest as consumed.
- The pane tail shows a collapsed-input placeholder rather than the verbatim text. Must skip.
- The target is a long prompt exceeding what the pane renders inline. Must skip.

Reproduce the failure first with a test that fails against the current code, then fix it.
