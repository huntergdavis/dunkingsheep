# Herdr removal: what the flip actually did, measured 2026-10-24 11:50 PDT

Written by the dunkies lane (herdr `w8:p8`, now tmux `%4`) after owner #1556 narrowed
t67 to "6-8 important lanes, then widen" and asked for a gotcha audit.

Everything below is a measurement with the command that produced it. No claim here
comes from reading code alone.

## 1. The fleet is FORKED, not restarted

`dunkingsheep panes` sees 6 tmux panes. `herdr pane list` still sees ~40 live herdr
panes, and the agent processes inside them are alive.

    agent processes: 45   attached to a live tmux pane: 5   in a live herdr pane: 40

Every tmux lane is a *fresh-session twin* of a herdr lane that is still running its
real session:

| lane | tmux pane | tmux session (started by the flip) | herdr twin | herdr session (the real history) |
|------|-----------|------------------------------------|------------|----------------------------------|
| cos | `%2` | `2026-10-24T18-04-46…` 124 KB | `w9:pS`, `w0:p1` | `2026-10-24T05-22-03…` 892 KB |
| dunkies | `%4` | `2026-10-24T18-07-18…` | `w8:p8` | `2026-10-17T17-24-02…` **19.9 MB** |
| vp | `%1` | (fresh, 11:04) | `w9:pH` | `2026-10-19T18-15-27…` 27.9 MB |
| par-a | `%3` | (fresh claude) | `wW:p6` | `b2f28115…` 32.8 MB |
| ports | `%5` | (fresh claude) | `wY:p2` | `c8ea3679…` 53 MB |

The flip started the twins with a bare command — `pi`, `claude --dangerously-skip-permissions` —
never `pi --session <recorded>` (`tr '\0' ' ' </proc/<pid>/cmdline` on all five).
`launches.json` still holds the recorded session for each lane, so the information to
resume exists; nothing used it.

Consequence: two incarnations of the same lane are live at once. For the dunkies lane
the overlap was 18:07:12 → 18:36:33 (both agents had the same mailbox and the same
tasks). The herdr twins are deaf — mail and dunks resolve on the tmux backend — so they
keep acting on their last instructions with no way to be corrected.

## 2. Dunks: 4 of 4 cannot fire

    d54 → wS:p1  LIVE=False  running=False  next=None
    d55 → w9:pH  LIVE=False  running=False  next=None
    d60 → w9:pH  LIVE=False  running=False  next=None
    d62 → wR:p5  LIVE=False  running=False  next=None

Two separate failures: the targets are herdr pane ids (no such pane on tmux), and no
dunker is running after the daemon restart (`status: Restored`, `next_send_at: null`).
A restore that spawns no timer is silent: nothing reports a dunk that will never fire.

## 3. The lane registry is one-way and stale

`lanes.json` has 72 entries, all herdr pane ids; **0 of the 6 live tmux panes appear in
it**. Resolution order (`DUNKINGSHEEP_MAILBOX` → lanes.json → mailbox named after the
pane) therefore resolves this lane — a fresh pi in `%4` whose title is `π - dunkingsheep` —
to **`vp of agentry`**, the wrong mailbox, because the daemon believes its own pane id is
`w9:pH`.

    $ dunkingsheep inbox count      →  vp of agentry: 0 unread of 2071
    $ dunkingsheep status           →  self_pane_id w9:pH   (this pane is tmux %4)

It is not only a miss, it is a cross-delivery: mail addressed to the vp lane lands in a
pane that is not the vp lane. The registry records pane → mailbox; nothing records
mailbox → live pane, which is the direction delivery needs on every backend flip.

Also measured: `TMUX_PANE=%4`, pane title `π - dunkingsheep`, cwd
`/home/hunter/workspace/dunkingsheep`, and `launches.json["w8:p8"].terminal_title` =
`π - dunkingsheep` — the title is the only field that agrees across backends.

## 4. agent_state captures the right panes and the wrong fields

    last capture 2026-10-24 18:48:17, 3475 rows, 666 without a session id

- Pane ids are correct for the current backend: recent rows key on `%1 %2 %3 %4 %5`.
- **Every tmux row has `session_id`, `model`, `thinking` empty.** The pre-flip rows
  (18:00:17) have them: e.g. `w9:pH → /…/2026-10-19T18-15-27…jsonl`, `w9:pM → bdbca7b7… (sonnet)`.
- So the capture is a *usable* restore source only up to the flip, and the newest
  capture is the least useful one. A restart that trusts "latest state" gets nulls.

## 5. Timers: one casualty, now loud

`systemctl --user list-timers`: dispatch / state / health / watchdog / watchdog-deadman
active, deepcheck daily. The dispatcher timer had been inactive 38 min after the flip and
only health's new staleness check named it (`dispatcher=stale last_tick=38.6m ago`); it was
re-enabled at 18:29. Nothing else in the unit set carries a tmux drop-in — only
`dunkingsheep.service.d/10-tmux-backend.conf` exists, so the flip never needed to touch the
other timers. `vp-herdr-webui{,-proxy}.service` are still active against a herdr that is
still running: that is the rollback path, not a leak, and it must be stopped (not merely
unused) for gate 5.

## 6. herdr is still running, so "without herdr" is not yet proven

    $ herdr status            → server: running, 0.9.5
    $ pgrep -af herdr         → herdr-real server, herdr-webui, auth proxy

Gate 5 asks for herdr *stopped*, not merely unused. With 40 agent processes inside herdr
panes, stopping the server is a fleet-wide action and has to come after every lane that
matters has a tmux home and exactly one surviving incarnation.

## 7. What the fix must do, in order

1. **Stop a lane from running twice.** For each lane in scope: exit-and-save the
   incarnation that will not survive, verify the process is gone and its session file is
   non-empty, then bring up the survivor. Never two at once.
2. **Decide the survivor by evidence, not by convenience.** Fresh+docs is a legitimate
   choice (the owner allowed it for the Poolrad lanes), but it must be chosen per lane and
   said out loud: a fresh twin has no history and cannot answer "what did you do before".
3. **Registry both ways.** Write `mailbox → current pane id` on every pane create/restore,
   and seed `pane id → mailbox` from it. A fallback is never recorded.
4. **Retarget dunks on restore**, and report a dunker that has a target no live pane
   matches instead of showing `running=False` in silence.
5. **Capture the session on tmux** (session id / model / thinking), so the *latest*
   capture is the one a restore can use.
6. **Only then** run the silence test with herdr stopped.

## 8. Rehearsal: exit-and-save then restore by session id, twice (scratch lane)

No live lane was touched. A throwaway tmux workspace was created by the daemon
(`new-workspace -l scratch-restore --cwd /tmp/ds-scratch -r pi`), and the restart was
driven only through sanctioned paths (`send` = submit_text, `read` = visible pane).

| step | action | evidence |
|------|--------|----------|
| 1 | start `pi` in the scratch pane | `%6 scratch-restore pi /tmp/ds-scratch` |
| 2 | give it a token `ZQ-7731`, ask for ACK | pane shows `ACK` |
| 3 | `/quit`, wait for the shell | pane command back to `bash`; session `…01a16f41…jsonl` 18667 B |
| 4 | restore `pi --session …01a16f41…jsonl` | process up in the same pane |
| 5 | ask "what token did I give you" | answers `ZQ-7731` — the transcript loaded |
| 6 | second cycle: token `QQ-4402`, `/quit`, same `--session` | same file, 21470 B |
| 7 | ask for both tokens in order | `ZQ-7731, QQ-4402` |

So: **same session id across two restarts, and one real turn executed after each** — the
t67 step-4 proof, on a lane where failure costs nothing. What the flip did instead was
step 1+4 only, with `pi` and no `--session`, and without step 3 at all.

Observed once and NOT explained: after the first restore the resumed agent printed
`dunkingsheep: failed: MCP connection closed` (its old MCP client died with the previous
process). It did not recur in the second cycle, but a lane that comes back without its
MCP tools is back without inbox/task/memory — worth a check after every real restore.

## 9. Gate 5 wording, and why the order matters

"herdr must not be in the path" was checked as *herdr not used*. Today herdr is
**running** with 40 live agent processes attached, so the honest statement is: the
daemon's backend is tmux, and a large part of the fleet is still on herdr. Stopping the
server now would take those processes with it. Dedup (section 7.1) has to complete for
every lane that matters first; only then is `herdr server stop` a measurement instead of
an outage.
