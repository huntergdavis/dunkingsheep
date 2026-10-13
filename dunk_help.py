#!/usr/bin/env python3
"""
The one guide to Dunking Sheep, shared by every surface.

- `dunkingsheep guide` prints it (plain text or `--markdown`).
- `dunkingsheep --help` shows the short version as its epilog.
- The MCP server returns it from `initialize.instructions` and the `help` tool.
- The socket API returns it from the `help` command, with the command registry.
- `docs/GUIDE.md` is the markdown rendering (a test keeps it in sync).

Keep it accurate: this is what an agent reads to teach itself the tool.
"""

from dunk_core import VERSION

SHORT = """\
Dunking Sheep types text (plus Enter) into herdr terminal panes on a schedule.
Each schedule is a "dunk": a target pane, a text, an interval, and a running
flag. Dunks live in a background daemon, so the TUI, this CLI, the unix
socket and the MCP server all see and control the same dunks.

quick start
  dunkingsheep panes                              # what can I target?
  dunkingsheep add -T "Codex Site" -e 15 -t continue
  dunkingsheep add -T self -e 60 --only-idle --skip-if-unconsumed \\
      -t "Check the backlog. If done: dunkingsheep remove {id}. Else keep going."
  dunkingsheep list                               # live status, ids, countdowns
  dunkingsheep remove d2
  dunkingsheep tui                                # the curses view of the same dunks

learn more
  dunkingsheep guide          full guide (concepts, MCP, socket protocol, recipes)
  dunkingsheep commands       every command the socket/MCP expose (--json: schemas)
  dunkingsheep <command> -h   per-command flags and examples
"""


def guide(markdown=False):
    """Return the full guide as plain text (default) or GitHub markdown."""
    out = []

    def h(level, text):
        if markdown:
            return f"{'#' * level} {text}\n\n"
        return _underline(text, level) + "\n"

    def code(body):
        if markdown:
            return f"```\n{body.rstrip()}\n```\n\n"
        return "".join(f"    {line}\n" for line in body.rstrip().splitlines()) + "\n"

    def w(text):
        # Paragraphs and lists are already newline-terminated; separate blocks.
        out.append(text if text.endswith("\n\n") else text + "\n")

    w(h(1, f"Dunking Sheep {VERSION} guide"))
    w("Dunking Sheep sends text (followed by Enter) into herdr terminal panes on a\n"
      "schedule. It exists to keep AI coding agents moving (\"continue\"), to let\n"
      "agents message each other, and to let an agent schedule future nudges to\n"
      "itself. Everything below is the same whether you drive it from the keyboard,\n"
      "the command line, a unix socket, or the Model Context Protocol (MCP).\n")

    w(h(2, "Concepts"))
    w("- dunk / dunker: one schedule. Fields: id (d1, d2, ...), name, target pane,\n"
      "  text, interval_minutes, running, only_when, max_sends, send_count, status.\n"
      "- target: a herdr pane. Give a pane id (w8:p3), the word self (the pane the\n"
      "  caller runs in, from $HERDR_PANE_ID), a terminal id, or a unique\n"
      "  case-insensitive substring of a tab label, workspace label, agent name or\n"
      "  directory. An exact tab/workspace/agent label wins over a substring; among\n"
      "  several substring matches a lone agent pane wins; anything still ambiguous\n"
      "  is rejected with the candidates listed.\n"
      "- send: the daemon runs `herdr pane send-text` then `herdr pane send-keys\n"
      "  Enter` (chunked for long texts). Nothing touches the OS keyboard.\n"
      "- daemon: `dunkingsheep serve`, auto-started by any client. Listens on a unix\n"
      "  socket at ~/.config/dunkingsheep/dunkingsheep.sock ($DUNKINGSHEEP_SOCKET).\n"
      "  Logs to ~/.config/dunkingsheep/daemon.log.\n"
      "- persistence: dunks are saved to ~/.config/dunkingsheep/dunks.json on every\n"
      "  change and restored, still running, when the daemon next starts. A dunk\n"
      "  whose send came due while the daemon was down fires shortly after restart.\n")

    w(h(2, "Text placeholders"))
    w("These expand at send time; any other braces are left untouched:\n")
    w(code("{id}        the dunk's id, e.g. d3      {name}      its name (or id)\n"
           "{count}     this send's number (1, 2..) {target}    tab / agent label\n"
           "{interval}  minutes between sends       {time} {date}  local clock"))
    w("So a dunk can say who it is: \"...when the backlog is finished, remove\n"
      "dunk {id}\" tells the receiving agent exactly which dunk to delete.\n")

    w(h(2, "Orchestration options"))
    w("- only_when = idle: hold each send until herdr reports the target agent as\n"
      "  idle (not working or blocked); re-checked every 5 seconds. Panes without a\n"
      "  detected agent count as idle. A new schedule aimed at a pane that has an\n"
      "  agent gets this gate by default, because typing into an agent mid-turn\n"
      "  can abort that turn; pass only_when=\"any\" (--any-time) to opt out.\n"
      "- max_sends = N: the dunk removes itself after its N-th send. N = 1 is a\n"
      "  one-shot delayed nudge (\"in 30 minutes, tell me to check CI\").\n"
      "- skip_if_unconsumed = true: backpressure. Before each send the daemon reads\n"
      "  the target's visible input box and skips the send if the previous one is\n"
      "  still sitting there unread: either the sent text verbatim, or the\n"
      "  collapsed-paste placeholder Claude Code shows for a long queued input\n"
      "  ('Pasted text', '+N lines (ctrl+o to expand)'). A skip is a no-op:\n"
      "  send_count unchanged, no error, skip_count +1, status 'Skipped\n"
      "  (unconsumed)', next send rescheduled as usual. The first send always\n"
      "  goes. It reads the input box, not the agent's busy state, because a pane\n"
      "  idling at a prompt with queued input reports idle and a pane busy on an\n"
      "  earlier turn says nothing about the newest send. If the box cannot be read\n"
      "  or recognised the send is held, since a missed nudge is recoverable next\n"
      "  interval but a stacked pile-up is not. Composes with only_when. Turn it on\n"
      "  for self-dunks and unattended agents, where several prompts would\n"
      "  otherwise queue in the box and flush into one turn. Blind spots: it\n"
      "  detects our own queued send (verbatim or its collapsed placeholder) but\n"
      "  not arbitrary short text a human left in the box, and it assumes an agent\n"
      "  pane that draws an input box (Claude/Codex), not a bare shell.\n"
      "- hold_while_typing = true (the default): never type over a human. Before\n"
      "  each send the daemon reads the target's input box with its styling. Both\n"
      "  Claude Code and Codex draw their empty-box hints dim and typed text at\n"
      "  full intensity, so bright text after the prompt sigil means someone is\n"
      "  mid-sentence. The send is then held (status 'Held (typing)', hold_count\n"
      "  +1, no error) and re-checked every minute until the box is clear; an\n"
      "  idle-gated dunk re-runs its idle gate afterwards, since finishing typing\n"
      "  usually means submitting. Our own leftover send does not count as typing\n"
      "  (that is skip_if_unconsumed's job), and a pane with no input box (a bare\n"
      "  shell) or an unreadable pane never holds. Only the sigil line is judged.\n"
      "  Turn it off (--ignore-typing / hold_while_typing=false) only for a pane\n"
      "  nobody types in. The box must read clear twice, a second apart, and once\n"
      "  more at the instant of sending, so neither a pause between thoughts nor a\n"
      "  draft begun while the send waits its turn can be typed over. This applies\n"
      "  to scheduled sends, fired dunks, direct messages and key presses alike.\n"
      "- start = false: create the dunk stopped; start it later.\n"
      "- Changing the interval of a running dunk reschedules its next send.\n")

    w(h(2, "Surface 1: command line"))
    w(code("dunkingsheep panes                          list targets (* marks this pane)\n"
           "dunkingsheep add -T <target> -e <every> -t <text> [-n name] [--only-idle]\n"
           "                 [--skip-if-unconsumed] [--ignore-typing] [--max-sends N] [--no-start]\n"
           "                 -e takes 15, 90s, 1.5h\n"
           "dunkingsheep list | get <id> | update <id> ... | remove <id>\n"
           "dunkingsheep start|stop|toggle <id>    stop-all    fire <id> (send now)\n"
           "dunkingsheep send <target> <text...>   one-off send, no dunk (queued if typing)\n"
           "                 [--wait S] [--ignore-typing]\n"
           "dunkingsheep send-keys <target> <keys...>     guarded key presses\n"
           "dunkingsheep messages | cancel-message <id>   the direct-message queue\n"
           "dunkingsheep flush <target>                   submit text stuck in a pane's box\n"
           "dunkingsheep inbox count|read|list|send|status|ack|cancel|delete|stats|fetch\n"
           "dunkingsheep install                          put the CLI on PATH\n"
           "dunkingsheep guard [status|install|check|uninstall]\n"
           "dunkingsheep read <target> [-l N]      last N lines of a pane\n"
           "dunkingsheep workspaces | new-workspace | new-tab | split | start-agent\n"
           "dunkingsheep rename|focus|close workspace|tab|pane <target> [label]\n"
           "dunkingsheep herdr <herdr args...>     any herdr subcommand;  herdr-help [group]\n"
           "dunkingsheep status | shutdown [--stop-all] | serve | tui | mcp\n"
           "dunkingsheep commands [--json]         the full command registry\n"
           "Add --json to any command for machine-readable output."))

    w(h(2, "Surface 2: unix socket API"))
    w("Newline-delimited JSON, any number of requests per connection. The command\n"
      "names and arguments are exactly the registry from `dunkingsheep commands`.\n")
    w(code('-> {"id": 1, "cmd": "add_dunk", "self_pane_id": "w8:p8",\n'
           '    "args": {"target": "self", "text": "continue", "interval_minutes": 20}}\n'
           '<- {"id": 1, "ok": true, "result": {"id": "d4", "running": true, ...}}\n'
           '<- {"id": 1, "ok": false, "error": "no dunk with id \'d9\'"}\n'
           "\n"
           "# from a shell:\n"
           "printf '%s\\n' '{\"id\":1,\"cmd\":\"list_dunks\"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock\n"
           "# discover everything, including JSON schemas:\n"
           "printf '%s\\n' '{\"id\":1,\"cmd\":\"help\"}' | nc -U ~/.config/dunkingsheep/dunkingsheep.sock"))
    w("`self_pane_id` is optional; it is what target \"self\" resolves to. Clients\n"
      "normally pass their own $HERDR_PANE_ID.\n")

    w(h(2, "Surface 3: MCP server (Claude Code, Codex, Gemini CLI, Cursor, ...)"))
    w("`dunkingsheep mcp` is a Model Context Protocol server over stdio (newline-\n"
      "delimited JSON-RPC, protocol versions 2024-11-05 through 2025-11-25, tools\n"
      "only, standard library only). Every registry command marked for MCP is a\n"
      "tool with a flat JSON schema (string/number/integer/boolean, no unions), so\n"
      "strict validators such as OpenAI's accept it. `shutdown` is not exposed.\n")
    w("`dunkingsheep mcp-setup` prints the exact registration for each harness and\n"
      "`dunkingsheep mcp-setup <harness> --apply` runs it when that harness has a CLI:\n")
    w(code("claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp\n"
           "codex mcp add dunkingsheep -- python3 /path/to/dunkingsheep mcp\n"
           "gemini mcp add --scope user dunkingsheep python3 /path/to/dunkingsheep mcp\n"
           "\n"
           "# any harness that reads an mcpServers JSON block (Cursor, Windsurf, Claude\n"
           "# Desktop, VS Code, ...):\n"
           '{"mcpServers": {"dunkingsheep": {"command": "python3",\n'
           '                                 "args": ["/path/to/dunkingsheep", "mcp"]}}}\n'
           "\n"
           "# ~/.codex/config.toml equivalent:\n"
           "[mcp_servers.dunkingsheep]\n"
           'command = "python3"\n'
           'args = ["/path/to/dunkingsheep", "mcp"]'))
    w("The harness spawns the MCP server inside the agent's own herdr pane, so the\n"
      "server inherits HERDR_PANE_ID and target \"self\" means the agent itself. If a\n"
      "harness scrubs the environment, pass `--self-pane <pane_id>` after `mcp` or\n"
      "set HERDR_PANE_ID in the server's env block.\n")
    w("Tools: help, status, list_panes, read_pane, list_dunks, get_dunk, add_dunk,\n"
      "update_dunk, remove_dunk, start_dunk, stop_dunk, stop_all, fire_dunk,\n"
      "send_text, send_keys, flush_box, list_messages, get_message, cancel_message,\n"
      "inbox_send, inbox_count, inbox_list, inbox_read, inbox_status, inbox_ack,\n"
      "inbox_cancel, inbox_delete, inbox_stats, inbox_fetch,\n"
      "and for herdr\n"
      "control list_workspaces, create_workspace,\n"
      "create_tab, split_pane, start_agent, rename, focus, close, herdr,\n"
      "herdr_help. Call `help` first if unsure; `initialize` also returns these\n"
      "instructions. Errors come back as tool results with isError=true and a\n"
      "plain-English message (never as protocol errors), so the agent can recover.\n")

    w(h(2, "Building the herd: herdr control from the same surfaces"))
    w("Every surface can also shape herdr itself, so an admin agent can spin up\n"
      "workspaces, tabs and agents and then dunk on them. Targets for workspaces\n"
      "and tabs follow the pane rules: id (w9, w9:t2), exact label, unique label\n"
      "substring, or 'self' (the caller's own). Everything is created in the\n"
      "background unless focus=true.\n")
    w("- list_workspaces: workspaces with their tabs; `self` marks the caller's.\n"
      "- create_workspace(label, cwd, command): a named space with its first tab\n"
      "  and shell pane; `command` (e.g. 'claude') is typed + Enter into that pane.\n"
      "- create_tab(workspace, label, cwd, command): a named tab; workspace\n"
      "  defaults to the caller's own.\n"
      "- split_pane(target, direction, cwd, command): a new shell pane right of or\n"
      "  below any pane.\n"
      "- start_agent(name, command, workspace|tab, split, cwd): launch an agent\n"
      "  process registered with herdr under `name` (herdr agent start), so herdr\n"
      "  lists it and tracks its status by that name. Returns its pane id. With\n"
      "  `workspace`, herdr picks the tab; create_tab first and pass `tab` to\n"
      "  control the tab's name.\n"
      "- rename(kind, target, label) / focus(kind, target) / close(kind, target)\n"
      "  for kind = workspace | tab | pane. close kills what runs inside, stops any\n"
      "  dunks aimed there, and refuses the caller's own pane, tab or workspace.\n"
      "- herdr(command): run any herdr subcommand ('pane zoom w9:p2 --on',\n"
      "  'notification show Done', 'wait agent-status w9:p2 --status idle\n"
      "  --timeout 60000', 'api schema --json'); returns its parsed JSON, or the\n"
      "  usage text for a bare group. 'self' in the arguments becomes the caller's\n"
      "  pane id (tab/workspace id in tab/workspace commands). herdr wants ids,\n"
      "  not labels, here. Refused: server stop, update, channel set,\n"
      "  session/agent attach.\n"
      "- herdr_help(topic): herdr's own usage, overall or for one group, to\n"
      "  discover the above. The `help` command also returns it as `herdr_help`.\n")
    w(code("dunkingsheep workspaces                       # what exists\n"
           "dunkingsheep new-workspace -l Site --cwd ~/site -r claude\n"
           "dunkingsheep new-tab -w Site -l writer -r 'codex --model gpt-5'\n"
           "dunkingsheep start-agent reviewer -w Site -- claude --model opus\n"
           "dunkingsheep split Site --down -r 'npm test -- --watch'\n"
           "dunkingsheep rename tab writer 'Site writer'   |  focus tab 'Site writer'\n"
           "dunkingsheep close tab 'Site writer'          # kills it, stops its dunks\n"
           "dunkingsheep herdr pane zoom self --on        # anything herdr can do\n"
           "dunkingsheep herdr-help pane                  # herdr's own usage"))
    w(h(2, "Recipe: an admin agent spins up a team and dunks on it"))
    w(code("create_workspace(label=\"Site\", cwd=\"~/site\")\n"
           "start_agent(name=\"builder\", command=\"claude\", workspace=\"Site\")\n"
           "start_agent(name=\"writer\", command=\"codex\", workspace=\"Site\")\n"
           "send_text(target=\"builder\", text=\"Implement the login page; tests first.\")\n"
           "add_dunk(target=\"builder\", interval_minutes=15, only_when=\"idle\",\n"
           "         skip_if_unconsumed=true, text=\"Status? Next step?\")\n"
           "add_dunk(target=\"writer\", interval_minutes=30, only_when=\"idle\",\n"
           "         text=\"Document what builder shipped since your last pass.\")\n"
           "# later, when the work is done:\n"
           "close(kind=\"workspace\", target=\"Site\")   # stops both dunks too"))

    w(h(2, "Surface 4: the TUI"))
    w("`dunkingsheep tui` (or ./run_dunking_sheep.sh) shows the same dunks live.\n"
      "Keys: j/k move, a add, d remove, c choose target, t test send, i interval,\n"
      "e text, n name, o toggle idle-gate, m max sends, space/s start-stop, q quit\n"
      "(dunks keep running), Q stop every dunk and shut the daemon down.\n")

    w(h(2, "Recipe: one agent watching and steering many"))
    w("Because every pane is addressable, one agent can supervise a herd:\n")
    w(code("list_panes()                                  # who is running where, idle or working\n"
           "read_pane(target=\"Codex Site\", lines=80)      # what is that agent doing?\n"
           "send_text(target=\"Codex Site\", text=\"Stop and write tests first.\")\n"
           "add_dunk(target=\"Melt Squad\", interval_minutes=20, only_when=\"idle\",\n"
           "         text=\"Progress report: what changed since the last one?\")"))
    w(h(2, "Direct messages queue behind the human, like dunks"))
    w("send_text never types over someone mid-sentence. If the target's input box\n"
      "holds a draft, the message waits in the daemon and is delivered once the\n"
      "box has been clear for a second; several messages to the same pane keep\n"
      "their order. The result says which happened: `queued: true` with a\n"
      "`message_id`, or `ok: true` when it went straight out. fire_dunk queues the\n"
      "same way when its dunk holds while typing.\n")
    w(code("send_text(target=\"Codex Site\", text=\"Stop and write tests first.\")\n"
           "  -> {\"message_id\": \"m3\", \"queued\": true, ...}   # they were typing\n"
           "list_messages()                  # what is waiting, and what just landed\n"
           "get_message(message_id=\"m3\")     # has it gone yet?\n"
           "cancel_message(message_id=\"m3\")  # never mind, drop it\n"
           "send_text(target=\"Codex Site\", text=\"...\", wait_s=30)  # block for delivery\n"
           "send_text(target=\"Codex Site\", text=\"...\", hold_while_typing=false)  # barge in"))
    w("Queued messages live in the daemon's memory, not on disk, so a daemon\n"
      "restart drops anything still waiting.\n")
    w("send_keys(target, keys) presses keys the same guarded way. Use it rather\n"
      "than the herdr passthrough: a bare Enter sent while someone is mid-sentence\n"
      "submits their half-written message. For that reason the `herdr` tool\n"
      "refuses `pane run`, `pane send-text`, `pane send-keys` and `agent send`.\n")
    w(h(2, "Getting the message actually submitted"))
    w("Pressing Enter once does not reliably submit. Measured on live panes:\n"
      "Claude Code submits on the first Enter whatever the size, but Codex needs\n"
      "the pane to have been quiet for a while first, and the bigger the paste the\n"
      "longer it needs. Raising the delay before Enter does not fix it on its own,\n"
      "and neither does a smaller paste chunk; pressing Enter repeatedly in quick\n"
      "succession does nothing either.\n")
    w("So the daemon confirms rather than guesses. After typing it reads the input\n"
      "box back, and while its own text is still sitting there it lets the pane\n"
      "fall quiet and presses Enter again, with gaps that grow each time. A blind\n"
      "double Enter would be unsafe: if the first one worked, the second would\n"
      "submit whatever you typed next.\n")
    w("The same confirmation covers a bare Enter sent to submit text already\n"
      "waiting in the box, which is how agents that type and submit in two steps\n"
      "were leaving messages stranded.\n")
    w("What it costs, measured on Codex: about 0.4s for a short message, 4s at\n"
      "600 characters, 10s at 1.2k and 19s at 2.2k. Claude is about a second at\n"
      "any size. Only the confirmation is slow, and only while a send is actually\n"
      "sticking.\n")
    w("Two consequences worth knowing. A send that never leaves the box is\n"
      "reported as failed rather than sent, and shows up in last_error and in\n"
      "`messages`. And text stranded in a box by an earlier failure is recognised\n"
      "as ours: the next send submits it first rather than appending to it or\n"
      "waiting behind it for ever. For text the daemon did not type (an agent\n"
      "using the herdr CLI, or someone who walked away mid-sentence), which would\n"
      "otherwise block that pane's queue indefinitely, `flush_box(target)` /\n"
      "`dunkingsheep flush <target>` submits it explicitly.\n")

    w(h(2, "Inboxes: messaging that never types"))
    w("Typing into a terminal is the human's channel, and an unreliable one for\n"
      "machines: Enter does not always submit, long pastes strand, and a message\n"
      "in flight can collide with someone mid-sentence. For agent-to-agent\n"
      "traffic, use an inbox instead. A message is stored for the recipient to\n"
      "read when it chooses, so none of those failures exist. There is no size\n"
      "limit, messages survive a daemon restart, and they can carry files.\n")
    w("The only thing typed is one short line telling the recipient it has mail,\n"
      "which is the one send short enough to always submit. Pass notify=false to\n"
      "skip even that.\n")
    w(code("inbox_count()                          # how many am I holding?\n"
           "inbox_read()                           # read them, oldest first\n"
           "inbox_send(to=\"reviewer\", body=\"...\", subject=\"PR 12\")\n"
           "inbox_send(to=\"reviewer\", body=\"...\", attach=\"/tmp/patch.diff\")\n"
           "inbox_status(message_id=\"i42\")         # where is that message?\n"
           "inbox_ack(message_ids=\"i42\")           # done with it\n"
           "inbox_stats()                          # who has mail piling up?\n"
           "\n"
           "dunkingsheep inbox count | read | list | stats\n"
           "dunkingsheep inbox send reviewer 'PR 12 is ready' -s review -a /tmp/p.diff\n"
           "dunkingsheep inbox status i42 | ack i42 | cancel i42 | delete i42\n"
           "dunkingsheep inbox fetch <attachment-id> -d ."))
    w("A mailbox identifies one pane: its own label first, since that is a name\n"
      "someone chose, then its tab label, then its pane id. Whichever is taken\n"
      "has to belong to exactly one pane. The agent *type* is never a mailbox,\n"
      "so 'codex' is refused rather than pooling every Codex pane together.\n"
      "`self` means your own. A name matching no running pane still works, so\n"
      "mail can be left for an agent that has not started. Renaming a pane or\n"
      "tab renames its mailbox, so mail already waiting stays under the old name.\n")
    w("Every send returns a stable message id. `inbox_status` reports where that\n"
      "message is - queued, read, acked or cancelled - with its destination,\n"
      "sender, timestamps, size, attachment count and any failure reason. It\n"
      "carries neither body nor subject, so a status board can poll it without\n"
      "seeing the contents, and it answers across daemon restarts.\n")
    w("Read, acknowledged and cancelled mail is kept for a week and then purged.\n"
      "Unread mail is never purged by age. Attachments are stored by content\n"
      "hash and deleted with the last message that referenced them.\n")
    w("A notice is never stacked. If one is still waiting for a pane when more\n"
      "mail arrives, it is updated to the new unread count rather than a second\n"
      "line being queued behind it: an agent that reads its inbox once has dealt\n"
      "with all of it. If our own text is sitting unsubmitted in the way, the\n"
      "daemon clears it within 30 seconds rather than waiting on itself; a human's\n"
      "draft is still never touched. A draft has to be there on two readings\n"
      "before a send is held, so a frame of an agent redrawing its pane is not\n"
      "mistaken for someone writing.\n")
    w("`inbox_status` tracks the notice too: it says queued while the notice is\n"
      "waiting and sent once it lands, including for messages whose notice was\n"
      "folded into another.\n")
    w("A standing dunk is the easy way to get an agent checking: point one at the\n"
      "agent with text like \"call inbox_count; if anything is waiting, read and\n"
      "act on it\".\n")

    w(h(2, "Setting an agent up to use its inbox"))
    w("Two things make the inbox reachable from an agent's session.\n")
    w(code("dunkingsheep install     # put the CLI on PATH as a plain command\n"
           "dunkingsheep mcp-setup claude --apply   # register the MCP server"))
    w("`install` matters more than it looks. herdr starts agents by exec'ing them\n"
      "from its server process, so they inherit that environment: ~/.local/bin is\n"
      "on their PATH but this project is not. Without the launcher an agent has to\n"
      "invoke the CLI by its path inside a workspace, which a sandboxed agent\n"
      "should refuse and does - Claude's auto mode reads it as running external\n"
      "code. A plain `dunkingsheep inbox read` is the supported shape, and an\n"
      "allow rule for `Bash(dunkingsheep *)` in ~/.claude/settings.json lets auto\n"
      "mode run it without prompting.\n")
    w("An agent whose MCP connection predates the inbox has a stale tool list and\n"
      "will not see inbox_read until it reconnects. The CLI works meanwhile, which\n"
      "is why the launcher is the first thing to install.\n")

    w(h(2, "Panels and dialogs"))
    w("A slash command such as /usage replaces the input box with a panel, and a\n"
      "dialog draws its selected row with the same prompt sigil the input box\n"
      "uses. Both used to read as occupied input: a /usage send was reported\n"
      "failed although it had run, three further Enter presses landed on the\n"
      "panel, and an Escape meant to dismiss it queued behind what looked like\n"
      "someone typing.\n")
    w("The daemon now recognises an open panel. A send whose text opened one is\n"
      "finished, not stuck. Text waits for the panel to go rather than typing\n"
      "into it, while key presses go straight through, since keys are how a panel\n"
      "is dismissed.\n")
    w("A panel that is left open would otherwise block its pane indefinitely: the\n"
      "queue check does not see a panel as a draft, so it keeps saying the pane is\n"
      "free while every send is refused. After 30 seconds open and unchanged, with\n"
      "something waiting, the daemon presses Escape, which is the key the panel\n"
      "itself advertises. A panel somebody is navigating keeps changing, so it is\n"
      "left alone, and a human's draft is never a panel.\n")

    w(h(2, "The herdr guard: agents that shell out"))
    w("An agent with a shell can call the herdr CLI directly, and `herdr pane run\n"
      "<pane> \"...\"` types straight into a terminal with no idea anyone is writing\n"
      "there. Nothing inside this daemon can see that. `dunkingsheep guard install`\n"
      "puts a small `herdr` wrapper first on PATH which forwards exactly the four\n"
      "typing subcommands to the daemon and execs the real herdr for everything\n"
      "else, so agents get the same protection without changing how they work.\n")
    w(code("dunkingsheep guard status      # installed? active in this shell?\n"
           "dunkingsheep guard install     # wrapper + a PATH line in ~/.bashrc\n"
           "dunkingsheep guard check       # prove a typing command is intercepted\n"
           "dunkingsheep guard uninstall   # remove both"))
    w("Installing does two things. It puts the wrapper first on PATH for new\n"
      "shells, and it takes herdr's own place: the real binary is renamed to\n"
      "`herdr-real` beside it. That second part matters, because herdr starts\n"
      "agents by exec'ing them from its server process, so they inherit the\n"
      "server's environment and never read a shell profile. Shadowing the binary\n"
      "is what covers agents that are already running.\n")
    w("Set HERDR_GUARD_OFF=1 for a single command that must bypass it. If you\n"
      "update herdr, it replaces the shadowed binary, so run `dunkingsheep guard\n"
      "install` again afterwards; `guard status` shows whether it is in place.\n")
    w("send_text is a direct message with no schedule attached; read_pane is the\n"
      "eyes. Together with add_dunk they let an agent communicate with, monitor and\n"
      "pace several projects in parallel.\n")

    w(h(2, "Recipe: meta-dunking (an agent schedules itself)"))
    w("An agent with the MCP tools can implement a backlog under external control:\n")
    w(code("add_dunk(target=\"self\", interval_minutes=60, only_when=\"idle\",\n"
           "         skip_if_unconsumed=True, name=\"backlog\",\n"
           "         text=\"Backlog check #{count}: read BACKLOG.md. If every item is done,\\n\"\n"
           "              \"call remove_dunk('{id}') and stop. Otherwise implement the next\\n\"\n"
           "              \"item, commit, and update BACKLOG.md.\")"))
    w("Every hour the daemon types that prompt into the agent's pane (waiting for\n"
      "it to be idle first, and skipping the hour entirely if the previous prompt\n"
      "is still sitting unread). The prompt carries its own dunk id, so the future\n"
      "agent can end the loop by removing the dunk. The same agent can read other\n"
      "panes (read_pane), nudge other agents (send_text), or put them on their own\n"
      "schedules (add_dunk with their pane as target).\n")

    w(h(2, "Troubleshooting"))
    w("- \"herdr server not running\": start herdr (`herdr status server`).\n"
      "- \"no herdr pane matches\": run `dunkingsheep panes` and use the pane id.\n"
      "- \"target 'self' needs HERDR_PANE_ID\": the caller is not inside a herdr\n"
      "  pane; pass a pane id or `--self-pane w8:p3`.\n"
      "- Daemon problems: ~/.config/dunkingsheep/daemon.log; `dunkingsheep shutdown`\n"
      "  then any command restarts it with the saved dunks.\n"
      "- Send failed: the pane was closed. Re-target with `update <id> -T ...`.\n")
    return "".join(out)


def _underline(text, level):
    char = {1: "=", 2: "-"}.get(level, ".")
    return f"{text}\n{char * len(text)}\n"


if __name__ == "__main__":
    import sys
    print(guide(markdown="--markdown" in sys.argv), end="")
