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

## Dispatch on turn.end, not on a timer (pi lanes first)

Measured 2026-10-27: the last 20 turns lasted **33-448 seconds** while the keep-moving
dunks fire every **60-120 minutes**. So a lane was unprompted ~95% of the time *by
construction* — the timer was never the mechanism, it was the only thing we had. The
harness already reports `turn.start`/`turn.end` for pi/claude panes, and the daemon
received every `turn.end` and did nothing with it.

`turn_end` can now dispatch to an opted-in lane holding a live task claim, after
the shared readiness/mail gate passes. The following log is historical scratch
evidence from the original author, not acceptance of the corrected head on a live pi lane:

```text
2026-10-27 19:15:50 INFO dunkingsheep: turn.end %51: trigger=turn.end nudge DELIVERED task=t1 item=board:t1
2026-10-27 19:15:39 INFO dunkingsheep: turn.end %51: no nudge (trigger=turn.end reason=no_open_work)
```

**Claimed, not assigned** (CoS ruling, 2026-10-27): the trigger fires only for a task the
lane has **claimed** — `in_progress` with a live lease held by that pane — never for a task
merely assigned to it. An umbrella or programme task sits assigned-and-open for days, so
assigning it to a lane says nothing about what that lane is doing now; prompting on it would
make the trigger a loop in exactly the lanes that must stay quiet (reviewer, CoS, VP). The
same rule stops double-prompting: a task another pane has leased is that pane's.

**Correction to PR49's original policy:** a claim does not bypass the dispatch
allowlist. The turn-end path now uses the same configuration resolver and
`lane_plan` readiness gate as the timer path. The persistent file or environment
must opt the lane in; an empty allowlist feeds nothing. A one-tick `--lanes`
argument does not configure future turn-end events. Dependency readiness comes
from the task store's `blocked_by` field, including prerequisites absent from the
open-task view. The ended turn remains available as evidence of idleness.

Turn RPCs now require kernel-attributed caller PID/start-time ancestry under the
backend's pane process. A requested pane label is not authentication. Start
returns a random `turn_id`; end must present that token from the same stable
caller. Missing, foreign, expired, duplicate and old-generation ends cannot
dispatch or manufacture new idle evidence.

**Paired producer deployment is required (owner task t296).** The pi producer is
an owner-managed extension outside this repository. The reviewable
`integrations/pi/turn-generation.patch` updates its event function to serialize
commands, parse `--json turn start`, and pass the token with `turn end --turn-id`.
It does not edit or install anything automatically. The input extension SHA256 is
`5583a23a50d49dde58e139ff1664c5b608a61ddb96b81bef24d79fe6cecd8f7c`.
Review/apply it only against the intended owner file and deploy coherently with
the daemon. A legacy tokenless end is refused, leaving the hold until its TTL;
other legacy turn producers also require migration before using this contract.
Unavailable pane PIDs fail closed. Tests execute the patch's function with a fake
exec; the real pi reload and event-to-delivery path remain operator acceptance.

The policy is pure and lives in `dunk_dispatch.turn_end_plan` / `claimed_tasks`, so the
guardrails are readable and testable without a daemon:

| skip code | meaning |
|---|---|
| `no_open_work` | the lane has no CLAIMED task (assigned-but-unclaimed work does not count) |
| `unread_mail` | it already has unread mail — it is fed; a second assignment is noise |
| `unread_unknown` | the unread count could not be read, so "already fed" is unknown |
| `cooldown` | this pane was prompted under `DUNKINGSHEEP_TURN_END_MIN_GAP_S` (30 s) ago |
| `task_cooldown` | every task it has claimed was prompted under 5 min ago |
| `held_delivery_pending` | a prompt for this pane is still queued undelivered |
| `disabled` | `DUNKINGSHEEP_TURN_END_DISPATCH=0` |
| `not_allowed` | the lane is not in the persistent/environment dispatch allowlist |
| `unavailable_lane` | the lane is missing, retired, or has unknown activity |
| `blocked` / `dead` / `working` | the shared readiness gate prevents dispatch |
| `readiness_unknown` | readiness could not be established; nothing is sent |
| `dispatch_in_progress` | a concurrent turn-end dispatch already reserved this pane |

**Held is not delivered.** If the trigger fires but the guarded send would hold (a box
that will not clear, a dead session, an unconfirmed submit), the record says
`delivered: false` with the message outcome, the log line says it was held, and the
cooldown clock does **not** start. Counting a held send as delivered is the exact claim
this work exists to stop.

The cooldowns are loop guards, not a cadence, and both are configurable:
`DUNKINGSHEEP_TURN_END_MIN_GAP_S` (30 s per pane) and
`DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S` (300 s per task). They are deliberately **short**:
a long value would re-create the hourly-timer problem one layer down, and a cooldown can
only ever *suppress* a prompt. They are set when the send is confirmed during this
request. Later outbox completion does not currently update these clocks, so this is
not a hard hourly limit across delayed deliveries or daemon restarts. A different
claimed task is not suppressed by the task cooldown. Note the
trigger only ever evaluates **at a turn end** — it cannot wake an already-idle lane, so the
timer remains the fallback for that.

**The timer cadence is unchanged.** Raising it first would be the wrong order:
1,541 dunks recently held on an open dialog and 738 on a draft, so a faster timer would
only produce more blocked sends. The timer remains the fallback for lanes whose harness
reports no turns. The codex/opencode hook is **t264**, deliberately not this change, and
the same allowlist also gates this trigger. The continuation tests use a fake backend
and the real API-to-turn-end-to-send chain with timer ticks forbidden. Live pi
delivery and both skip observations remain a separate operator acceptance gate.

## Step 2 ordering: codex's turn socket first

Codex is the harness whose mail depends on a short hold: it does not queue typed text,
so a nudge to a busy codex lane waits for its turn to end. That makes
`turn/started` + `turn/completed` on codex's socket **load-bearing** for step 2 rather
than a refinement - its acceptance test is concrete: a nudge to a mid-turn codex lane
is delivered within N seconds of `turn/completed`, with the hold reason gone.

**t264 status (2026-10-28):** the producer code is in `integrations/codex/` and
`integrations/opencode/`, with tests.  Codex hooks only pass the ancestry check when
the lane is launched with `codex --no-daemon` (default shared app-server hooks are
detached; measured).  Existing lanes therefore need a respawn with that flag before
they report real transitions; until then their activity stays `unknown` and the hold
remains.  The trade is a per-lane app-server instead of the shared daemon (cross-lane
`codex agents` browsing is not shared); that is an owner/VP fleet decision, and the
memory cost was not measured.  OpenCode plugins run in-process and need only the plugin install.  Neither
producer falls back to a label, and a failed/expired start never produces an end.

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

## Nobody is fed unless a lane is named (the allowlist)

The tick has **no role concept**: it cannot tell a worker lane from a reviewer, the
chief of staff, or the VP's own pane. Until t227 that did not matter, because every
pane reported its status as `unknown` and the candidate list was always empty. Once
lane status is derived from a real signal, that same code would have become live for
**every** ready, mail-free agent lane at once - including the ones nobody wants
auto-nudged with board work.

So the allowlist is **empty by default, and empty means nothing is fed**. Three ways
to opt a lane in, in order of precedence:

```bash
# 1. one tick, explicitly
dunkingsheep dispatch tick --lanes %4,codex-worker

# 2. persistent, ~/.config/dunkingsheep/dispatch_lanes.json
{"lanes": ["%4", "codex-worker"]}

# 3. environment
DUNKINGSHEEP_DISPATCH_LANES=%4,codex-worker
```

Entries are pane ids **or** lane names; names are resolved to pane ids at tick time,
so a list of names survives a restart that renumbers panes. An unreadable config
feeds nothing (it fails closed and says so on stderr) rather than falling back to
"everyone".

The gate is still two conditions on top of this: a lane must be **ready** and have
**no unread mail**. The allowlist decides *who may ever be considered*; the gate
decides *whether, right now*.

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
