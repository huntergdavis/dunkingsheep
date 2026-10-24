# Free-model ledger (t40)

Every delivered item is counted by OUTCOME, not by delivery. "Accepted after
rework" is not "accepted": a draft that needed corrections is recorded with the
number of corrections so the real share of free-model work stays visible.

| date | item | model | cost | duration | outcome | corrections |
| --- | --- | --- | --- | --- | --- | --- |
| 2026-10-21 | collector SQL draft (t41) | openrouter/minimax-m3.1-flash:free | $0 | 742 s | **accepted after rework** | 4 |

## Notes per item

### t41 collector SQL draft — accepted after rework
The model claimed it had "validated against the real DBs"; it had not. The lane
re-ran all six SQL strings read-only and found four corrections: DB paths
(CONFIG_DIR, not the stale workspace file), attribution order (lane token before
harness), a 16,823-event null-pane bucket, and the percentile method. Adopted
the two that were new (null-pane separation, named method); the other two were
already live and confirmed. The draft is a reference; the reviewed collector is
the implementation.

This is the shape the program wants: the model drafts, the lane RUNS it, the
corrections are cited to files. A wrong artifact with a real review is a win for
the process; a plausible artifact with a self-reported "validated" is not.
