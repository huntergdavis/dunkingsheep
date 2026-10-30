# Dunking Sheep

A deterministic operating system for a fleet of coding agents on one machine.
One owner, one front desk, a task queue, and GitHub as the gate. No managers.

The agents (Claude Code, Codex, pi) each run in a tmux pane. The daemon owns
every byte of shared state and every rule of who may do what; the agents pull
work from it, report through it, and never talk to each other outside it.

## What it does for the owner

- **Ask one thing.** The front desk (`/dash/`) answers from the board, the
  metrics and the task notes, in seconds. If an answer needs work, it files a
  task and tells you the id. Decisions lanes owe you are at the top.
- **Work moves only through the queue.** `dunkingsheep task next` is the only
  way a lane gets work. Unassigned means claimable; nobody hands work out.
- **All code is reviewed, by construction.** A timer files one review task per
  PR head; a reviewer lane that did not author it judges it; the reviewer App
  approves on GitHub; branch protection merges. Authors cannot approve.
- **Rules come only from you.** Agents write facts; only the owner writes
  ground truths, and the daemon is the only path that records them.
- **Play.** `/dash/play` runs Pool of Radiance with save states and the code
  wheel, from the same tree.

Design: [`docs/FLAT_FLEET.md`](docs/FLAT_FLEET.md). Desk design:
[`docs/DESK_DESIGN.md`](docs/DESK_DESIGN.md). Lane briefs:
[`docs/LANES.md`](docs/LANES.md).

## Roles

Every pane has a role, bound to its root process when the owner launches it;
a label never grants anything. The allowlist is enforced once, in the daemon.

| role | may | may not |
| --- | --- | --- |
| `owner` | everything | - |
| `desk` | read the board, file work, answer the owner | type, mail lanes, claim, complete |
| `worker` | pull work, mail, note, open PRs as the author App | assign others, change status, review |
| `reviewer` | pull reviews it did not author, record a verdict | push code, review its own author's work |
| `planner` | write task bodies | everything else |
| `web` / `system` | the desk's and the timers' service tokens | fleet-destructive verbs |
| `anonymous` | reads | writes |

```
dunkingsheep whoami
dunkingsheep lane register --role owner           # first owner self-registers (bootstrap)
dunkingsheep start-agent worker-1 --role worker --provider claude -- claude ...
dunkingsheep token issue --role web --label desk  # for a pane-less service
```

## The queue

```
dunkingsheep task next                            # claim the next task your role may take
dunkingsheep task note t12 --reason "evidence"    # commands run, sha, what is NOT verified
dunkingsheep task complete t12                    # reviews: --verdict accept|needs-fix
dunkingsheep task block t12 --reason "needs-owner: which port?"   # the one way to ask the owner
dunkingsheep task list --needs-owner
dunkingsheep task metrics --window 86400 --bucket hour
```

Tasks carry a `kind` (`work`, `review`, `verify`, `report`). A review is keyed
by repo, PR and exact head; a new push is a new review. The store refuses a
duplicate.

## GitHub

Two GitHub Apps owned by your login: `dunkingsheep-author` (lanes push and open
PRs as it) and `dunkingsheep-reviewer` (approvals and the `dunkingsheep/review`
status). The daemon holds the keys and mints one-hour tokens per request;
lanes see them only through `gh` and `git`, which are wrapped.

```
dunkingsheep github wrappers                       # gh wrapper + git credential helper
dunkingsheep github protect huntergdavis/<repo> --approvals 1
dunkingsheep review sync | review list
dunkingsheep gh-token                              # empty for the owner, by design
```

## Context: facts and rules

```
dunkingsheep context digest                        # injected at every session start
dunkingsheep context add --class fact --authority measured --source <cmd> --text "..."
dunkingsheep context promote <slug>                # owner only
dunkingsheep context check
```

Ground truths are injected only when the daemon wrote them (the ledger);
a file dropped in by hand is named by `check` and ignored.

## Install

```
git clone https://github.com/huntergdavis/dunkingsheep
cd dunkingsheep && ./dunkingsheep install                       # launcher on PATH
cp packaging/systemd/*.service packaging/systemd/*.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now dunkingsheep dunkingsheep-web dunkingsheep-review.timer dunkingsheep-poolrad.timer
dunkingsheep lane register --role owner             # from your own tmux pane
dunkingsheep token issue --role system --label timers > ~/.config/dunkingsheep/system-token
dunkingsheep token issue --role web --label desk     > ~/.config/dunkingsheep/web-token
```

Releases are immutable trees under `~/.local/share/dunkingsheep/releases/<sha>`
(`tools/t514_rollout.sh stage|activate|verify|rollback`); the web desk runs
from the same tree (`bridges/run.sh` builds its venv and `ui/dist` on first
start). Secrets (`github-app-*.pem`, tokens, `desk/telegram.json`) live in
`~/.config/dunkingsheep/`, never in git.

## Layout

```
dunk_*.py        daemon: socket, tasks, inbox, roles, review timer, context, memory
dunkingsheep     CLI (and MCP: `dunkingsheep mcp`)
web/             FastAPI desk; ui/ the React app; bridges/ telegram, host watch, ops board
integrations/    harness hooks (codex, pi, opencode); packaging/systemd/ units and timers
docs/            FLAT_FLEET.md (the design), DESK_DESIGN.md, LANES.md, GUIDE.md (generated)
tests/           python3 -m unittest discover -s tests
```

## What it does not enforce

Every lane runs as the same Unix user as the daemon and as your own `gh` login.
The rules above stop confusion, generalisation and bugs; they do not stop a lane
that sets out to read your credentials. `docs/FLAT_FLEET.md` 4.4 says exactly
where that line is.
