# Design: Dunking Sheep

## Goal

Reproduce the Dunking Bird TUI feature-for-feature, deliver text through herdr's
socket API instead of the OS input layer, and (since 2.0) let agents control
every faculty through the same mechanism the keyboard uses.

## Components

```
dunkingsheep              CLI entry point: client commands, serve, tui, mcp, mcp-setup, guide
dunk_core.py              Flock + Dunker: state, timers, templates, gating, persistence
dunk_api.py               command registry: names, descriptions, JSON schemas, handlers
dunk_server.py            daemon: unix socket (JSON lines) around one Flock
dunk_client.py            FlockClient: socket client + daemon auto-start
dunk_mcp.py               MCP server over stdio (JSON-RPC 2.0, stdlib only)
dunk_help.py              the guide text shared by --help, guide, MCP and the socket
dunking_sheep_tui.py      curses view; every key is a registry command
herdr_client.py           thin wrapper around the `herdr` CLI (the transport)
text_editor.py            logical-line state for the full-screen text editor
run_dunking_sheep.sh      TUI launcher (ensures ~/.local/bin on PATH)
```

### One mechanism, four surfaces

```
 keyboard ──► TUI ──┐
 shell ──► CLI ─────┼──► FlockClient ──► unix socket ──► FlockServer ──► dispatch() ──► Flock
 agent ──► MCP ─────┘                                                      (dunk_api)
 anything ──► raw JSON lines ─────────────────────────────┘
```

`dunk_api.COMMANDS` is the contract. Each `Command` has a name, a description
written for an agent, `params` (JSON-schema properties with flat types only),
what is `required`, and a handler `(flock, args, self_pane_id)`. The socket
server calls `dispatch()`; the CLI maps subcommands onto command names; the MCP
server publishes every `mcp=True` command as a tool with `input_schema()`.
Adding a faculty means adding one `Command`; all surfaces pick it up, and the
`help` command / `dunkingsheep commands --json` document it automatically.

`self_pane_id` travels with every request because `target: "self"` must mean
the *caller's* pane (the agent's), not the daemon's. Clients default it from
`$HERDR_PANE_ID`, which herdr sets in every pane it spawns; the MCP server
inherits it from the harness that launched it.

### `dunk_core.Flock`

Owns the list of `Dunker`s and their timer threads. Thread-safe (one RLock for
state, one Lock serializing sends across dunkers, as in Dunking Bird).

- `add / update / remove / start / stop / toggle / stop_all / get / list`.
- `fire(id)` — the classic test send; synchronous by default so agents learn
  whether it worked. `send_now(target, text)` — one-off send, no dunk.
- `read_pane(target, lines)` — agents inspect another pane before deciding.
- `resolve_target(spec, self_pane_id)` — pane id, `self`, terminal id, or a
  unique case-insensitive substring of tab/workspace/agent/cwd/title. If several
  panes match but exactly one hosts an agent, that one wins; otherwise the error
  lists the candidates.
- Timer loop per running dunk: countdown against an absolute `next_send_at`
  (so interval edits and daemon restarts are honoured), optional idle gate
  (`herdr pane get` → `agent_status`, re-polled every 5 s while `working` or
  `blocked`), send under the shared `send_lock`, reschedule, persist, and
  self-remove when `max_sends` is reached. `stop_event.wait()` instead of
  `sleep()` makes stop immediate.
- Backpressure (`skip_if_unconsumed`, off by default). The idle gate cannot
  see unread input: a pane sitting at a prompt with text queued in its input
  box reports `idle`, so on 2026-09-12 a 240-minute self-dunk landed four
  prompts in one unattended Claude pane and they flushed into a single turn.
  The first fix inferred consumption from agent busyness and a verbatim tail
  match; both failed in the field (0b8a113): busyness on an unrelated turn
  latched the newest send as taken, and Claude Code collapses a long queued
  paste into a placeholder (`Pasted text`, `+N lines (ctrl+o to expand)`), so
  the verbatim needle was never present and every send scored as consumed. It
  never skipped once in production.
- The mechanism now reads the pane. At send time (after the idle gate, so the
  box has settled) `_previous_send_consumed` reads the target's visible screen
  and, via `_input_area` + `_input_pending`, isolates the input-box region as
  everything from the last prompt-sigil line (Claude `❯`, Codex `›`) to the
  bottom. That excludes the transcript, so a transcript-collapse chip like
  `+28 lines (ctrl + t to view transcript)` is not mistaken for queued input.
  The previous send is still pending if the box holds a collapsed-input marker
  (`COLLAPSED_INPUT_MARKERS`) or the sent text's whitespace-collapsed prefix.
  A pending box, an unreadable pane, or an unrecognisable box all hold the
  send: for this opt-in feature "not sure" means skip, because a missed nudge
  is recoverable next interval and a stack is not. A skip reschedules
  `next_send_at`, bumps `skip_count`, sets status `Skipped (unconsumed)` and
  leaves `send_count`, `last_sent_at` and `last_error` alone. Busyness is no
  longer consulted for consumption, and there is no extra polling thread: the
  one pane read happens inline at send time. Blind spots: it recognises our own
  queued send (verbatim, or the collapse placeholder a long one becomes) but not
  arbitrary short text a human left in the box, since agents render rotating
  idle hints there that are indistinguishable from typed text; and it assumes an
  agent pane that draws an input box, not a bare shell.
- Never type over a human (`hold_while_typing`, on by default). A dunk that
  lands while the human is mid-sentence splices its text into their draft and
  Enter submits the mangled result; seen live on 2026-09-19 in a Claude pane
  whose transcript reads `...in priorit yoPM poll: check list_panes...`. The
  blind spot above ("hints are indistinguishable from typed text") is only
  true in plain text: read with `herdr pane read --format ansi`, both Claude
  Code and Codex draw their empty-box hints dim (SGR 2, including Claude's
  dim prompt suggestions and Codex's rotating `Use /skills ...`) and typed
  text at full intensity. `_typed_text` finds the last prompt-sigil line,
  keeps only its non-dim characters (a small SGR walker that skips the `2` in
  `38;2;r;g;b`), strips the sigil and box decoration (braille animation dots,
  rules) and returns the draft; '' for an empty or hint-only box, None when no
  box is drawn. Only the sigil line is judged: every draft starts there, and
  continuation lines are hard to tell from an agent's footer. `_is_our_send`
  excludes our own leftover text (the visible line is a prefix of the last
  send, or a collapsed-paste placeholder while `awaiting_consumption`), which
  stays backpressure's business. In the timer loop the gates form one loop:
  idle gate, then typing check; while typing, status `Held (typing)`,
  `hold_count` +1 once per hold, re-check every `TYPING_RECHECK_S` (60 s), and
  after the box clears the idle gate runs again because finishing typing
  usually means submitting. Unreadable pane or no box: never hold, since this
  guards the human's draft rather than the agent's queue. Since 2.3.0 `fire`
  and `send_now` respect it too, via the message queue below.
- Operations telemetry (2.13.0). Two durable stores already existed but
  neither answered "how is delivery going": `inbox.db` knows when mail was
  written, read and acknowledged, and `dunks.json` keeps cumulative counters
  per schedule. Everything about the typing pipeline - how long a send waited,
  how many Enter presses it took, what blocked it - lived only in memory and
  in an unstructured 5 MB log, and was lost on every restart. `dunk_metrics`
  adds an append-only event store in its own database, so telemetry retention
  is independent of mail and can be pruned or dropped without touching
  anything a person is waiting to read. Ten hooks record where a real event
  already happens; nothing is inferred or back-filled. `metrics_summary`
  returns a versioned, self-describing document carrying its own interval,
  units and definitions. Two deliberate choices: a duration is null rather
  than zero when nothing measured it, with the names listed in
  `coverage.missing`, and `coverage.events_since` states when recording began
  so a window reaching further back is reported as partly covered rather than
  quietly empty. Cumulative per-schedule counters are kept in their own block,
  since reading them as a rate would be wrong. Privacy is enforced by a test
  that asserts no body, subject or address can reach the output.
- Global task system (2.14.0). Coordination is an algorithm, not a
  conversation. `dunk_tasks.py` stores tasks and append-only task events in
  their own SQLite database; `task_leases` carries one row per live claim.
  Scope is `global` (fleet-wide) or a pane id (private to that pane), and
  per-pane privacy is enforced in `Flock` before the store is touched. The
  atomicity the fleet depends on is SQLite's: `claim_next` runs `BEGIN
  IMMEDIATE`, selects eligible tasks, checks `depends_on`, writes the lease and
  flips the task to `in_progress` in one transaction, so two agents polling at
  the same instant cannot win the same exclusive task. A lease is a claim with
  a deadline: `task_heartbeat` extends it, and a reaper requeues one whose
  lease expired or whose pane disappeared from herdr, so an agent that dies
  does not strand work. `task_follow` turns pull into push: the daemon checks
  the same `agent_status` idle gate that `only_when="idle"` uses, claims the
  next task for an idle follower, and delivers it through the guarded inbox
  notice, so the agent never polls. `task_metrics` computes throughput, queue
  depth, per-actor load and completion/claim latency from the event log; no
  model is in any of it.
- A notice that says what arrived (2.12.0). The nudge used to say only how
  many messages were unread, which told a recipient nothing about whether to
  stop what it was doing. It now names the inbox (an agent may be asked about
  more than one), the sender and the subject, while staying one short line,
  because a short send is the one that always submits and the content itself
  never has to be typed.
- The daemon was answering dialogs (2.11.0). Two faults compounded. The modal
  check scanned the whole visible screen, so an approval prompt that had
  scrolled up into the transcript still read as a live dialog long after it
  was answered: four ordinary Codex panes were flagged modal at once. And
  2.9.0 pressed Escape at a "stale" panel to get the pane moving. Together
  they pressed Escape into working agent panes 1042 times. On a Codex approval
  prompt Escape chooses "No, and tell Codex what to do differently"; in a
  working pane it interrupts the turn. That is the likeliest source of the
  aborted turns reported from the field, and of sends refused on panes that
  were not actually blocked.
  `_modal_open` now reads only the last `MODAL_TAIL_LINES` of the screen, which
  is the live interface; anything above is transcript. The dismissal is gone
  entirely: `_note_stale_panel` logs once that a dialog is waiting for a
  person and returns False. A dialog is a question put to someone, not a state
  to resolve, so the daemon types nothing and presses nothing there and
  delivery resumes on its own once it is answered.
- A notice orphaned by a restart, and the idle gate default (2.10.0). A PM
  pane held an `[inbox] 1 unread` line in its composer for ten hours and its
  hourly dunk never recovered. The log is unambiguous: the daemon restarted at
  21:02, and 30 seconds later wrote "input box has held text we did not type
  for 30s with messages waiting. Not submitting someone's draft". The text was
  ours - we had typed it minutes earlier - but `sent_text_by_pane` lives in
  the process, so the restart erased the record and the backstop correctly
  refused to submit what then looked like a draft. `_looks_like_ours` now
  recognises an inbox notice by its fixed prefix as well as by memory, which
  survives a restart because no person writes that line. The dunk timer loop
  also gained the stranded-text recovery the outbox has had since 2.6.0; it
  had none, which is why the dunk held rather than escalating.
  Separately, a one-minute schedule sent into active Codex turns and aborted
  them. A new schedule aimed at a pane with a detected agent now defaults to
  `only_when="idle"`, since typing into an agent mid-turn costs that turn's
  work silently. Existing schedules keep whatever they were given, a pane with
  no agent is left ungated, and `only_when="any"` opts out.
- A panel blocks a pane for ever (2.9.0). A VP self-dunk on a one-minute
  interval held 34 consecutive times over 34 minutes, and the outbox beside it
  looped between "box clear, delivering" and "draft appeared, not typing" at
  the recheck interval. None of the diagnostic lines added in 2.8.1 and 2.8.2
  appeared in that window, which left exactly one silent `return False` in
  `_ensure_box_clear`: the modal branch. A panel had been open on the pane the
  whole time. `_typing_in` correctly says a panel is not a draft, so
  `_clear_to_send` kept reporting the pane free, while `_ensure_box_clear`
  correctly refuses to type into one - a disagreement with nothing to break
  it, since `_unwedge` only submits text it recognises as ours and a panel is
  not text. The pane recovered only when an owner message dismissed the panel.
  `_box_signature` now distinguishes a panel from a draft, and
  `_dismiss_stale_panel` presses Escape - the key the panel itself advertises
  - once one has been open and unchanged for `MODAL_WAIT_S` with something
  waiting. A panel being navigated keeps changing and is left alone, and a
  draft is never a panel, so draft protection is untouched. The dunk timer
  loop gained the same recovery; it previously had none at all, which is why a
  dunk could hold indefinitely while only the outbox had a backstop.
- Transient drafts and honest notify state (2.8.2). After the livelock fix a
  notice to a Codex pane was still refused 14 times in 82 seconds while the
  pane showed only its dim placeholder. Sampling `_typed_text` on that pane
  492 times over two minutes found no false positive, so the artifact is
  sub-second: an agent redrawing its pane can put status text on the prompt
  line for a frame. `_clear_to_send` has always taken two readings a second
  apart for exactly this reason; `_ensure_box_clear` took one, and a single
  unlucky sample refused the send. `_draft_persists` gives it the other half
  of that pair, so a draft must survive `DRAFT_CONFIRM_S` to hold a send,
  and the refusal now logs what it saw so the next case needs no guesswork.
  Separately, `inbox_status` reported notify=queued for ever: the record was
  written when the notice was created and never updated. A notice now carries
  the inbox ids it speaks for (`Message.notify_for`, appended to when notices
  fold) and `_deliver` writes the real outcome back.
- The worker livelock (2.8.1). A VP inbox notice was stored at 07:40 and not
  read until 08:07, and only because the daemon was restarted. The log shows
  the worker alternating, about once a second for the whole 27 minutes,
  between "box clear after 1s, delivering" and "draft appeared while waiting
  for the send lock; not typing". Two checks disagreed about the same box:
  `_clear_to_send` forgives our own stranded text (it is ours to clean up),
  while `_ensure_box_clear` only forgives it if it can be flushed, and the
  flush was failing. The refusal branch then looped straight back with no
  sleep, so it never reached the stale-box recovery further down, which is why
  the 150s backstop never fired. The refusal now falls through to that
  recovery, and `STALE_BOX_S` drops to 30s, which bounds how long an agent
  waits on a notice. The backstop still only ever submits text we typed, so a
  human's draft is untouched by the shorter window.
  Notices also no longer stack: `_refresh_queued_notice` updates the notice
  already waiting for a pane to the new unread count instead of queueing a
  second one, since an agent that reads its inbox once has handled all of it.
- Panels, dialogs and pane independence (2.8.0). A Poolrad worker's `/usage`
  read was reported as a daemon timeout. Two causes, both reproduced live.
  First, a slash command replaces the input box with a panel, and a dialog
  draws its selected row with the same sigil the prompt uses, so both read as
  occupied input. `/usage` had actually run on the first Enter; the daemon
  then spent three more presses on the open panel and reported the send
  failed, and a follow-up Escape queued for ever behind it. `_modal_open`
  recognises a panel by the phrases only a panel shows ("esc to cancel",
  "enter to confirm"). A send whose text opened a panel counts as submitted;
  `_typing_in` no longer calls a panel a draft; text waits for a panel to go
  rather than typing into it, while key presses pass through, since keys are
  how a panel is dismissed (Message.kind distinguishes them).
  Second, `send_guarded` held one global lock across the whole
  confirm-and-retry sequence, which runs for tens of seconds when an agent
  will not take Enter. Every other pane's send queued behind it, and a caller
  waiting on the daemon saw a timeout. Locks are now per pane, so panes are
  as independent as they look; sends to the same pane are still serialised.
- Mailbox identity (2.7.1). The first cut keyed a mailbox on
  `pane["agent"]`, which is the agent *type*, not one agent. Every Codex pane
  therefore shared one mailbox: a message addressed to the Poolrad PM was
  filed under `codex` and readable from the website and worker panes too.
  `_mailbox_for_pane` now takes the pane's own label, then its tab label, then
  its pane id, accepting each only if exactly one pane carries that value, so
  the key always identifies one pane. A spec matching several panes raises
  `AmbiguousTarget` rather than pooling them, and a bare agent type is refused
  by name. Messages an older version filed under a shared mailbox cannot be
  attributed to a recipient, so they are not guessed at: on startup they are
  moved to `unrouted:<type>`, which no pane resolves to, logged, and reported
  in `status` for review.
- Inboxes (2.7.0). Every failure this module has fought comes from one
  choice: delivering machine-to-machine messages by typing into the human's
  input box. Enter does not reliably submit, long pastes strand, a send can
  collide with someone mid-sentence, and a stranded box parks a queue. An
  inbox removes the transport instead of patching it. `dunk_inbox.Inbox` is a
  SQLite store in the config directory: a message is a row addressed to a
  mailbox, read by the recipient through the same three surfaces as everything
  else. It is durable across restarts (the typing outbox is in memory and has
  lost messages to restarts twice), ordered per mailbox with read state,
  unbounded in size, and carries attachments stored by content hash.
  Mailboxes key on the agent's herdr name rather than its pane id, because a
  name survives a pane moving or restarting; a name with no live pane is still
  a valid mailbox, so mail can be left for an agent that has not started.
  The only thing ever typed is one short nudge line on send, which is the one
  case that always submits, and `notify=false` skips even that. Every send
  returns a stable id; `inbox_status` reports queued / read / acked /
  cancelled with destination, sender, timestamps, size, attachment count and
  reason, and deliberately carries neither body nor subject so a status board
  can poll it without seeing the contents. Read, acked and cancelled mail is
  purged after a week; unread mail is never purged by age. A parity test now
  asserts every registry command is reachable from the CLI, since the CLI
  mapping is hand-written while the socket and MCP surfaces are generated.
- The queue must never wedge (2.6.0). Recognising our own stranded text
  covers nearly every case, but one mis-read parked a pane's whole queue: a
  dunk's checkpoint sat in wN:p1 for 37 minutes with three messages behind it,
  needing a human to run `flush`. `_outbox_worker` now watches what the box
  holds while it waits. Text that keeps changing is someone writing, and is
  left alone; text unchanged for `STALE_BOX_S` while messages wait is
  stranded, so `_unwedge` submits it and the queue moves. Nothing is unwedged
  when no message is waiting, so an idle pane with a draft in it is untouched.
  Also fixed here: `_box_holds` conflated "pane unreadable" with "pane has no
  input box", so after the stricter confirmation rule every send to a bare
  shell reported failure. Only an unreadable pane is inconclusive now.
- Guard recursion incident, 2026-10-11 (2.5.0). Shadowing was first written so
  that `install()` did it implicitly, and a guard test called `install()`
  without sandboxing PATH. That renamed the real `~/.local/bin/herdr` to
  `herdr-real` and put the wrapper in its place at 08:30, under a daemon that
  had been running since the previous evening. `HerdrClient` resolves the
  binary once at construction, so the daemon kept calling the old path, which
  was now the wrapper, and every internal send became a fresh
  `dunkingsheep send` that timed out after 8 s: 39 messages queued and four
  panes stranded. Recovery was to move the real binary back in place, which
  needs no restart because the daemon holds the path, not an inode; the queue
  drained on its own and the stranded text was flushed. Three sends were lost
  to timeouts and resent. Two changes make the class impossible: `_run` now
  passes `DUNKINGSHEEP_GUARD=1` to every herdr invocation, so a wrapper that a
  daemon calls execs the real binary instead of routing back; and shadowing is
  explicit (`install(shadow_binary=True)`, i.e. only the CLI asks for it), with
  every guard test pinned to a sandboxed PATH that has its own stub herdr.
- Universal confirmation (2.5.0). Direct agent-to-agent messages were still
  sticking while dunks were fine, for two reasons. First, `send_keys` pressed
  Enter once and reported success without reading the box, and agents that
  type with `pane send-text` and submit with a separate `pane send-keys Enter`
  therefore had no retry at all; `Flock.press_keys` now routes an Enter meant
  to submit waiting text through `_press_until_submitted`. `_after_create`
  (the `command=` argument on create_workspace / create_tab / split_pane) went
  through the unconfirmed `herdr pane run` and now uses `submit_text` too.
  Second, and the real cause: the guard was reaching nobody. Every agent pane
  was checked and all nine were unguarded, including two started that morning,
  because herdr starts agents by exec'ing them from its server process, so
  they inherit the server's environment rather than a shell's. A PATH entry in
  ~/.bashrc can never reach them. `dunk_guard.shadow()` therefore renames the
  real binary to `herdr-real` and takes its place, which covers every caller
  including agents already running. A `dunkingsheep-herdr-guard` marker in the
  wrapper lets `_find_herdr` and the wrapper itself tell the two apart, so the
  daemon never recurses into its own guard. `herdr update` will overwrite the
  shadow, so `guard status` reports whether it is in place.
- Confirmed submit (2.4.0). Messages were arriving stuck in the input box, or
  seeming to send too early. Measured on a live Codex pane 2026-10-10: a
  ~2.2k-character message stayed in the box 3 times out of 3; raising the
  pre-Enter delay from 0.2 s to 1 s and 2 s did not help (3/3 stuck each), and
  neither did shrinking the paste chunk from 900 to 300 or 120 characters
  (3/3 each). A second Enter submitted it every time. Claude Code submitted
  single-line, multi-line and 2k messages first time. So this is not a timing
  race and not the paste-chunk placeholder: a large input simply needs another
  Enter. A separate shell-pane probe showed that a newline inside the text
  executes immediately in a bare shell, though agent TUIs take it literally.
  `Flock.submit_text` therefore types, then `_press_until_submitted` presses
  Enter and reads the box back (`_box_holds`), pressing again only while our
  own text is still there, up to `SUBMIT_ATTEMPTS` (3) with a backoff poll.
  A blind second Enter was rejected as unsafe: if the first worked, the second
  would submit whatever the human typed next. Follow-up measurement showed
  what actually decides it on Codex is how long the pane has been *quiet*, and
  that the requirement scales with the paste: pressing Enter again immediately
  never worked, while escalating gaps (`SUBMIT_GAPS` = 0.2, 1, 3, 6 s) gave
  3/3 submits at 2.2k. Cost on Codex: 0.4 s short, 4.2 s at 540 chars, 10 s at
  1k, 18.9 s at 2.2k; Claude is ~1 s at any size and always submits on the
  first press. `flush_box` submits text the daemon did not type, which would
  otherwise block that pane's queue for ever. Text that never leaves the box
  is reported failed, so `send_count` and message status stay honest.
  `Flock.last_text_by_pane` remembers what we last typed into each pane, so
  text stranded by a failed submit is recognised as ours: `_typing_in` does
  not mistake it for a human draft (which stalled that pane's queue for ever)
  and `_ensure_box_clear` flushes it with Enter before the next send instead
  of appending to it. The test double now models the box for real, with
  `enters_needed` to reproduce the Codex case.
- Closing the send race, and the two ways around the protection (2.3.1). On
  2026-09-23 an agent's message still landed in the middle of a sentence.
  Red-teaming found the detector innocent (it classifies idle, working,
  typing and queued-message states correctly on live Claude and Codex panes)
  and three real holes, all reproduced:
  1. **The race.** `_clear_to_send` returned, then the send waited on the
     *global* `send_lock` that every pane's send shares, then typed. On a busy
     flock that gap ran to seconds. Reproduced live: check clear, wait 2 s
     while a human types, send, and the pane received
     `I START TYPING RIGHT HERE and keep goingAGENT-MESSAGE-INTERLEAVED`.
     Fix: `Flock.send_guarded` takes the send lock, re-reads the box *under*
     it, and only then types; the remaining window is one pane read (~40 ms).
     A refused message keeps its place in the queue; a refused dunk goes back
     to holding. `_deliver` returns False in that case, so the outbox worker
     does not drop it.
  2. **The passthrough.** `herdr(command=...)` happily ran `pane send-text`,
     `pane run`, `agent send` and `pane send-keys`. The daemon log shows
     `herdr pane send-keys wH:p7 Enter` going through it: a bare Enter submits
     whatever half-written sentence is in the box. These four are now refused
     with a pointer to `send_text` / `send_keys`.
  3. **Agents shelling out.** The dominant path in the evidence: the pane
     transcripts are full of `Bash(herdr pane run wH:p7 "...")`, which never
     touches this daemon. `dunk_guard.py` installs a `herdr` wrapper first on
     PATH that forwards those four subcommands to the daemon and execs the
     real herdr for everything else. Recursion is blocked from both ends: the
     wrapper sets `DUNKINGSHEEP_GUARD=1` when calling back in and re-execs the
     real binary when it sees it, and `_find_herdr` skips the guard directory.
     The wrapper falls back to the real herdr if the daemon is unreachable, so
     a broken guard can never make herdr unusable.
  `send_keys` and `send_text(press_enter=false)` exist so the wrapper can
  mirror `pane send-keys` and `pane send-text` semantics exactly.
- Direct-message queue (2.3.0). `send_text` used to type immediately, so an
  agent messaging a pane could splice itself into a human's draft even though
  scheduled dunks would not. Messages now go through a per-pane outbox in the
  daemon: `Flock.send_now` builds a `Message`, `_enqueue` appends it to
  `outbox[pane_id]` and starts one delivery thread per pane, and
  `_outbox_worker` delivers in FIFO order, each only once `_clear_to_send`
  agrees. While someone types it re-checks every `MESSAGE_RECHECK_S` (5 s,
  against a dunk's 60 s, because a message is waiting to be delivered rather
  than scheduled for later). `wait_s` lets a caller block for delivery;
  `hold_while_typing=false` bypasses the queue. `fire_dunk` uses the same
  outbox when its dunk holds while typing, with the dunk id as the message's
  `source`, so a test send no longer bypasses the protection. `list_messages`,
  `get_message` and `cancel_message` expose the queue plus a bounded history
  (`MESSAGE_HISTORY`, 50). The queue is in memory only: a daemon restart drops
  what is waiting, which is the right trade for "deliver this now-ish".
- Typing settle window (2.3.0). `_clear_to_send` replaces a single typing
  check: the box must read clear, then still read clear `TYPING_SETTLE_S`
  (1 s) later. A pause between thoughts no longer looks like being finished.
  Dunks pass their `stop_event` so a stop during the settle wait returns at
  once. `_typing_in` is the pane-level form of the old `_human_typing`, which
  now delegates to it.
- herdr layout control (2.2.0). The same registry that schedules dunks can
  build the herd: `list_workspaces`, `create_workspace`, `create_tab`,
  `split_pane`, `start_agent`, `rename`, `focus`, `close`, and a generic
  `herdr` passthrough plus `herdr_help`. `HerdrClient` gained thin wrappers
  (`_run_result` parses herdr's JSON, including the JSON errors herdr prints
  on stderr, into `(ok, result-or-message)`); `Flock` resolves workspace and
  tab targets with the same id / exact label / unique substring / `self` rules
  as panes (`_pick`), runs an optional command in a freshly created pane via
  `herdr pane run` (verified to work immediately after creation), and turns
  herdr refusals into `DunkError`s. `start_agent` uses `herdr agent start` so
  herdr registers the process under the given name and tracks it. `close`
  refuses the caller's own pane / tab / workspace (a process closing its own
  terminal is almost always a mistake) and stops dunks whose target pane was
  inside. The passthrough shell-splits the command, maps `self` to the
  caller's pane id, returns parsed JSON or usage text (a bare group's usage is
  an answer, not an error) and refuses only what would stop, replace or attach
  herdr itself (`server stop`, `update`, `channel set`, `session`,
  `completion`, `agent attach`, `integration`, `--session/--remote`). Nothing
  here is persisted; herdr is the source of truth for layout.
- Templates: only the known placeholders are substituted (regex), so JSON or
  code braces in a prompt survive intact.
- Persistence: `dunks.json` written atomically on every structural change and
  after every send (`send_count`, `next_send_at`). `Flock.close()` halts threads
  but keeps `running=True` so the next daemon resumes them; an overdue send
  fires after a short grace period.

### `dunk_server.FlockServer` and `dunk_client.FlockClient`

Protocol: newline-delimited JSON, many requests per connection,
`{"id", "cmd", "args", "self_pane_id"}` → `{"id", "ok", "result" | "error"}`.
Errors are always replies, never dropped connections. Binding reclaims a stale
socket file (nobody listening) and refuses when a live daemon holds it.
`shutdown` replies first, then stops; SIGTERM/SIGINT do the same.

The client opens a connection per call (cheap on a unix socket, no reconnect
logic) and `connect_or_start()` spawns `dunkingsheep serve` detached
(`start_new_session`, output to `daemon.log`, `HERDR_PANE_ID` scrubbed so the
daemon never thinks it *is* a pane) and waits for the socket.

### `dunk_mcp.McpServer`

A hand-written MCP stdio server: newline-delimited JSON-RPC 2.0, `initialize`
(negotiates 2024-11-05 … 2025-11-25 and returns the guide as `instructions`),
`tools/list`, `tools/call`, `ping`, and empty `resources/prompts` lists so
harnesses that probe them are happy. Tool failures are `isError: true` results
with the `DunkError` message, never JSON-RPC errors, because agents can read and
recover from the former. `structuredContent` is included only for protocol
versions that define it. Schemas use only string/number/integer/boolean (no
unions, no null enums) so OpenAI/Codex and Gemini validators accept them.

### `dunking_sheep_tui.DunkingSheepTui`

Now a client. Each frame (200 ms) it fetches `list_dunks` and redraws; keys map
1:1 onto registry commands (`a`→`add_dunk`, `d`→`remove_dunk`, `space`→
`toggle_dunk`, `c`→`update_dunk(target)`, `t`→`fire_dunk(countdown_s=2,
wait=false)`, `i/e/n/o/u/m`→`update_dunk`). `q` closes the view; `Q` sends
`shutdown(stop_all=true)`. If the daemon cannot be started at all, the TUI falls
back to an in-process `Flock` (clearly labelled, not shared) so it still works.
Modals (line input, workspace-grouped target picker, logical-line text editor)
are unchanged from 1.x.

## Why a daemon

Agents create dunks from processes that come and go; a dunk that dies with its
creator is useless as an external control loop. Putting the flock in a daemon
also makes the TUI optional, lets several surfaces see one truth, and makes
persistence natural.

## Preserved behavior details

- Interval parsing with a helpful error; live `Next: MM:SS` countdown.
- Test send with a 2-second countdown from the TUI.
- Full-screen logical-line text editor (Ctrl+G save / Esc cancel).
- `send_lock` serializes sends across dunkers; chunked `send-text` and a settle
  pause before `Enter` (see `herdr_client.py`) so Codex/Claude submit reliably.
- Defensive `safe_addstr` / `clip` so a small terminal never crashes curses.

## Non-goals

- A TCP/HTTP transport. The unix socket is local by design; the MCP server is
  the remote-friendly surface (harnesses spawn it over stdio).
- Exposing `shutdown` to agents.

## "It compiles" is not "it runs"

A gate that proves a module **binds** proves nothing about **execution**. On
2026-10-22 an undefined Unicorn register constant reached `main` in the poolrad
gate because `check_checker_builds.py --compile-only` compiled that module without
executing a single hook; the `NameError` only fired inside a real audit hours
later (#1437).

Our own surface had the same hole: the CLI parity test greps the help text, which
proves a name is *spelled*, not that a handler *works*. `tests/test_command_smoke.py`
closes it by executing **every** command in the registry against a fixture fleet:

- a command with no required parameters must **return a result**;
- a command that needs parameters must refuse **with words**;
- no command may raise `NameError`, `AttributeError`, `TypeError`,
  `UnboundLocalError` or **`KeyError`** — a handler that indexes `args["target"]`
  for a key the `Command` does not declare `required` is a schema that lies about
  its needs, and it reaches a user as "internal error".

### The two lies it found, on the first run

Both were commands the night depended on, and both were invisible to every
spelling-based check we had:

| command | what the registry claimed | what it actually did |
|---|---|---|
| `clear_box` | "DESTRUCTIVE, superuser only: clear an unsubmitted draft" | raised `KeyError: 'target'` when run against a blocked pane — the one command that unblocks a lane crashed on the lanes that needed it |
| `delivery` | "Can this lane receive a message, and if not why: ready / working / blocked / dead" | the printer read the clear result as if it were a message, and `target` was never declared required, so an empty call crashed rather than answering |

The registry said what they *were for*; no test said they *ran*. **A gate that
found two lies in one run is a gate that should have existed three hours earlier** —
those two commands were written and shipped in the same window as the incidents
they were meant to fix.

The gate is proven to fail: `test_the_gate_would_catch_an_unimported_name` registers
a handler that reads an unimported name and asserts the smoke gate reports it.

### The general form

> **A check that proves SPELLING is not a check that proves FUNCTION.**

Grepping a help text, importing a module, compiling a file, listing a registry —
each proves a **name exists**, and none proves a handler **runs**. A test has to
**execute** the thing and assert a **result**.

And the clause that turns a smoke test into a real one: *a command with no required
parameters must return a result, not a polite nothing*. A handler that returns `{}`
and calls it success passes every weaker assertion, so the test rejects `None`, `{}`,
`[]` and `""` explicitly.

Corollary, from the same message: a **repair** that changes no claim carries a
dated note beside the evidence, not a rewritten document — a rewritten doc reads as
if the claim had always held.

## A rule tested only in its happy path may be impossible in its only case

`supersede()` is the *only* mechanism by which a context entry may change. It
derived the same slug as the entry it replaced — same day, same subject — so the
new file collided with the old and it refused with "entry already exists". The
mechanism was documented, tested on a convenient case (a *later* date), and
impossible in the only situation it exists for. **A rule that is documented,
happy-path tested, and impossible in practice is worse than no rule**: it reads
like a guarantee.

> **When a mechanism exists for one purpose — supersede, retire, rollback,
> migration, restore — test the case it exists FOR, not a convenient neighbour.**

The census is in **`docs/MECHANISMS.md`**: one row per one-purpose mechanism, each
stating *what its only case is in one line*, the test that exercises it, and its
state. If the case cannot be stated in one line, we do not know what the mechanism
is for — and no test can be right. `tests/test_mechanism_census.py` enforces the
file (one-line case, cited test exists, mechanism exists in code, every declared
one-purpose mechanism has a row), so the census cannot drift into a document nobody
updates.

Applying it the same night found one more: `memory_backfill` exists to enqueue the
whole corpus *resumably*, and its nearest test exercised `enqueue`, not the
command. The case that matters is "run it again on a corpus that was partly done",
because a backfill that is not idempotent re-ingests 14 GB. That test exists now,
alongside the same-case tests for `clear_box` (blocked on a full context — the
reason it was built) and `retire` (a pane that no longer exists — which is what
we retire).

## A mailbox name is a conversation; a pane id is a process

On 2026-10-22 a message addressed to `codex-poolrad2 (wR:p5)` was **answered by
`wR:p4`**, and that lane stood down believing it was the duplicate. A misdelivered
instruction that changes behaviour is the worst failure mode there is — the same
family as the ack that cleared someone else's inbox.

Two causes, both now closed:

1. **The decorated address was not parsed.** `name (wR:p5)` matched neither an exact
   pane id nor an exact label, so it fell through to being a mailbox **name** with
   `pane_id: null`. Delivery then resolved the name — and a name is a conversation,
   not a process. The parenthetical is the only unambiguous part, so it decides:
   a decorated address resolves to that pane, and a decorated address naming a pane
   that is not live is refused by name.
2. **Pane ids with a lettered workspace were not recognised.** `_PANE_ID_RE` was
   `^w\d+:p\d+$`, so `wR:p9` was not a pane id at all and a *dead* pane id became a
   mailbox name. The pattern now accepts a lettered workspace.

A name that two panes share is still refused, with both candidates named. The rule:
**resolve by pane id at send time; refuse when the name is ambiguous.**

And the same night, the poll cache: its key was the command alone, so
`inbox count wR:p5` and `inbox count wR:p4` shared one entry and a caller asking
about one lane was handed the other's numbers. The key is now a hash of the whole
argument vector plus the store — **the third "correct-looking cache, scoped wrong"**
of the week, after the cross-store collision and the never-expiring entry.

## A version number is a claim about the code

For eight commits `dunk_core.py` said `VERSION = "2.37.4"` while the commit subjects
claimed 2.38 through 2.51, and `status` printed the constant. So the one number
people look at to answer **"is my fix live?"** answered *wrong, in the reassuring
direction* — and the only way to find out was to compare the daemon's process start
time to the file's mtime, which is not a check any human should have to run.

A hand-maintained version constant is the same failure as a hand-maintained census:
**a hand-maintained list is a future omission.** Two guards, one of each kind:

1. **Derived, so it cannot drift.** `code_identity()` hashes `dunk_core.py` (12 hex)
   and records its mtime at import; `status` prints the hash beside the constant and
   answers the real question:

   ```text
   version          2.52.0 (code 9c2de7f79541)
   code_mtime       2026-10-23 12:11:15 (7s ago)
   started_at       2026-10-23 12:11:18 (4s ago)
   code_stale       no
   ```

   `code_stale` compares the hash this process imported with the hash of the file
   **now**. (The first attempt compared a captured mtime against the process start
   time — a value against itself — and said `no` while the daemon ran four commits
   behind the file. A guard that has never been seen to fail is a guard we are
   assuming: appending one comment to the file now flips it to `YES - the code on
   disk is newer than this process; restart it`.)

2. **Checked, so a commit cannot claim what the code does not.** A test reads the
   latest commit subject that declares `N.N.N:` and fails when it disagrees with
   `VERSION`. Writing the guard is what forces the constant to be bumped in the same
   commit as the claim.

## The pane is the only source of truth about the pane

Four components failed the same way in one day: `send` believed a **claim** (claude's
session list saying busy, held for 8.5 hours while the pane sat with an empty prompt);
`delivery` and the queue asked **different questions** and gave different answers;
and `flush` disbelieved the pane, reporting "the input box is already clear" over a
visible draft because the draft sat on a bordered row (`│ ❯ check your inbox`) above
an empty prompt and only the last sigil line was consulted.

> **Any code path that answers a question about the pane without reading the pane
> will eventually disagree with it.** Caching, claims and inference are all the same
> mistake at different distances from the terminal.

Consequences enforced in the code:

- **`box_text(screen)` is the single reader** for "what is in the box", and it returns
  `(text, detail)` — a sentence saying *what it read and how* ("read 2 prompt
  line(s); text on row 1 of 4: 'check your inbox'"). A caller that finds nothing can
  therefore say `nothing to submit: <that sentence>`, never a bare "already clear".
- **`send_blocker()` is the single predicate** for "may a send go now", used by
  `delivery_state`, the outbox worker and `send_text`, so no two surfaces can answer
  differently.
- **A claim is believed only while fresh**, and the release needs *positive* evidence
  of idleness — a draft-free box is not evidence (a 75-minute byte job has one).

### The root cause worth keeping: a well-formed box defeated a parser that assumed plain text

`flush` read each line's **raw first character** to find the prompt sigil. On a
bordered composer that character is the **box border**, not the sigil:

```text
│ ❯ check your inbox      <- first char is '│': "not a prompt"
│
❯                         <- the only line it believed, and it is empty
```

So the reader concluded "already clear" over a real draft. Two details make this a
lesson rather than an anecdote:

- **it failed in the SAFE direction** (nothing was typed over anyone), which is
  exactly why it survived so long without anyone noticing;
- **the screen was well-formed** — the parser assumed plain text, and a correctly
  drawn box is what defeated it.

### One reader means one wrong answer, so the fixtures are the guard

When three components read the pane independently, a bug shows up as
*disagreement*. When they share one reader, a bug shows up as **everything being
wrong in the same direction** — so the fixture set matters more, not less. Three
literal screens, each a real shape a composer draws:

| fixture | what it pins |
|---|---|
| `bordered_draft_above_an_empty_prompt()` | the border character is not the sigil, and the draft is not the last line |
| `multi_line_draft()` | a pasted draft draws a sigil on every line: the draft **starts** at the first one, and the rest are continuations (reading the last line reported the tail as if it were the whole draft) |
| `draft_that_looks_like_a_prompt()` | the message itself begins with the sigil (`❯ ❯ ls`): the inner sigil is text, not a second box |

**One test drives all three through all three askers** — `box_text`, `flush_box`, and
`delivery_state`/`send_blocker` — and requires the last two to agree in the same
words. A wrong answer from the shared reader is now reproducible from a fixture
instead of mysterious from a pane.

Both shapes are literal fixtures, and
**one fixture proves all three askers**, because they share one reader: `box_text()`
returns the draft, `flush_box()` submits it, and `delivery_state()`/`send_blocker()`
report it as blocked *in the same words*.

Every question about the pane goes through `read_screen()` / `read_box()`. One read,
one parser, one fixture — so a component cannot quietly answer from a claim, a cache,
or a neighbouring row again.

## The automatic path that a human covers for fails silently

Four components failed this way in one day: **delivery** (the hold froze the fleet
while the VP hand-nudged), **the slot guard**, **flush** (reported "already clear"
while the box held a draft), and **the dispatcher** (the unit failed every tick for
hours while lanes were fed by hand). In each case the automatic path was dead, a
person kept the lights on, and nothing anywhere said so.

> **Any automatic path that a human can cover for will fail silently.** Give every
> one of them a freshness number that health reports, and a message that says what
> it was doing when it stopped.

Two instances of the fix, both cheap:

- **the feeder is a number**: `health` reports `dispatcher=ok last_tick=2.1m ago
  (128s)`, or `stale`, or `absent — never`, and lists a problem when the last
  completed tick is older than `DISPATCH_STALE_S` (two missed ticks). Absence and
  staleness are different facts with different sentences.
- **a pace problem is not a failure**: when a batch outlives its socket deadline the
  CLI says `still working after 180s: dispatch has not answered yet. This is a pace
  problem, not a failure - the daemon keeps going and its result lands in
  dispatch-state.json` and exits **75**, which the unit accepts via
  `SuccessExitStatus=75`. `error: timed out` sent a reader hunting a hang that did
  not exist; the difference between "needs a rewrite" and "outgrew its deadline" is
  the difference between two very different investigations.

## Activity comes from the tool; absent means UNKNOWN

A pi lane read `ready` while its pane plainly said `⠋ Working`, because pi reports no
status on the pane record. A live pane read `dead` because its transcript quoted the
marker. Both are the same defect: **a status field that is not the thing it claims to
be**. `dunk_activity.activity(pane)` answers the question per harness, with its own
source and, when there is none, a reason:

| harness | source | on failure |
|---|---|---|
| claude | `claude agents --json --cwd`, matched by session/pane cwd | `unknown` + "no claude session matches this pane's cwd" |
| pi | the extension's turn events (`turn_start` / `turn_end`) | `unknown` + "no pi turn has ever been recorded for this pane" |
| codex | its local turn socket - **not wired yet** | `unknown` + names the gap |

> **An absent signal is UNKNOWN, never idle.** "Assume idle" cost four stalls in one
> day; "unknown" costs one question.

Two consequences that are easy to lose:

- a pi turn that ENDED is kept (with its `ended_at`) rather than popped: that end
  time is the only evidence of idleness the extension will ever give, and popping it
  made every pi lane read `unknown` between turns;
- the DELIVERY path does not consult activity at all (that is step 0 of the herdr
  removal: a busy agent is not a hold). Activity serves the fleet questions - the
  roster, the dispatcher's reporting, the UI - so a gap there is a wrong dot, not a
  stuck message.

## The busy-hold policy is per harness, and it is measured

The policy is "a busy agent is not a hold — the harness queues". That is true of two
of our three harnesses, and the third would **lose** the message:

| harness | queued while working? | evidence |
|---|---|---|
| claude | **yes** | composer showed `ctrl+x ctrl+s to send now` / `Press up to edit queued messages`; the task ran unbroken to 3000 and PROBE-ACK-1 was answered afterwards (2026-10-24) |
| pi | **yes** | `Alt+Up to edit all queued messages`; PI-ACK-1 answered after the turn |
| codex | **NO** | four notices reported `sent` during a 54-minute turn; the composer sat at its placeholder and the scrollback held exactly one composer line — accepted and lost |

So `HARNESS_QUEUES_WHILE_BUSY = {"claude": True, "pi": True}` and anything else
(including an unknown harness) keeps a hold **with a reason that says which harness
and why**:

```text
codex does not queue text typed during a turn (measured: notices are lost), so this waits for the turn to end
```

Two bugs fell out of writing that: codex's status line is drawn **dim** (`• Working
(55m · esc to interrupt)`), so a bright-text-only reader never saw it work — the
working check now accepts dim text within eight lines; and codex's composer hint
(`Ask Codex to do anything`) was not in the placeholder patterns, so a bright redraw of
the hint was classified as a human typing. A hint drawn bright is still a hint.
