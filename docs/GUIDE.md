# Dunking Sheep 2.19.0 guide

Dunking Sheep sends text (followed by Enter) into herdr terminal panes on a
schedule. It exists to keep AI coding agents moving ("continue"), to let
agents message each other, and to let an agent schedule future nudges to
itself. Everything below is the same whether you drive it from the keyboard,
the command line, a unix socket, or the Model Context Protocol (MCP).

## Concepts

- dunk / dunker: one schedule. Fields: id (d1, d2, ...), name, target pane,
  text, interval_minutes, running, only_when, max_sends, send_count, status.
- target: a herdr pane. Give a pane id (w8:p3), the word self (the pane the
  caller runs in, from $HERDR_PANE_ID), a terminal id, or a unique
  case-insensitive substring of a tab label, workspace label, agent name or
  directory. An exact tab/workspace/agent label wins over a substring; among
  several substring matches a lone agent pane wins; anything still ambiguous
  is rejected with the candidates listed.
- send: the daemon runs `herdr pane send-text` then `herdr pane send-keys
  Enter` (chunked for long texts). Nothing touches the OS keyboard.
- daemon: `dunkingsheep serve`, auto-started by any client. Listens on a unix
  socket at ~/.config/dunkingsheep/dunkingsheep.sock ($DUNKINGSHEEP_SOCKET).
  Logs to ~/.config/dunkingsheep/daemon.log.
- persistence: dunks are saved to ~/.config/dunkingsheep/dunks.json on every
  change and restored, still running, when the daemon next starts. A dunk
  whose send came due while the daemon was down fires shortly after restart.

## Text placeholders

These expand at send time; any other braces are left untouched:

```
{id}        the dunk's id, e.g. d3      {name}      its name (or id)
{count}     this send's number (1, 2..) {target}    tab / agent label
{interval}  minutes between sends       {time} {date}  local clock
```

So a dunk can say who it is: "...when the backlog is finished, remove
dunk {id}" tells the receiving agent exactly which dunk to delete.

## Orchestration options

- only_when = idle: hold each send until herdr reports the target agent as
  idle (not working or blocked); re-checked every 5 seconds. Panes without a
  detected agent count as idle. A new schedule aimed at a pane that has an
  agent gets this gate by default, because typing into an agent mid-turn
  can abort that turn; pass only_when="any" (--any-time) to opt out.
- max_sends = N: the dunk removes itself after its N-th send. N = 1 is a
  one-shot delayed nudge ("in 30 minutes, tell me to check CI").
- skip_if_unconsumed = true: backpressure. Before each send the daemon reads
  the target's visible input box and skips the send if the previous one is
  still sitting there unread: either the sent text verbatim, or the
  collapsed-paste placeholder Claude Code shows for a long queued input
  ('Pasted text', '+N lines (ctrl+o to expand)'). A skip is a no-op:
  send_count unchanged, no error, skip_count +1, status 'Skipped
  (unconsumed)', next send rescheduled as usual. The first send always
  goes. It reads the input box, not the agent's busy state, because a pane
  idling at a prompt with queued input reports idle and a pane busy on an
  earlier turn says nothing about the newest send. If the box cannot be read
  or recognised the send is held, since a missed nudge is recoverable next
  interval but a stacked pile-up is not. Composes with only_when. Turn it on
  for self-dunks and unattended agents, where several prompts would
  otherwise queue in the box and flush into one turn. Blind spots: it
  detects our own queued send (verbatim or its collapsed placeholder) but
  not arbitrary short text a human left in the box, and it assumes an agent
  pane that draws an input box (Claude/Codex), not a bare shell.
- hold_while_typing = true (the default): never type over a human. Before
  each send the daemon reads the target's input box with its styling. Both
  Claude Code and Codex draw their empty-box hints dim and typed text at
  full intensity, so bright text after the prompt sigil means someone is
  mid-sentence. The send is then held (status 'Held (typing)', hold_count
  +1, no error) and re-checked every minute until the box is clear; an
  idle-gated dunk re-runs its idle gate afterwards, since finishing typing
  usually means submitting. Our own leftover send does not count as typing
  (that is skip_if_unconsumed's job), and a pane with no input box (a bare
  shell) or an unreadable pane never holds. Only the sigil line is judged.
  Turn it off (--ignore-typing / hold_while_typing=false) only for a pane
  nobody types in. The box must read clear twice, a second apart, and once
  more at the instant of sending, so neither a pause between thoughts nor a
  draft begun while the send waits its turn can be typed over. This applies
  to scheduled sends, fired dunks, direct messages and key presses alike.
- start = false: create the dunk stopped; start it later.
- Changing the interval of a running dunk reschedules its next send.

## Surface 1: command line

```
dunkingsheep panes                          list targets (* marks this pane)
dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle]
                 [--skip-if-unconsumed] [--ignore-typing] [--max-sends N] [--no-start]
                 -e takes 15, 90s, 1.5h
dunkingsheep list | get <id> | update <id> ... | remove <id>
dunkingsheep start|stop|toggle <id>    stop-all    fire <id> (send now)
dunkingsheep send <target> <text...>   one-off send, no dunk (queued if typing)
                 [--wait S] [--ignore-typing]
dunkingsheep send-keys <target> <keys...>     guarded key presses
dunkingsheep messages | cancel-message <id>   the direct-message queue
dunkingsheep flush <target>                   submit text stuck in a pane's box
dunkingsheep inbox count|read|list|send|status|ack|cancel|delete|stats|fetch
dunkingsheep metrics [--window S]             operations metrics as JSON
dunkingsheep install                          put the CLI on PATH
dunkingsheep guard [status|install|check|uninstall]
dunkingsheep read <target> [-l N]      last N lines of a pane
dunkingsheep workspaces | new-workspace | new-tab | split | start-agent
dunkingsheep rename|focus|close workspace|tab|pane <target> [label]
dunkingsheep herdr <herdr args...>     any herdr subcommand;  herdr-help [group]
dunkingsheep status | shutdown [--stop-all] | serve | tui | mcp
dunkingsheep commands [--json]         the full command registry
Add --json to any command for machine-readable output.
```

## Surface 2: unix socket API

Newline-delimited JSON, any number of requests per connection. The command
names and arguments are exactly the registry from `dunkingsheep commands`.

```
-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",
    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}
<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, ...}}
<- {"id": 1, "ok": false, "error": "no dunk with id 'd9'"}

# from a shell:
printf '%s\n' '{"id":1,"cmd":"list_dunks"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
# discover everything, including JSON schemas:
printf '%s\n' '{"id":1,"cmd":"help"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock
```

`self_pane_id` is optional; it is what target "self" resolves to. Clients
normally pass their own $HERDR_PANE_ID.

## Surface 3: MCP server (Claude Code, Codex, Gemini CLI, Cursor, ...)

`dunkingsheep mcp` is a Model Context Protocol server over stdio (newline-
delimited JSON-RPC, protocol versions 2024-11-05 through 2025-11-25, tools
only, standard library only). Every registry command marked for MCP is a
tool with a flat JSON schema (string/number/integer/boolean, no unions), so
strict validators such as OpenAI's accept it. `shutdown` is not exposed.

`dunkingsheep mcp-setup` prints the exact registration for each harness and
`dunkingsheep mcp-setup <harness> --apply` runs it when that harness has a CLI:

```
claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp
codex mcp add dunkingsheep -- python3 /path/to/dunkingsheep mcp
gemini mcp add --scope user dunkingsheep python3 /path/to/dunkingsheep mcp

# any harness that reads an mcpServers JSON block (Cursor, Windsurf, Claude
# Desktop, VS Code, ...):
{"mcpServers": {"dunkingsheep": {"command": "python3",
                                 "args": ["/path/to/dunkingsheep", "mcp"]}}}

# ~/.codex/config.toml equivalent:
[mcp_servers.dunkingsheep]
command = "python3"
args = ["/path/to/dunkingsheep", "mcp"]
```

The harness spawns the MCP server inside the agent's own herdr pane, so the
server inherits HERDR_PANE_ID and target "self" means the agent itself. If a
harness scrubs the environment, pass `--self-pane <pane_id>` after `mcp` or
set HERDR_PANE_ID in the server's env block.

Tools: help, status, list_panes, read_pane, list_dunks, get_dunk, add_dunk,
update_dunk, remove_dunk, start_dunk, stop_dunk, stop_all, fire_dunk,
send_text, send_keys, flush_box, list_messages, get_message, cancel_message,
inbox_send, inbox_count, inbox_list, inbox_read, inbox_status, inbox_ack,
inbox_cancel, inbox_delete, inbox_stats, inbox_fetch,
and for herdr
control list_workspaces, create_workspace,
create_tab, split_pane, start_agent, rename, focus, close, herdr,
herdr_help. Call `help` first if unsure; `initialize` also returns these
instructions. Errors come back as tool results with isError=true and a
plain-English message (never as protocol errors), so the agent can recover.

## Building the herd: herdr control from the same surfaces

Every surface can also shape herdr itself, so an admin agent can spin up
workspaces, tabs and agents and then dunk on them. Targets for workspaces
and tabs follow the pane rules: id (w9, w9:t2), exact label, unique label
substring, or 'self' (the caller's own). Everything is created in the
background unless focus=true.

- list_workspaces: workspaces with their tabs; `self` marks the caller's.
- create_workspace(label, cwd, command): a named space with its first tab
  and shell pane; `command` (e.g. 'claude') is typed + Enter into that pane.
- create_tab(workspace, label, cwd, command): a named tab; workspace
  defaults to the caller's own.
- split_pane(target, direction, cwd, command): a new shell pane right of or
  below any pane.
- start_agent(name, command, workspace|tab, split, cwd): launch an agent
  process registered with herdr under `name` (herdr agent start), so herdr
  lists it and tracks its status by that name. Returns its pane id. With
  `workspace`, herdr picks the tab; create_tab first and pass `tab` to
  control the tab's name.
- rename(kind, target, label) / focus(kind, target) / close(kind, target)
  for kind = workspace | tab | pane. close kills what runs inside, stops any
  dunks aimed there, and refuses the caller's own pane, tab or workspace.
- herdr(command): run any herdr subcommand ('pane zoom w9:p2 --on',
  'notification show Done', 'wait agent-status w9:p2 --status idle
  --timeout 60000', 'api schema --json'); returns its parsed JSON, or the
  usage text for a bare group. 'self' in the arguments becomes the caller's
  pane id (tab/workspace id in tab/workspace commands). herdr wants ids,
  not labels, here. Refused: server stop, update, channel set,
  session/agent attach.
- herdr_help(topic): herdr's own usage, overall or for one group, to
  discover the above. The `help` command also returns it as `herdr_help`.

```
dunkingsheep workspaces                       # what exists
dunkingsheep new-workspace -l Site --cwd ~/site -r claude
dunkingsheep new-tab -w Site -l writer -r 'codex --model gpt-5'
dunkingsheep start-agent reviewer -w Site -- claude --model opus
dunkingsheep split Site --down -r 'npm test -- --watch'
dunkingsheep rename tab writer 'Site writer'   |  focus tab 'Site writer'
dunkingsheep close tab 'Site writer'          # kills it, stops its dunks
dunkingsheep herdr pane zoom self --on        # anything herdr can do
dunkingsheep herdr-help pane                  # herdr's own usage
```

## Recipe: an admin agent spins up a team and dunks on it

```
create_workspace(label="Site", cwd="~/site")
start_agent(name="builder", command="claude", workspace="Site")
start_agent(name="writer", command="codex", workspace="Site")
send_text(target="builder", text="Implement the login page; tests first.")
add_dunk(target="builder", interval_minutes=15, only_when="idle",
         skip_if_unconsumed=true, text="Status? Next step?")
add_dunk(target="writer", interval_minutes=30, only_when="idle",
         text="Document what builder shipped since your last pass.")
# later, when the work is done:
close(kind="workspace", target="Site")   # stops both dunks too
```

## Surface 4: the TUI

`dunkingsheep tui` (or ./run_dunking_sheep.sh) shows the same dunks live.
Keys: j/k move, a add, d remove, c choose target, t test send, i interval,
e text, n name, o toggle idle-gate, m max sends, space/s start-stop, q quit
(dunks keep running), Q stop every dunk and shut the daemon down.

## Recipe: one agent watching and steering many

Because every pane is addressable, one agent can supervise a herd:

```
list_panes()                                  # who is running where, idle or working
read_pane(target="Codex Site", lines=80)      # what is that agent doing?
send_text(target="Codex Site", text="Stop and write tests first.")
add_dunk(target="Melt Squad", interval_minutes=20, only_when="idle",
         text="Progress report: what changed since the last one?")
```

## Direct messages queue behind the human, like dunks

send_text never types over someone mid-sentence. If the target's input box
holds a draft, the message waits in the daemon and is delivered once the
box has been clear for a second; several messages to the same pane keep
their order. The result says which happened: `queued: true` with a
`message_id`, or `ok: true` when it went straight out. fire_dunk queues the
same way when its dunk holds while typing.

```
send_text(target="Codex Site", text="Stop and write tests first.")
  -> {"message_id": "m3", "queued": true, ...}   # they were typing
list_messages()                  # what is waiting, and what just landed
get_message(message_id="m3")     # has it gone yet?
cancel_message(message_id="m3")  # never mind, drop it
send_text(target="Codex Site", text="...", wait_s=30)  # block for delivery
send_text(target="Codex Site", text="...", hold_while_typing=false)  # barge in
```

Queued messages live in the daemon's memory, not on disk, so a daemon
restart drops anything still waiting.

send_keys(target, keys) presses keys the same guarded way. Use it rather
than the herdr passthrough: a bare Enter sent while someone is mid-sentence
submits their half-written message. For that reason the `herdr` tool
refuses `pane run`, `pane send-text`, `pane send-keys` and `agent send`.

## Getting the message actually submitted

Pressing Enter once does not reliably submit. Measured on live panes:
Claude Code submits on the first Enter whatever the size, but Codex needs
the pane to have been quiet for a while first, and the bigger the paste the
longer it needs. Raising the delay before Enter does not fix it on its own,
and neither does a smaller paste chunk; pressing Enter repeatedly in quick
succession does nothing either.

So the daemon confirms rather than guesses. After typing it reads the input
box back, and while its own text is still sitting there it lets the pane
fall quiet and presses Enter again, with gaps that grow each time. A blind
double Enter would be unsafe: if the first one worked, the second would
submit whatever you typed next.

The same confirmation covers a bare Enter sent to submit text already
waiting in the box, which is how agents that type and submit in two steps
were leaving messages stranded.

What it costs, measured on Codex: about 0.4s for a short message, 4s at
600 characters, 10s at 1.2k and 19s at 2.2k. Claude is about a second at
any size. Only the confirmation is slow, and only while a send is actually
sticking.

Two consequences worth knowing. A send that never leaves the box is
reported as failed rather than sent, and shows up in last_error and in
`messages`. And text stranded in a box by an earlier failure is recognised
as ours: the next send submits it first rather than appending to it or
waiting behind it for ever. For text the daemon did not type (an agent
using the herdr CLI, or someone who walked away mid-sentence), which would
otherwise block that pane's queue indefinitely, `flush_box(target)` /
`dunkingsheep flush <target>` submits it explicitly.

## Inboxes: messaging that never types

Typing into a terminal is the human's channel, and an unreliable one for
machines: Enter does not always submit, long pastes strand, and a message
in flight can collide with someone mid-sentence. For agent-to-agent
traffic, use an inbox instead. A message is stored for the recipient to
read when it chooses, so none of those failures exist. There is no size
limit, messages survive a daemon restart, and they can carry files.

The only thing typed is one short line telling the recipient it has mail.
It names the inbox, who it is from and what it is about, so they can judge
urgency without reading first:

```
[inbox] new message for poolrad-codex from PM: PR 130 review.
        Read it with: dunkingsheep inbox read
```

That is the one send short enough to always submit, which is the point:
the message itself never has to be typed. Pass notify=false to skip even
the line.

```
inbox_count()                          # how many am I holding?
inbox_read()                           # read them, oldest first
inbox_send(to="reviewer", body="...", subject="PR 12")
inbox_send(to="reviewer", body="...", attach="/tmp/patch.diff")
inbox_status(message_id="i42")         # where is that message?
inbox_ack(message_ids="i42")           # done with it
inbox_stats()                          # who has mail piling up?

dunkingsheep inbox count | read | list | stats
dunkingsheep inbox send reviewer 'PR 12 is ready' -s review -a /tmp/p.diff
dunkingsheep inbox status i42 | ack i42 | cancel i42 | delete i42
dunkingsheep inbox fetch <attachment-id> -d .
```

A mailbox identifies one pane: its own label first, since that is a name
someone chose, then its tab label, then its pane id. Whichever is taken
has to belong to exactly one pane. The agent *type* is never a mailbox,
so 'codex' is refused rather than pooling every Codex pane together.
`self` means your own. A name matching no running pane still works, so
mail can be left for an agent that has not started. Renaming a pane or
tab renames its mailbox, so mail already waiting stays under the old name.

Every send returns a stable message id. `inbox_status` reports where that
message is - queued, read, acked or cancelled - with its destination,
sender, timestamps, size, attachment count and any failure reason. It
carries neither body nor subject, so a status board can poll it without
seeing the contents, and it answers across daemon restarts.

Read, acknowledged and cancelled mail is kept for a week and then purged.
Unread mail is never purged by age. Attachments are stored by content
hash and deleted with the last message that referenced them.

A notice is never stacked. If one is still waiting for a pane when more
mail arrives, it is updated to the new unread count rather than a second
line being queued behind it: an agent that reads its inbox once has dealt
with all of it. If our own text is sitting unsubmitted in the way, the
daemon clears it within 30 seconds rather than waiting on itself; a human's
draft is still never touched. A draft has to be there on two readings
before a send is held, so a frame of an agent redrawing its pane is not
mistaken for someone writing.

`inbox_status` tracks the notice too: it says queued while the notice is
waiting and sent once it lands, including for messages whose notice was
folded into another.

A standing dunk is the easy way to get an agent checking: point one at the
agent with text like "call inbox_count; if anything is waiting, read and
act on it".

## Setting an agent up to use its inbox

Two things make the inbox reachable from an agent's session.

```
dunkingsheep install     # put the CLI on PATH as a plain command
dunkingsheep mcp-setup claude --apply   # register the MCP server
```

`install` matters more than it looks. herdr starts agents by exec'ing them
from its server process, so they inherit that environment: ~/.local/bin is
on their PATH but this project is not. Without the launcher an agent has to
invoke the CLI by its path inside a workspace, which a sandboxed agent
should refuse and does - Claude's auto mode reads it as running external
code. A plain `dunkingsheep inbox read` is the supported shape, and an
allow rule for `Bash(dunkingsheep *)` in ~/.claude/settings.json lets auto
mode run it without prompting.

An agent whose MCP connection predates the inbox has a stale tool list and
will not see inbox_read until it reconnects. The CLI works meanwhile, which
is why the launcher is the first thing to install.

## Tasks: deterministic coordination

Tasks are the fleet's shared work queue. The daemon stores them in SQLite
and serves them through the same CLI, socket and MCP surfaces as
everything else. Nothing in the loop is a model: selection, priority
ordering, dependency gating, atomic claims and requeueing are
deterministic algorithms, so agents can spend their tokens on work rather
than coordination.

A task is global (visible fleet-wide) or scoped to one pane (private to
that pane and its agent). Global is the default because it is the
coordination backbone; use scope='self' for local work.

```
fields: id, title, body, status (open|in_progress|blocked|done|cancelled),
        priority (urgent > high > medium > low), assignees (0..N),
        parent_id (subtasks), depends_on, attempts, and a lease
        (claimed_at, heartbeat_at, expires_at)
rules:  global completion (any assignee completes it for all);
        next = open/in_progress, unblocked, deps done, priority then age;
        atomic claim under BEGIN IMMEDIATE (two agents never win the
        same exclusive task); multi_assignee tasks accept several;
        lease default 1800s; expired or dead-pane leases requeue;
        push to idle followers reuses the dunk idle gate + inbox nudge
```

`task_next` is the atomic pull: it selects, assigns you, sets in_progress
and records a lease in one SQLite transaction. `task_heartbeat` extends the
lease; `task_complete` completes it for everyone. `task_follow` is the
push: the daemon then hands you the next task when herdr reports your
pane idle, so you do not poll. An empty task_next subscribes you
automatically. `inbox_read` returns your global tasks and your own pane's
tasks after the messages.

```
dunkingsheep task add "Ship the parser" -p high --body "..."
dunkingsheep task add "Write tests" --depends-on t12
dunkingsheep task next                 # atomic claim (global by default)
dunkingsheep task next auto            # own pane queue, then global
dunkingsheep task heartbeat t12        # extend the lease
dunkingsheep task complete t12         # global completion
dunkingsheep task follow               # receive pushed work when idle
dunkingsheep task list --all
dunkingsheep task assign t12 self      # explicit self-assign
dunkingsheep task block t12 --reason "waiting on API"

task_add(title=..., priority="urgent", depends_on=["t3"])
task_next(scope="auto"); task_heartbeat(id="t12"); task_complete(id="t12")
task_follow(); task_list(); task_assign(id="t12", assignees=["self"])
```

`task_metrics` is the read-only analytics endpoint for a status board or
the CTO. It reports current queue depth and the status / priority / scope
mix, per-assignee load, created/claimed/completed/cancelled/requeued
throughput in hourly or daily buckets, completion and claim latency
(avg/p50/p90/max), and per-actor counts. It is computed deterministically
from an append-only `task_events` table, so it is timeseries-friendly and
needs no model. Scope is 'all' (default), 'global', 'self' or a pane id:
an agent sees global plus its own pane, while the human/CTO CLI sees
everything.

```
dunkingsheep task metrics                 # last hour, hourly buckets
dunkingsheep task metrics --window 86400 --bucket day
dunkingsheep task metrics global --since <epoch> --until <epoch>
task_metrics(window_s=3600, scope="global", bucket="hour")
```

Per-pane privacy is enforced by the daemon: an agent may add, list, read
or claim only its own pane's tasks (plus global tasks). A caller with no
pane, such as the human CLI outside herdr, may inspect any pane. Use
`task_reap` to requeue expired or dead-pane leases immediately; the daemon
also does it on a timer.

## Memory and recall

A SQLite + FTS5 memory service ingests Claude/Codex/opencode/pi sessions
in the background through one writer, and answers recall from read-only
connections. A query never reindexes, never swaps an index, and never
waits for ingest; that separation is the fix for the 2026-10-18 stalls.
The sample acceptance is 20 clients for 60 s while ingest runs, p95 <1 s.

```
dunkingsheep memory recall "query" [--limit 10] [--harness ...]
dunkingsheep memory context "query" [--sessions 5]
dunkingsheep memory search "query" --project ... --since ...
dunkingsheep memory stats | status
dunkingsheep memory ingest <session.jsonl|history.sqlite> [--harness ...]
```

MCP: memory_recall / memory_context / memory_search / memory_stats /
memory_status; memory_ingest is superuser-only. `memory_stats` reports
p50/p95/p99, DB bytes and queue depth. Deja stays in place until parity
(t10) and the agent upgrade (t11); do not start the full backfill before
the sample acceptance passes.

## Superusers and destructive operations

Only panes and names on the superuser allowlist may close a pane, tab or
workspace, stop the daemon, manage dunks, spawn agents, or use the raw
herdr passthrough. Everyone else communicates through the inbox and task
queue, and may type only into their own pane. This is the response to the
2026-10-18 incident, where a free model's cleanup closed the VP's tab and
took nine panes with it.

```
dunkingsheep superuser status
dunkingsheep superuser add <pane-or-name>
dunkingsheep superuser remove <pane-or-name>
dunkingsheep superuser recover    # setup/takeover: add this pane + VP name
```

The allowlist is ~/.config/dunkingsheep/superusers.json: pane ids and
names. A name matches a pane's own label, tab label or workspace label, so
the VP is recognised after its pane id changes. An empty allowlist refuses
every destructive operation; the first `superuser recover` bootstraps it.
Names default to the VP and owner; the setup/recovery step adds the VP's
actual pane id.

Self-protection: even a superuser cannot close the pane, tab or workspace
it runs in unless it passes `close --force`. Spawning an agent without an
explicit workspace/tab puts it in the dedicated `sandbox` workspace,
never the caller's tab.

## Panels and dialogs

A slash command such as /usage replaces the input box with a panel, and a
dialog draws its selected row with the same prompt sigil the input box
uses. Both used to read as occupied input: a /usage send was reported
failed although it had run, three further Enter presses landed on the
panel, and an Escape meant to dismiss it queued behind what looked like
someone typing.

The daemon now recognises an open panel. A send whose text opened one is
finished, not stuck. Text waits for the panel to go rather than typing
into it, while key presses go straight through, since keys are how a panel
is dismissed.

A dialog is never answered for you. An approval prompt is a question put
to a person, so pressing its dismiss key would choose on their behalf, and
in a working pane that key interrupts the turn. While a dialog is open the
daemon types nothing and presses nothing there, says so once in the log,
and shows it in `messages` as what is blocking that pane. Delivery resumes
by itself once somebody answers.

Only the bottom of the screen counts as the live interface. An approval
prompt scrolled up into the transcript is history, not a dialog.

## Operations metrics

`metrics` returns a read-only JSON snapshot of how delivery is going, for
a status board to poll. It carries identifiers and numbers only: mailbox
names, pane ids, dunk ids, counts, durations and byte sizes. No message
body, no subject, no filename, no account email, and nothing about token
spend, which would be a guess dressed as a measurement.

```
dunkingsheep metrics --window 3600        # the last hour, as JSON
dunkingsheep metrics --since <epoch> --until <epoch>
metrics(window_s=3600)                    # same thing over MCP or the socket
```

The document is versioned (`schema_version`) and self-describing: it
carries its own interval, units, and a definition of every event kind,
outcome and block reason it can report.

Two things to read carefully. A duration is null, never zero, when nothing
in the window measured it, and the names of those appear in
`coverage.missing`. And `coverage.events_since` says when durable
recording began, so a window that reaches back further is reported as
partially covered rather than quietly empty. Counters kept per schedule
appear under `cumulative`, separate from anything windowed, because they
run from each schedule's creation and are not a rate.

Events are kept for thirty days and pruned on daemon start. The store is a
separate database from the inbox, so it can be pruned or deleted without
touching mail anyone is waiting to read.

## The herdr guard: agents that shell out

An agent with a shell can call the herdr CLI directly, and `herdr pane run
<pane> "..."` types straight into a terminal with no idea anyone is writing
there. Nothing inside this daemon can see that. `dunkingsheep guard install`
puts a small `herdr` wrapper first on PATH which forwards exactly the four
typing subcommands to the daemon and execs the real herdr for everything
else, so agents get the same protection without changing how they work.

```
dunkingsheep guard status      # installed? active in this shell?
dunkingsheep guard install     # wrapper + a PATH line in ~/.bashrc
dunkingsheep guard check       # prove a typing command is intercepted
dunkingsheep guard uninstall   # remove both
```

Installing does two things. It puts the wrapper first on PATH for new
shells, and it takes herdr's own place: the real binary is renamed to
`herdr-real` beside it. That second part matters, because herdr starts
agents by exec'ing them from its server process, so they inherit the
server's environment and never read a shell profile. Shadowing the binary
is what covers agents that are already running.

Set HERDR_GUARD_OFF=1 for a single command that must bypass it. If you
update herdr, it replaces the shadowed binary, so run `dunkingsheep guard
install` again afterwards; `guard status` shows whether it is in place.

send_text is a direct message with no schedule attached; read_pane is the
eyes. Together with add_dunk they let an agent communicate with, monitor and
pace several projects in parallel.

## Recipe: meta-dunking (an agent schedules itself)

An agent with the MCP tools can implement a backlog under external control:

```
add_dunk(target="self", interval_minutes=60, only_when="idle",
         skip_if_unconsumed=True, name="backlog",
         text="Backlog check #{count}: read BACKLOG.md. If every item is done,\n"
              "call remove_dunk('{id}') and stop. Otherwise implement the next\n"
              "item, commit, and update BACKLOG.md.")
```

Every hour the daemon types that prompt into the agent's pane (waiting for
it to be idle first, and skipping the hour entirely if the previous prompt
is still sitting unread). The prompt carries its own dunk id, so the future
agent can end the loop by removing the dunk. The same agent can read other
panes (read_pane), nudge other agents (send_text), or put them on their own
schedules (add_dunk with their pane as target).

## Troubleshooting

- "herdr server not running": start herdr (`herdr status server`).
- "no herdr pane matches": run `dunkingsheep panes` and use the pane id.
- "target 'self' needs HERDR_PANE_ID": the caller is not inside a herdr
  pane; pass a pane id or `--self-pane w8:p3`.
- Daemon problems: ~/.config/dunkingsheep/daemon.log; `dunkingsheep shutdown`
  then any command restarts it with the saved dunks.
- Send failed: the pane was closed. Re-target with `update <id> -T ...`.

