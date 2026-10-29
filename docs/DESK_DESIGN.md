# The Desk: dashboard design for the flat fleet

Companion to `FLAT_FLEET.md`. This is the product design for the web
experience that replaces the VP-of-Agentry desk. It is written before the code
so that the code can be reviewed against it.

## 1. Who it is for, and what they are doing

One user: the owner, usually on a tablet over the LAN or Telegram from a phone,
sometimes at the PC. Three jobs, in order of frequency:

1. **Ask and decide** (dozens of times a day): "how is throughput", "what is
   stuck", "is X done", and answering the questions lanes raise. Must be
   answerable in one glance or one sentence.
2. **Watch** (a few times a day): is the fleet burning through the backlog, is
   anything dead, is money on track.
3. **Play** (evenings): Pool of Radiance, with save states and the code wheel.

Not a job any more: managing managers. There is no "ask the VP", no relay.

## 2. Principles

1. **One question per screen.** Each view answers one question in its first
   viewport. Desk: "what do I need to decide, and what did I ask?" Operations:
   "is the fleet healthy and moving?" Terminals: "what is this lane doing right
   now?" Play: "the game."
2. **Measured, never narrated.** Every number on a screen comes from a daemon
   read with a timestamp and a source; nothing is a summary a model produced.
   If the desk agent wrote it, it is in a chat bubble, not a tile.
3. **Decisions first.** Anything that needs the owner is at the top of the
   first screen, with its one-tap answer. Nothing else may push it down.
4. **Status before chat.** Lane and task state is rendered from the queue, not
   described in prose, so "is it done" never depends on an agent remembering.
5. **Fast by construction.** Every panel is a single daemon call; polling is
   per-panel (3 s terminals, 5 s desk, 15 s operations); no panel blocks another.
6. **Calm.** No motion unless something changed. Alarms are the only red.
   Idle is the default visual state, so activity is visible.
7. **Phone first.** Every view works at 390 px with a 16 px gutter and no
   horizontal scroll; the tablet layout adds columns, the desktop adds density.
8. **Themes are skins, not layouts.** The 12 sci-fi themes stay as token sets
   (colour, type, radius); they never change structure, spacing or copy. A
   theme that fails the contrast gate (`THEME_CONTRAST_GATE.md`) is not shipped.

## 3. Information architecture

```
/dash/desk        Desk         default; decisions + owner conversation
/dash/operations  Operations   fleet health, throughput, review queue, budget, reports
/dash/fleet       Terminals    lanes, live screen, one-line send (owner only)
/dash/play        Play         Pool of Radiance
```

Top bar, one line: wordmark, four tabs, theme picker, the message dock button
(Ctrl+/). No eyebrow/subtitle band under it (owner #1803, #1808). The old
`/dash/cos` and `/dash/vp` redirect to `/dash/desk` for a month, then 404.
The live ttyd terminal (today `vp-terminal-webui`) stays as the "open full
terminal" link on the Terminals view; its session check moves into the web
process.

## 4. Views

### 4.1 Desk

Two regions on phone (stacked), two columns on tablet and up.

**Decisions I need** (top, collapses to a one-line "nothing waiting" when empty)
- Source: tasks with `status=blocked` and `block_reason` starting `needs-owner:`.
- Each card: task id, the lane's question verbatim, age, which task it blocks.
- Actions: a reply box and one tap "Answer"; the reply is posted as
  `task unblock --note "<owner reply>"`. A second tap "Not now" lowers priority.
- Nothing else may render above this region.

**Conversation** (the owner mailbox)
- The owner's messages and desk replies, newest at the bottom, 80% width
  bubbles (owner #1810), markdown rendered, attachments inline.
- Each owner message shows its delivery state from the daemon's receipt, not a
  guess: *sent -> seen by desk (t+2 s) -> answered (t+14 s)*. If no desk reply
  within 60 s, the bubble shows "desk slow" and the watchdog gets the event.
- Replies that filed a task show the task id as a chip that opens the task.
- Each bubble carries a small transport mark (web / Telegram). Replies go back
  only on the transport the question used; Telegram never receives web
  traffic, reports or digests, only answers to Telegram questions and the
  three page classes.
- The composer is the existing rich-text one (TipTap) with attachments. Enter
  sends; Shift-Enter newlines. The same composer is the message dock (Ctrl+/)
  on every other view, so the owner can ask from anywhere without losing the
  screen.

**Right rail on wide screens, below on phone:** "What I asked for" = tasks
created from the owner mailbox in the last 24 h with status chips (open /
in progress by whom / review / done), so "is X done" is read, not asked.

### 4.2 Operations

Answers "healthy and moving?" in the first viewport, then detail on scroll.

**Header strip** (four stat tiles, sparkline each, 15 s):
- Throughput: tasks completed last hour / last 24 h.
- Backlog: open by priority, oldest age.
- Review: PRs awaiting review, oldest age (the one number that proves "all
  code reviewed" is working).
- Lanes: working / idle / dead.

**Alarms** (only when non-empty; the only red on the site): watchdog rules that
fired, host warnings, dead lanes holding tasks, budget projection over.

**Decisions I need**: same component as the Desk, read-only link back.

**Review queue**: PR, repo, author lane, head, age, reviewer lane, CI state.

**Lanes**: table from the daemon roster: lane, role, current task, turn state,
last turn end, model/provider, today's cost estimate. Click opens the lane in
Terminals.

**Budget**: provider windows (claude, codex, pi/opencode-go) used/remaining %,
reset time, projection; subscription cost table. Unchanged from `ops_budget`.

**Reports**: the last five `dunkingsheep report` outputs, collapsed, newest
open. Pool of Radiance credited/replaced counts live here as a tile with the
ceiling word CEILING on the denominator (ground truth `ceilings`).

### 4.3 Terminals

Unchanged in substance from today's Fleet view, which the owner uses. Lanes
grouped by project, selected lane's screen (read-only, 3 s), one-line send box
that goes through the daemon's guarded `send` as the owner. Add the lane's
current task and role under its name. Remove "Contact the VP or chief of
staff" copy.

### 4.4 Play

Unchanged: DOSBox video/audio, keys, typing, save states, code wheel helper,
character editor. The "Direct line to the VP" box becomes the message dock
(same component as everywhere else), so the owner can ask the desk mid-game.

## 5. Components and tokens

- **Stat tile**: label, value, delta, sparkline, source+timestamp on hover.
- **Decision card**: question, age, blocked task, reply box, Answer / Not now.
- **Status chip**: open / claimed / in progress / review / done / blocked,
  colour from tokens, never from theme-specific CSS.
- **Lane row**: name, role badge, task chip, turn state dot (idle grey,
  working accent, dead red).
- **Message bubble**: owner / desk / report (reports are a distinct style so a
  machine-made digest is never mistaken for the desk agent's words).
- Tokens live in `ui/src/themes/<name>.json`; today each theme also ships a
  `<name>.css`, so "structural CSS references only tokens" is new work: the
  per-theme CSS files are folded into token sets and deleted. The contrast gate runs in CI for every
  theme on every PR that touches `ui/`.

## 6. Data contract (web -> daemon, socket only)

| Panel | Daemon command | Poll |
| --- | --- | --- |
| Decisions | `task list --status blocked --reason-prefix needs-owner:` | 5 s |
| Conversation | `inbox read --mailbox owner --no-mark` + `owner reply` | 5 s |
| What I asked for | `task list --created-by owner --since 24h` | 15 s |
| Stat tiles | `task metrics --window 24h --bucket hour` | 15 s |
| Alarms | `watchdog digest --latest` | 15 s |
| Review queue | `review list` | 15 s |
| Lanes | `views roster` | 5 s |
| Budget | `usage project` | 60 s |
| Reports | `report list --limit 5` | 60 s |
| Terminal screen | `pane read` | 3 s |
| Send | `send` (web role, as the owner) | - |
| Decision answer | `task unblock --note` (web role) | - |

The web process binds role `web` with its service token (FLAT_FLEET 4.3); it
is the owner's transport, so these calls are the owner's.

Where a command does not exist yet, it is added to the daemon (`FLAT_FLEET.md`
section 9.2, rule 1). The web process holds no state but sessions and the
viewport report.

## 7. Acceptance

- At 390 px and 1440 px, every view's first viewport answers its one question
  with no horizontal scroll (the 390/1440 capture pair from
  `tools/capture_served.py`, kept in `ui/tools/`).
- Decisions I need renders above everything on Desk and Operations.
- Every number on Operations has a source and timestamp in its tooltip.
- Owner message -> desk reply shows the three receipt states from daemon data.
- All 12 themes pass the contrast gate.
- `grep -r "subprocess\|sqlite3" web/` returns only the session store.
