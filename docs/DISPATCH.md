# The dispatcher: deterministic feeding, no model in the loop

Every lane finished its assignment, went idle, and nothing noticed. A human
re-fed four lanes by hand, twice. This is the structural fix: a standby queue and
a five-minute tick. **There is no model anywhere in it** - the gate is a pane's
delivery state and an empty mailbox, both deterministic reads.

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
