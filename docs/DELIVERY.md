# Delivery state: can this lane receive a message, and if not why

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
