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

## The fix (`add23ca`)

Consumption is read from the pane's **input box**, not from busyness:

1. `_input_area` isolates the box as everything from the **last prompt-sigil
   line** (Claude `❯`, Codex `›`) to the bottom, so transcript-collapse chips
   are excluded.
2. `_input_pending` reports pending when the box holds a collapsed-input marker
   (`ctrl+o to expand`, `paste again to expand`, `pasted text`, `[image`) or the
   sent text's whitespace-collapsed prefix.
3. An unreadable pane or unrecognisable box is **inconclusive**, and for this
   opt-in feature "not sure" means **skip**: a missed nudge is recoverable next
   interval, a stack is not.

A skip reschedules `next_send_at`, bumps `skip_count`, sets status
`Skipped (unconsumed)`, and leaves `send_count`, `last_sent_at` and
`last_error` alone. The first send always proceeds.

Surfaces: `skip_if_unconsumed` / `--skip-if-unconsumed` (CLI add/update),
`skip_if_unconsumed` in the MCP schemas, and the TUI key **`u`** (the idle gate
is **`o`**). Persisted in `dunks.json`; old files default to `false`.

## Blind spots (documented, not hidden)

- It recognises **our own** queued send, verbatim or collapsed; it does **not**
  recognise arbitrary short text a human left in the box.
- A short prompt whose echo lingers in the box can hold one extra interval.
- It assumes an agent pane that draws an input box (Claude/Codex), not a bare
  shell.
- A turn that starts and finishes entirely between observations is irrelevant
  now: consumption is judged from the box at send time, not sampled.

## Tests

`tests/test_dunk_core.py` models real pane text (`tests/fakes.py`), including
the four field cases: busy for an unrelated reason; busy processing an earlier
send; collapsed placeholder; long prompt. The first send, the skip accounting,
the state round-trip and the old-state default are covered too. A **herdr 0.9.5
fixture** captured from a real Claude pane (`claude_box_empty_herdr_095`) guards
the parser against the upgrade.

```sh
python3 -m unittest tests.test_dunk_core -q     # 137 tests
dunkingsheep guard check                        # guard intercepts pane typing
dunkingsheep health                             # daemon + memory health
```

Verified 2026-10-19 on herdr 0.9.5: 137 tests green; `guard check` ok;
`health ok`; a real `herdr pane read --source visible` tail from an idle Claude
pane parses as an empty box (consumed).
