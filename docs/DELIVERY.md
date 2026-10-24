# Delivery state: can this lane receive a message, and if not why

> *"inbox.sent ok is the MAIL; message.failed is the NUDGE. Two different events with
> two different meanings, and conflating them made the situation look worse than it
> was."*

> *"clear-box and flush read the PANE; send read a CLAIM about the pane. Both were
> right about their own evidence and only one was right about the world."*
> — the whole hold bug in one line: a report is a claim, the pane is the evidence,
> the claim is believed while fresh, and the evidence decides after that.

A lane can be alive, have unread mail, and be **unreachable**. Its input box holds
a draft that never submitted, so every later send is held as "a human is typing"
and the lane starves silently. From the outside that is indistinguishable from
busy, which is why "busy" was hiding four lanes at once.

`dunkingsheep delivery <pane>` answers the question directly:

```text
w8:p8   working - working
w9:pM   ready
wS:p1   dead    - dead session (automatic reconnect could not restore this session)
wS:p2   dead    - dead session (server connection could not be restored)
wW:p1   blocked - context full (/clear to save) - needs a /clear first
```

## The four states

| state | meaning | what to do |
|---|---|---|
| `ready` | idle, input box clear | send |
| `working` | the pane says it is working | wait; do not interrupt |
| `blocked` | alive but a send would be held | fix the reason, then send |
| `dead` | the agent session is gone; Enter can never submit | reconnect or replace the pane |
| `unknown` | the pane could not be read | find out; do not assume ready |

A **placeholder is not a state**. Claude shows `new task? /clear to save 497.4k
tokens` beside an empty composer, and it is drawn grey (not dim), so neither
colour nor position tells it from a real banner. Reading it as "context full"
made a healthy lane permanently unreachable while the dashboard showed it
present. The discriminator is the hint's own grammar (`new task? /clear to save
<n> tokens`, `type your request`): a hint never blocks, and a real draft sitting
next to the hint still does.

`blocked` reasons, each named because "busy" hides them:

- `unsubmitted draft in the input box` - the nudge that would not take
- `paused goal (.../goal resume)` - a codex pane that will never act on a message
- `context full (.../clear)` - the agent cannot take another task until it clears

## Where a state marker counts: the banner, not the scrollback

A dead pane prints its marker as a **status line**:

```text
■ Server connection could not be restored
```

A live pane can contain the same words inside a sentence - including the pane that
just wrote them into a file. The first version of this check scanned the whole
visible screen, so a live lane that merely *quoted* the phrase was classified dead;
in practice that was the pane doing the work. The rule is now:

- **dead**: a marker must START a line (after colour codes and banner glyphs);
- **paused / context full**: the marker must be on the composer, within the last
  three non-blank lines.

This is the same lesson as the ack scope: the guard has to match the shape the
thing actually has, or the guard becomes the outage.

## The hold: one predicate, an expiry, and a queue that survives a restart

The hold exists to stop us typing over a human. On 2026-10-23 it stopped the fleet
from talking at all, invisibly: every message to par-e was held for 8.5 hours while
that pane sat with an **empty prompt, no turn, and no output**.

Three fixes, and the shape of each is the same — *a remembered state needs an
expiry, and two checks that can disagree will*:

1. **`send_blocker()` is the single predicate.** `delivery_state`, the queued-message
   worker and `send_text` all ask it, so `delivery` cannot say `ready` while the
   queue says held. It returns the reason in the same words both surface.
2. **A `busy` report expires — and a WRONG claim is distinguished from a stale one.**
   Claude's session status is a claim about a pane made by another process, and it
   can be wrong in two different ways that produce the same symptom:

   | the claim | what it means | what happens |
   |---|---|---|
   | names a session whose **process is gone** | the claim is **wrong**, now | released immediately: `releasing the hold on wS:p1: the session named in the claim is gone (pid 3379009 is not running) - the claim is wrong, not stale` |
   | names a live session still saying busy, but the pane shows no turn and no draft | the claim is **stale** | released after `CLAUDE_BUSY_TTL_S` (600 s): `releasing the hold on wW:p5 after 10 min of 'busy' (TTL 600s): the report is STALE, not wrong - the session is running and still says busy, but the pane shows no turn and a clear box` |

   The pid in the claim is the difference, and it is why the two log lines read
   differently: the first is a bug in the reporter, the second is a reporter that
   cannot tell. Both are auditable at 3am without reading this file.
3. **The queue survives a restart.** Held messages are persisted with the daemon
   state and re-queued on start, and the restart says what it resumed:
   `resumed 1 held message(s) after restart: m1`. Before this, restarting to clear a
   stuck hold silently discarded every held message and three lanes were never told
   anything. **Losing mail is worse than delaying it.**

## Rule 15, second half: when a harness cannot queue, HOLD **and** CONFIRM

A harness's queue behaviour is a measured property, not an assumption
(`HARNESS_QUEUES_WHILE_BUSY = {claude, pi}`; everything else holds with a reason that
names the tool). The second half matters just as much: **the submit must be
confirmed**, because the confirmation is what makes the hold diagnosable instead of
mysterious.

Codex proved both halves in one night:

- it does not submit text typed during a turn, so the nudge is **held** (rule 15);
- the four-Enter confirmation detected that the text never left the box and logged
  `FAILED text did not submit; it is still in the pane's input box` **twenty times**,
  each with its message id - which is why the failure was a conversation with numbers
  instead of a lane quietly going silent for an hour.

And the ledger that kept the story straight:

| event | means |
|---|---|
| `inbox.sent ok` | the **MAIL** arrived in the mailbox (the row the lane will read) |
| `message.sent ok` | the **NUDGE** was typed and confirmed submitted |
| `message.failed` | the **NUDGE** did not submit; the mail is still there, unread |
| `message.held` | the nudge is waiting, by policy, with a reason |

**A mechanism that reports its own failures is worth the extra second it costs.**

### When the hold is wrong: `--ignore-typing`

If `delivery <pane>` says blocked but the pane in front of you is empty, read the
pane first, then send with the hold bypassed:

```bash
dunkingsheep delivery wW:p5                    # what does it think, and why
dunkingsheep send wW:p5 --ignore-typing 'text' # bypass the typing hold for this send
```

`--ignore-typing` skips the draft check for that one send. It is the documented
answer to "the hold says typing and the pane is empty" — **and the warning is the
point**: read the pane before using it, because the same flag will type over a real
human draft. This is a bypass, not a fix; if the hold is wrong, fix the reason.

## Getting a stuck lane moving again

Two different problems, two different commands:

| command | effect | when |
|---|---|---|
| `dunkingsheep flush <pane>` | presses Enter to **submit** what is in the box | the text is a message we want delivered and it simply never went in |
| `dunkingsheep clear-box <pane> --reason ...` | **Ctrl-U then Escape**: destroys the draft | submit failed (the nudge that would not take) and the draft now blocks everything |

`clear-box` is **superuser only**, is refused for a working lane (a clear must not
interrupt work), does nothing when the box is already clear, and logs every use
with the reason and both the before and after state:

```text
WARNING flushed pane wS:p2 by w9:pH (owner asked: nudge stuck): dead -> ready
```

Clearing someone's draft is destructive even when the draft is our own text, so it
is never automatic: the daemon reports `blocked` and names the command, and a human
or a superuser decides.
