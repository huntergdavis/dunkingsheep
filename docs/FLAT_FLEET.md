# Flat Fleet: the daemon is the org

**Status:** accepted by the owner 2026-10-29 (chat, this session). Supersedes the
CTO -> Chief of Staff -> VP of Agentry -> lanes model, `docs/PM.md`, the VP/CoS
role briefs in `vp-of-agentry/` and `chief-of-staff/`, and the four ground truths
listed in section 8.

**One line:** remove every LLM in the management path; the Dunking Sheep daemon
routes work deterministically, GitHub enforces review, one tool-gated front desk
answers the owner, and the owner alone writes rules.

## 1. Why (measured, not claimed)

The hierarchy was copied from a human org. Each property that makes it work for
people is one agents lack, and each cost is one agents amplify.

| Property | Humans | Agents |
| --- | --- | --- |
| Span of control | can't scale horizontally, so layers | scale trivially; a layer is a hop |
| Context | a manager accumulates it | every hop is a cold context window |
| Latency budget | hours of relay vs days of work | seconds of reply vs minutes of relay |
| Attention | a person can answer *and* work | a pane is single-threaded |
| Rules | judged, quietly ignored when dumb | followed literally and generalised |

Numbers from the live stores on 2026-10-29 (owner desk `messages.sqlite3`,
`inbox.db`, `tasks.db`, `dispatch-state.json`):

- Owner reply latency since 10-25: p50 61 s, p90 13 min, p99 days.
- Mail CoS->VP 769 and VP->CoS 488 in 7 days (~180/day). 43% of all 12,835
  fleet messages were addressed to the two managers; the VP mailbox averaged
  32 min send-to-read.
- VP+CoS created 268 of ~519 tasks; the VP also *completed* the most (61).
- 34 blocked tasks, nearly all "awaiting VP/owner direction".
- Dispatch: 16 tasks available, 0 dispatched in the last hour, 5 lanes enrolled
  of 35 allowlisted.
- ~12 of 58 panes were canaries and wiring lanes for "finish a task, get the
  next one" (t372/t374/t377/t400): the fleet was building the machinery the
  managers were meant to be.
- The "restart needs both Hunter and CoS to sign off" rule was an agent-written
  stop condition on its own 2-minute self-nudge (d82), carried forward by a
  handoff file after the owner overrode it twice. The context store accepted
  `type: rule, authority: agent`; `context promote` checked a self-declared env
  var; `self_pane_id` was whatever the client sent.

The recurring defect of the week, "claim one step wider than evidence" (ten
recorded instances), is the telephone game: lane -> VP -> CoS -> owner, each a
fresh model rounding "I did a thing" up toward "it is done".

## 2. Principles, each with a mechanism

A principle without a mechanism is a prompt, and prompts are where "I'll just
quickly look into this" comes from.

1. **Routing is deterministic.** Work moves only through the task queue. No
   agent assigns work to another agent by mail. *Mechanism:* roles (section 4)
   forbid `inbox send` to lanes from the desk; `task add` with assignees other
   than self is `owner`/`system` only.
2. **Identity is kernel-bound.** A pane's role is what the daemon verifies from
   process ancestry (`caller_pane_auth_failure`), never what the client claims.
3. **Rules come only from the owner.** Agents write facts with sources. The
   context store refuses anything else.
4. **Review is a property of the repository.** GitHub refuses the merge; no agent
   has to remember. Author and approver are different GitHub identities.
5. **The owner talks to one thing, and that thing cannot do work.** Enforced by
   the command allowlist, not by its brief.
6. **One repo, one package, one reviewed release, one state directory;
   separate processes; the daemon owns every byte of state.**

## 3. Target shape

```
OWNER --Telegram / web--> FRONT DESK (one pane, role=desk, fast model)
                              | task add / task read / owner reply
                              v
                     DUNKING SHEEP DAEMON  <-- the org: queue, leases, push-on-idle,
                      |        |        |        review timer, reporter, watchdog
                      v        v        v
                   WORKER   WORKER   REVIEWER lanes (pull; never relay; never merge)
                              |
                   GitHub: branch protection + two GitHub Apps enforce
                           "reviewed, author != approver"
```

Gone: VP, CoS, PM layer, the 4-hour LLM brief, manager-to-manager mail,
agent-written rules, the separate `vp-of-agentry` repo and release pipeline.

## 4. Identity and roles

### 4.1 Binding (prerequisite for everything)

Today `dunk_server.handle_line` trusts `self_pane_id` from the client;
`--self-pane` returns early in `dunk_client.resolve_identity`; and the real
ancestry check (`dunk_callers.caller_pane_auth_failure`) is used only for turn
events. Any lane can claim `%15`.

- In `dunk_api.dispatch()`: resolve the caller's pane from the peer pid's
  ancestry. If the claimed id disagrees, the bound id wins and the mismatch is
  recorded in `metrics.db` (`identity.mismatch`).
- A caller with no TTY (systemd units) is role `system`. A caller with a TTY
  that resolves to no pane is role `anonymous` (read-only), not refused.
- `--self-pane` becomes an *assertion* that must match, never a source.

### 4.2 Roles file

`~/.config/dunkingsheep/roles.json`, same shape as `superusers.json`:

```json
{"panes": {"%95": "owner", "%96": "desk"},
 "names": {"owner": "owner", "desk": "desk", "pi-worker-*": "worker", "pi-review-*": "reviewer"},
 "default": "worker"}
```

Name patterns match pane label, tab label or workspace label, case-insensitive,
`fnmatch`. `superusers.json` is retired; `owner` is the superuser role.

### 4.3 Allowlist (enforced in `dunk_api.dispatch`, next to `SUPERUSER_ONLY`)

| Role | May call | Refused |
| --- | --- | --- |
| `owner` | everything | - |
| `system` | timers and bridges: `task add/assign/cancel`, `inbox send --system-sender`, `respawn_pane`, `send_keys` under the stuck/watchdog guards, `report`, `owner_*` | interactive typing |
| `desk` | `task add` (unassigned only) / `list` / `read` / `update` / `note` / `metrics`, `inbox read` (own mailbox), `owner_take`, `owner_reply`, `context digest` / `search`, `metrics summary`, `pane read`, `views *`, `memory recall/search` | `send_text`, `send_keys`, `inbox send`, `task next/claim/complete/heartbeat/release`, every dunk verb, `respawn`, `start_agent`, `context add`, `gh_token` |
| `worker` | `task next/heartbeat/complete/note/block/release/follow`, `inbox read/send`, `context add --class fact`, `memory *`, `pane send self`, `gh_token author` | `task add` with other assignees, `task complete` on a task it does not lease, anything superuser, `gh_token reviewer` |
| `reviewer` | as worker, except `task next` yields only `kind=review` with `author != self`, and `gh_token reviewer` not `author` | pushing code: the reviewer App has `contents: read` |
| `web` | the owner's transport: everything `owner` may call except the fleet-destructive verbs (`close`, `shutdown`, `respawn_pane`, `start_agent`, every dunk verb, `superuser *`, `gh_token`). Bound by a service token, not a pane (below). | the excluded verbs |
| `anonymous` | `views *`, `task list/read`, `metrics summary`, `memory recall` | all writes |

**How a process with no pane proves a role.** systemd units have no TTY and no
pane, so roles for them are bound by a service token: `~/.config/dunkingsheep/
service-tokens.json` maps `sha256(token) -> role` (`web`, `system`), the unit
reads its token from a 0600 file (`web-token`, `system-token`) and sends it as
`service_token` on every request. A request with a valid token gets that
role; a no-TTY request without one is `anonymous`. The old desk credential
(`dunk_desk.py`, `desk_token`, `--desk-token`) is retired: it was this
mechanism for one verb pair, and its name collides with the desk role.

Also closed: `inbox read --mailbox X` for another mailbox (`_mailbox_or_self`),
`task complete` by a non-lease-holder, ungated `owner_push`.
`dunk_mcp.list_tools` filters by role so the desk never sees a typing tool; the
dispatch check is what enforces it.

## 5. Tasks

- Migration: add `kind` (`work|review|verify|report`, default `work`) and
  `author` (lane that produced the artefact under review, or null).
- **Unassigned means claimable** by any lane whose role matches the kind
  (`_claimable`). `_default_task_owner` no longer makes the creator the owner.
  The ground truth `unowned-means-leader-owned` is retired: the queue owns.
- Ordering: priority, then age. Default priority `low` (owner #2361), so the
  backlog is a strict ranking with no "unset" state.
- `Tasks._record` gains an `on_event` list; the daemon reacts to `created` /
  `completed` without polling.
- The only way a lane asks the owner anything is
  `task block --reason "needs-owner: <question>"`. The reporter and the desk
  surface those as "Decisions I need"; the owner's answer unblocks with the
  reply as the note. No mail to a manager, no manager relaying.
- Information flows lane -> owner through `task note` and `task complete`,
  which the desk reads. No inbox hop.

## 6. Review enforced by GitHub, fed by a timer

### 6.1 Two GitHub Apps, one login

The owner has one GitHub login and wants no sub-accounts. A GitHub App is a bot
identity owned by that login, not an account. Two apps:

| App | Used by | Permissions |
| --- | --- | --- |
| `dunkingsheep-author` | worker lanes | contents: write, pull_requests: write, workflows: write, metadata: read |
| `dunkingsheep-reviewer` | reviewer lanes and the review timer | contents: read, pull_requests: write, **commit statuses: write**, metadata: read |

The reviewer App posts the `dunkingsheep/review` commit status, which needs
`statuses: write`; branch protection pins that required check to this App so
no other source can satisfy it. (Created 2026-10-29 as ids 5531864 / 5531872
without `statuses`; the owner adds the permission on the App page and accepts
it on the installation.) Whether an App's approval satisfies "require
approving reviews" is verified live in step 5 before anything depends on it.

Cost: $0 (apps are free; branch protection is free on public repos; all three
repos are public). Limits: 100 apps per account; 5,000 API requests/hour per
installation, one bucket per app; installation tokens last one hour and are
minted on demand, so nothing long-lived sits in a pane.

The daemon holds the two private keys (`~/.config/dunkingsheep/github-app-
{author,reviewer}.pem`, mode 0600, never in git) and exposes `gh_token <app>`,
role-gated as in section 4.3. Lanes never see a key:

- `git`: a credential helper (`dunkingsheep git-credential`) asks the daemon
  for a fresh token for the lane's role.
- `gh`: the PATH-wrapper pattern already used for `tmux` sets `GH_TOKEN` from
  the same call.

New dependency (dependency-rule): RS256 JWT signing for the app. `PyJWT`
2.10 and `cryptography` 46 are both installed as Ubuntu apt packages on this
host (`/usr/lib/python3/dist-packages`), which the daemon's `/usr/bin/python3`
sees; `requirements.txt` stays stdlib-only and the import is guarded with a
clear error naming the apt package.
Boring fallback: a fine-grained PAT with a one-year expiry, which is the owner's
own login again and therefore loses the author != approver guarantee. The ADR
records that trade; the fallback is for an outage, not a design choice.

### 6.2 Branch protection

On `dunkingsheep` and `poolrad-decomp` (`vp-of-agentry` is archived): require
one approving review, dismiss stale approvals on push, require the
`dunkingsheep/review` status, auto-merge enabled, no bypass for anyone. GitHub
itself refuses an app approving its own PR, so author != reviewer is enforced
by GitHub, not by a prompt. `no-self-merge` and `no-self-credit` stop being
rules and become refused API calls.

### 6.3 Review timer

`dunkingsheep-review.timer` (same shape as the dispatch timer, interval 2 min
instead of 5) runs
`dunkingsheep review-sync`:

1. `gh pr list --json number,headRefOid,author,isDraft,baseRefName` per repo.
2. For each open, non-draft PR with no `kind=review` task for
   `(repo, pr, head)`: `task add --kind review --author <lane> -p high` with
   body = repo / PR / head / CI state / diff stat. A new head cancels the old
   task and files a delta-review task. Dedupe key is the head SHA, so a re-push
   is re-reviewed: "all code is reviewed" exactly.
3. When a review task completes with verdict `accept`, the timer posts the
   `dunkingsheep/review` success status on that head (as the reviewer App).
   With `needs-fix`, it posts failure and files a `kind=work` follow-up for the
   author lane.

Reviewer lanes `task follow`; the daemon pushes review tasks to idle reviewers.

## 7. The front desk and the reporter

### 7.1 Front desk (replaces the CoS)

One pane, role `desk`, a fast model (DeepSeek 4.1 Flash via pi, or Haiku); the
job is reading, not reasoning. Its whole brief:

> You answer the owner within 20 seconds from what the board, the metrics, the
> digest and task notes say. If the answer requires work, file a task
> (`task add`, priority from the owner's words, body = his words verbatim) and
> reply with the task id. You cannot message lanes, type into panes, or complete
> tasks; the daemon refuses those calls, so do not try. Never say "done" unless
> a task's `completed_by` and note say so.

Loop: `owner take` -> answer -> optional `task add` -> `owner reply`. Target:
p50 < 20 s, p90 < 60 s. Because the allowlist forbids every slow path, it
cannot pick up work.

### 7.2 Owner channel becomes a mailbox

`vp-of-agentry/data/messages.sqlite3` (role=owner/agent, `answered_at`) is a
second inbox with its own schema. It becomes mailbox `owner` in `inbox.db`:
an owner message is a row; a reply is an `ack` linked by `reply_to`; Telegram
and the web are two transports for one mailbox. Owner-reply latency is then the
same metric as lane-read latency and the watchdog alarms on it with no special
code.

**Reply on the transport the question arrived on (owner, 2026-10-29).** Each
owner message records its `transport` (`telegram` | `web`); a desk reply is
delivered only on that transport. Telegram carries answers to Telegram
questions and nothing else: no web-conversation echoes, no reports, no
watchdog digests. Pages (security / outage / money, max 3 per hour, per
`WATCHDOG_ESCALATION_SPEC.md`) remain the one exception. The web conversation
shows everything, including Telegram exchanges, so the web is the complete
record and Telegram is the quiet one.

Telegram mechanics after the merge: the pairing state (`bot_token`,
`chat_id`, `update_offset`) moves from `vp-of-agentry/data/telegram.json` to
`~/.config/dunkingsheep/telegram.json` unchanged; `channels.json`,
`telegram-routing.json` and `telegram-status.json` are retired, because the
transport column replaces channel routing and the watchdog confirms a page by
the `owner` mailbox's delivery receipt on the `telegram` transport instead of
reading a status file. The 2,439 historical rows (744 owner, 1,695 agent) are migrated so the
latency baseline survives; attachments move to the inbox blob store.

### 7.3 Reporter (replaces the VP)

`dunkingsheep report` is deterministic and has no model: task metrics, lanes
working/idle/dead, `needs-owner` list, budget projection, watchdog digest, PRs
awaiting review and their age, Poolrad credited/replaced counts. A timer runs it
every 4 hours; the desk may run it on demand. It posts to the `owner` mailbox
on the `report` transport, so Telegram keeps working. Optionally the text is
piped through a one-shot `claude -p "ELI5 bullets"`: a process that exits, not
a pane with an inbox, opinions and rule-writing power.

Three separate mechanisms address the VP mailbox today and each gets its own
edit: `host_watch.py` (literal `"π - vp-of-agentry"` via subprocess),
`dunkingsheep-watchdog-deadman.service` (`--to "vp of agentry"`), and
`escalation.vp.mailbox` in `~/.config/dunkingsheep/watchdog.json`. All three
go to the `owner` mailbox on the `report` transport. The consumers of
`DUNKINGSHEEP_VP_PANE` (`dunk_pm.py`, `dunk_tasks.py` leader_held,
`dunk_core.py`, `dunk_superusers.py`) are removed with the PM layer.

## 8. Context store governance

The context store today never crosses the socket: the CLI writes markdown
files directly (`dunkingsheep` `context` subcommands, `dunk_context.Store`),
so nothing in `dispatch()` can see it, and a lane can `cat > ground-truth/x.md`.
Every lane runs as the same Unix user, so the filesystem cannot be the
boundary either. The honest design is three layers:

- **Writes go through the daemon.** `context_add / promote / retire /
  supersede` become daemon commands; the CLI calls them over the socket; the
  role check lives in `dispatch()` like every other write.
- **The daemon stamps what it writes.** Each entry it creates carries an
  HMAC over (slug, class, authority, body) keyed by a daemon secret in
  `~/.config/dunkingsheep/context-key` (0600). The digest injects a ground
  truth only when its stamp verifies and `authority: owner`; `context check`
  reports every unstamped or mis-authored entry as a violation with its path.
- **Rules of the road.** `context add` refuses `--class ground-truth`,
  `type: rule`, and `authority: owner` unless the bound role is `owner`.
  `TYPES` is validated. `context promote` checks the bound role, not
  `DUNKINGSHEEP_SUPERUSER=1`.

A lane that bypasses the daemon can still drop a file; it will not be injected,
and `check` names it. That is the strongest guarantee a same-uid fleet allows,
and the ADR says so rather than claiming more.
- Every ground truth carries `recheck_after`; the digest marks it STALE like a
  fact. Rules are scoped ("until t130 is live" is the right shape).
- Fix the two ground truths with broken front matter
  (`delivery-no-typing-hold`, `owner-prefers-ascii-separators`) that are
  silently dropped from every digest today.
- Retire: `unowned-means-leader-owned` (queue owns), `brief-cadence` (a timer),
  `lane-duty-press-enter` (stale fact), `review-assignment-rule`
  (agent-authored, moot). Keep: `no-self-merge` / `no-self-credit` (now
  mechanical), `pane-safety`, `no-reboot`, `production-stays-installed`,
  `dependency-rule`, `ceilings`, `free-models-worker-tier`.

## 9. One repo

### 9.1 Layout

```
dunkingsheep/                 one repo, one package, one reviewed release
  dunk_*.py                   daemon (unchanged module names; nothing imports web/)
  web/                        FastAPI desk + terminal gateway  <- vp-of-agentry/server, herdr proxy
  ui/                         Vite/React app                    <- vp-of-agentry/web
  play/                       gameplay, character editor, code wheel
  bridges/                    telegram, host_watch, free-catalog
  integrations/               codex / pi / opencode hooks (as today)
  packaging/systemd/          dunkingsheep.service, dunkingsheep-web.service,
                              dunkingsheep-telegram.service, timers
```

### 9.2 Rules that make the merge pay

1. **The web process speaks to the daemon only over the socket.** No
   `subprocess(["dunkingsheep", ...])`, no `sqlite3.connect(inbox.db)`. Every
   read the UI needs is a daemon command, so the role allowlist applies to the
   UI exactly as to an agent. Missing reads are added to the daemon; that is
   the migration work.
2. **Separate processes.** The daemon is the control plane; when it breaks,
   every lane goes blind (2026-10-28 outage). The web layer streams DOSBox
   video and proxies terminals and changes ten times as often. Web code is
   never in the daemon's import path (a test asserts it).
3. **One release pipeline.** The reviewed-release install
   (`~/.local/share/dunkingsheep/releases/<sha>/`) covers the web too; a release
   is `<sha>` plus a built `ui/dist`. "Merged but not deployed" becomes one
   state, `code_stale`, that one command (`dunkingsheep release install <sha>`)
   clears for everything.
4. **`data/` moves to `~/.config/dunkingsheep/`**: `auth.json`,
   `telegram.json`, `operations.sqlite3` (subscription costs), release dirs.
   Secrets stay out of git.
5. **Keep with a daemon command:** Poolrad credited/replaced
   (`integrations/poolrad-dashboard`, today rendered by the web process via
   `git show`) becomes `poolrad progress`; the budget projection
   (`ops_budget.project`, today reading `operations.sqlite3` written by
   `opsctl collect`) becomes `usage project`, with the subscription-cost
   table moved into the daemon's state directory. The web's own
   `sessions` table lives in `web.sqlite3`, the one SQLite file `web/` may
   open.
6. **Drop, don't port:** the VP/CoS channels; `vpctl brief` (-> `dunkingsheep
   owner reply`); `opsctl` / `ops_metrics` (overlaps `dunk_metrics`; one
   survives); `vp-terminal-gateway` (its session check folds into the web
   process); unrendered components (`OpsSummary`, `RecreationLab`,
   `CoverageEconomics`, `LineTrend`, `QuotaCard`, `Search`,
   `ForceSendDialog`). **Keep:** Play, character editor, themes as a system,
   host_watch, budget projection.
7. `vp-of-agentry` is archived on GitHub after cutover, not deleted.
8. **Environment that must carry into `dunkingsheep-web.service`:**
   `VP_DOSBOX_X` (the pinned DOSBox-X build), `POOLRAD_EDITOR_TOOL`, the
   terminal URL. `dunkingsheep release install <sha>` is new work: today a
   release is a hand-pointed systemd drop-in for the daemon and a separate
   `vp-site-releases/<sha>-prep/launch.sh` for the web; the command replaces
   both.

## 10. Rollout (serial, each step verifiable, all in worktrees)

| Step | Work | Verified by |
| --- | --- | --- |
| 0 | This document and `docs/DESK_DESIGN.md` merged | PR reviewed by a non-author |
| 1 | Task up everything in the queue with bodies | `task list` shows bodies and dependencies |
| 2 | Drain and stop: every lane `task note` + `release` (2-minute drain), then stop; delete all dunks; retire panes | `dunk-poll`: 0 running dunks, 0 agent panes but owner + desk |
| 3 | Identity binding + roles allowlist | a lane calling `--self-pane %15` is bound to its own pane; a `desk` pane calling `send_text` is refused (tests) |
| 4 | Task `kind`/`author`, claimable-by-role, `on_event`, default `low` | migration test; `task next` as reviewer never returns own-author work |
| 5 | GitHub Apps wired: `gh_token`, credential helper, branch protection, review timer | unit tests; the live PR check runs in step 9 |
| 6 | Repo merge: `web/`, `ui/`, `play/`, `bridges/`; owner mailbox migration; `report`; web speaks socket-only | `dunkingsheep-web.service` serves `/dash/desk`; `grep -r "subprocess\|sqlite3" web/` returns only the session store |
| 7 | Context governance; retire the four rules; fix the two broken ground truths | `context check` clean; `context add --class ground-truth` refused for a worker |
| 8 | Cutover: stop and disable every `vp-*` unit (`vp-of-agentry`, `vp-telegram`, `vp-terminal-gateway`, `vp-herdr-webui`, `vp-herdr-webui-proxy`, `vp-ops-metrics`, `vp-ops-snapshot`, `vp-host-watch`, `vp-free-catalog`); rename `vp-terminal-webui` (ttyd) to `dunkingsheep-terminal` so the live terminal survives, with its session check folded into `dunkingsheep-web`; start `dunkingsheep-web`, `dunkingsheep-telegram`, the review and report timers; launch the desk pane; archive the repo | Telegram round-trip through the desk |
| 9 | Launch 1 reviewer + 1 worker lane on `task follow`; run the step 5 and step 8 checks that need live lanes (PR -> review task -> approval -> auto-merge; owner p50) | all step 5/8 checks green |
| 10 | Scale: 2 reviewer lanes, N worker lanes; point them at the poolrad-decomp backlog | dispatched_1h > 0; review queue age < 30 min; owner p50 < 20 s over a day |

Acceptance, measured not claimed: owner p50 < 20 s / p90 < 60 s over a full
day; manager-to-manager mail = 0 (there are no managers); `needs-owner` is the
only blocked-on-a-person state; 100% of merged PRs have a review task and a
non-author approval; 0 agent-authored `ground-truth` / `rule` entries after
the cutover.

## 11. What this does not solve

- Model quota churn still parks lanes. The deterministic fallback already filed
  matters more here, because no manager will notice a parked lane: the
  watchdog's `lane.idle_holding_task` rule has to.
- The desk will sometimes answer "I don't know; filed t600" where the CoS would
  have dug in. That is the trade, on purpose.
- A GitHub outage stalls merges. The daemon's review status is the only gate
  that can be relaxed, and only by the owner editing branch protection.
