# Send backpressure (`skip_if_unconsumed`)

Status: **fixed and verified against herdr 0.9.5** (2026-10-19). Original spec:
[`history/BACKPRESSURE_BUG.md`](history/BACKPRESSURE_BUG.md) and
[`history/BACKPRESSURE_PROMPT.md`](history/BACKPRESSURE_PROMPT.md).

## The failure

A self-dunk on an unattended Claude pane fired repeatedly and every prompt
queued in the input box, then flushed into one turn. `only_when="idle"` cannot
see that: a pane at a prompt with unread input reports `idle`. The first
backpressure attempt (`0b8a113`) inferred consumption from **agent busyness**
plus a **verbatim tail match**, and neither worked:

- Busyness on an unrelated turn (or an earlier queued send) latched the newest
  send as taken.
- Claude Code collapses a long queued paste (`[Pasted text #1 +75 lines]`,
  `+75 lines (ctrl+o to expand)`), so the verbatim first 80 characters were
  never visible.
- No evidence was treated as consumed, so the default resolved toward sending.

## The fix (`add23ca`, pi/fallback in `2.21.2`)

Consumption is read from the pane's **input box**, not from busyness:

1. `_input_area` isolates the box as everything from the **last prompt-sigil
   line** (Claude `❯`, Codex `›`) to the bottom, so transcript-collapse chips
   are excluded.
2. `_input_pending` reports pending when the box holds a collapsed-input marker
   (`ctrl+o to expand`, `paste again to expand`, `pasted text`, `[image`) or the
   sent text's whitespace-collapsed prefix.
3. **pi has no prompt sigil.** Its input is the line(s) between the last two
   full-width `─` borders, so `_input_area` falls back to that region. An empty
   box is consumed; queued text is pending.
4. An unreadable pane or unrecognisable box is **inconclusive**: the send is
   held up to `MAX_UNKNOWN_SKIPS = 3` times, then sent **once** with
   `last_error` set (`detector fallback`). A parser gap cannot silently kill
   the nudge; a genuinely pending box is still skipped indefinitely.

A skip reschedules `next_send_at`, bumps `skip_count`, sets status
`Skipped (unconsumed)`, and leaves `send_count`, `last_sent_at` and
`last_error` alone. The first send always proceeds.

The `dunk.skipped` metric uses the schedule's `last_skip_reason`: `unconsumed`
for detected pending input, or `unrecognised` when the box cannot be classified.
Both keep outcome `held`; send counters, rescheduling and guards are unchanged.
These are additive reason values in metrics schema 1. Historical skip events
were mislabeled `stranded` and are not rewritten; their exact classification
cannot be recovered from that label alone. Actual stranded-text guard holds
and dead-session refusals retain their existing classifications.

Surfaces: `skip_if_unconsumed` / `--skip-if-unconsumed` (CLI add/update),
`skip_if_unconsumed` in the MCP schemas, and the TUI key **`u`** (the idle gate
is **`o`**). Persisted in `dunks.json`; old files default to `false`.

## Blind spots (documented, not hidden)

- It recognises **our own** queued send, verbatim or collapsed; it does **not**
  recognise arbitrary short text a human left in the box.
- A short prompt whose echo lingers in the box can hold one extra interval.
- It assumes the harness draws an input box (Claude/Codex/pi), not a bare shell.
- A pi box is read between its last two borders; a renderer change that removes
  the borders falls back to the 3-skip detector fallback (and surfaces it).
- A stale `target pane not found` error is cleared by the reaper as soon as the
  pane is back, and by the next successful send.
- A turn that starts and finishes entirely between observations is irrelevant
  now: consumption is judged from the box at send time, not sampled.

## Tests

`tests/test_dunk_core.py` models real pane text (`tests/fakes.py`), including
the four field cases: busy for an unrelated reason; busy processing an earlier
send; collapsed placeholder; long prompt. The first send, the skip accounting,
the state round-trip and the old-state default are covered too. A **herdr 0.9.5
fixture** captured from a real Claude pane (`claude_box_empty_herdr_095`) and
**real pi fixtures** (`pi_box_empty`, `pi_box_working_empty`, `pi_box_pending`)
guard the parsers; the 3-skip detector fallback and the stale-error clear have
their own tests.

```sh
python3 -m unittest tests.test_dunk_core -q     # 137 tests
dunkingsheep guard check                        # guard intercepts pane typing
dunkingsheep health                             # daemon + memory health
```

Verified 2026-10-19 on herdr 0.9.5: 137 tests green; `guard check` ok;
`health ok`; a real `herdr pane read --source visible` tail from an idle Claude
pane parses as an empty box (consumed).
