# The dispatcher: deterministic feeding, no model in the loop

Every lane finished its assignment, went idle, and nothing noticed. A human
re-fed four lanes by hand, twice. This is the structural fix: a standby queue and
a five-minute tick. **There is no model anywhere in it** - the gate is a pane's
delivery state and an empty mailbox, both deterministic reads.

## The board is the queue (a hand-written list is a future omission)

The standby queue started hand-written, and it ran dry twice in one night while
open tasks sat unclaimed on the board. That is the same defect as a hand-maintained
checklist: **the list was already wrong before anyone touched it.**

So the tick **derives** its items from the task board first, and the hand queue is
only for work that is not a task yet. A task is dispatchable when it is:

- `open` — not in progress, done or cancelled;
- **not blocked** — any declared `block_reason` disqualifies it (held for the owner
  is just one of those reasons);
- **not dependency-gated** — every `depends_on` is done;
- **not claimed** — no live lease;
- **not recently dispatched** — a 6-hour cooldown, so a lane is not re-fed the same
  assignment while it works;
- **not already in the hand queue**.

Order is priority (urgent → low) then age. A title longer than 90 characters is
reported as **coarse**: an umbrella like "advance strict C" is a signal to split it,
not a reason to keep a parallel hand-written list.

With this, `queue empty` means **no open unblocked work anywhere** — a real
finding — instead of "somebody forgot to top it up".

## Step 2 ordering: codex's turn socket first

Codex is the harness whose mail depends on a short hold: it does not queue typed text,
so a nudge to a busy codex lane waits for its turn to end. That makes
`turn/started` + `turn/completed` on codex's socket **load-bearing** for step 2 rather
than a refinement - its acceptance test is concrete: a nudge to a mid-turn codex lane
is delivered within N seconds of `turn/completed`, with the hold reason gone.

## A tick has a budget, and a dead feeder is a number

A tick classifies every lane, and that classification used to cost **3-4 pane reads
per lane** — 35 lanes at ~1.5 s each, which exceeded the client's socket timeout, so
the systemd unit failed with `status=3/NOTIMPLEMENTED` and the dispatcher was dead for
hours while a human fed lanes by hand. Three fixes:

- **one styled read per classified lane** (`delivery_state` reuses the screen it
  already read instead of asking `send_blocker` to read again);
- **no read at all for a lane that reports `working`** — it can never be fed, so the
  tick skips it;
- **a batch timeout** for `dispatch` (`DUNKINGSHEEP_BATCH_TIMEOUT_S`, 180 s): a tick is
  a batch, not a single send, and a 15 s socket timeout turned a working tick into a
  failed unit.

And because the failure was invisible while the manual path covered for it, **health
now reports the feeder**:

```text
dispatcher=ok last_tick=82s ago
dispatcher=stale last_tick=1030s ago          # a problem line names the threshold
dispatcher=absent last_tick=never             # no tick has ever run
```

`health` lists a problem when the last successful tick is older than
`DISPATCH_STALE_S` (16 minutes — two missed ticks) or when no tick has ever run, so
"the feeder is dead" is a value with an age rather than an absence.

## The three moving parts

| part | what it is |
|---|---|
| **standby queue** | ordered one-line assignments (`dispatch add <task-id> "<brief>"`), optionally reserved for one lane |
| **gate** | a lane is fed only if `delivery_state` says **ready** *and* its mailbox has no unread mail |
| **tick** | `dunkingsheep dispatch tick`, run by `dunkingsheep-dispatch.timer` every 5 minutes |

## Why the gate is two conditions, not one

**Ready** means the pane can accept a message right now (not working, no draft, not
dead, not a placeholder-only box). **Empty mailbox** means the lane has no
assignment waiting already. A ready lane with unread mail is *not* idle in the
sense that matters: it has work to do once it reads. Sending it a standby item
makes two assignments compete and the queue becomes noise.

`lane_plan()` reports five buckets, so a tick never has to guess:

```text
ready      idle and inbox empty       -> may receive a standby item
has_mail   idle but unread mail       -> already fed; not dispatchable
working    the pane says it is busy   -> never interrupted
blocked    alive but cannot receive   -> named reason (draft / context full / paused goal)
dead       the agent session is gone  -> needs a reconnect, not an assignment
```

An `unknown` pane (unreadable screen) is reported as **blocked**, never as ready.

## What a tick reports

```text
idle lanes: 14
dispatched this hour: 0
blocked lanes: 0
dead lanes: 4 (usage-research, codex poolrad, 1, crackdown-lanes)
the standby queue is empty and 14 lane(s) are idle
```

That last line is the honest answer, and it is a finding about the **instruction
supply**, not about the lanes. A dispatcher that silently did nothing while
fourteen lanes sat idle would look identical to a fleet with no work.

The numbers come from `dispatch-state.json`, which the tick writes atomically and
keeps 24 hours of history in, so `dispatched this hour` is a real count and not a
per-tick count multiplied by twelve. The ops snapshot reads that file; it never
runs a tick.

## Rules the tests pin

- one item per lane per tick (a lane gets an assignment, not a batch);
- a dry run sends nothing **and consumes nothing** from the queue;
- a refused send (typing hold, dead session) is recorded with its error and the
  item stays queued - a failure must not lose an assignment;
- an item reserved for lane X is never handed to lane Y;
- a take is atomic: the same item cannot be dispatched twice, even by two ticks
  racing;
- `dispatched this hour` counts real sends and expires out of the window.

## Feeding it

```bash
dunkingsheep dispatch add t6 "advance strict C from 240 to 250 bodies" --task-id t6
dunkingsheep dispatch add t5 "crackdown lane A: A/B the frame budget" --lane wS:p2
dunkingsheep dispatch list
dunkingsheep dispatch tick --dry-run     # plan only
dunkingsheep dispatch tick               # what the timer runs
dunkingsheep dispatch status             # the last tick's numbers
```

A brief is one line. The lane is sent `[standby t6] advance strict C from 240 to
250 bodies`, so its reply can cite the task without guessing.

## Retired panes: silence with a reason

A pane can be superseded (`par-d2` started in `wW:p7` because `wW:p4` is stuck on a
full context) or replaced by machinery (the dispatcher took over the PM's feeding
job). Its mailbox then holds mail nobody will read, and paging about it every four
hours is an alarm that cannot be satisfied.

```bash
dunkingsheep retired retire wW:p4 --reason superseded-by:wW:p7     # superuser
dunkingsheep retired list
dunkingsheep retired unretire wW:p4
```

Retiring:

- **stops the page**: `inbox.unread_live` and `inbox.unread_dead` skip the mailbox,
  matched however the row spells it (`wW:p4`, `par-d (wW:p4)`, `par-d`);
- **stops the feeding**: the pane is excluded from the dispatcher's ready/idle
  counts;
- **keeps every message row** - the mail is the record of what was tried, and
  deleting it would destroy the evidence;
- **is still reported**: the watchdog emits an info digest line
  `retired panes: 1 (wW:p4)` every run, so a lane cannot vanish from the fleet
  unnoticed. Retired is a *state to report*, the same way dead is - not a
  condition to page about.

Both the reason and the actor are recorded; a retirement without a reason is
refused, because an unexplained silence is how an alert dies without anyone
learning. Unretiring is deliberate and reversible.
