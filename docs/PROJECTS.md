# project_spawn: the front desk starts a project (t1171)

Owner ask (2026-10-31): "I want to tell the front desk to spin up a new project and that
project gets spun up with new agents. I don't want to write any commands or spin up agents
myself."

This is the Rust-only project contract (Python is frozen). The command and durable budgeted
project engine are implemented; production policy attachment and live activation remain gated.

## Why a daemon verb

`start_agent`, `create_workspace`, `create_tab`, `split_pane`, `close` and `shutdown` are
denied to the desk and web roles by design (2026-10-18 incident). Giving the desk raw
`start_agent` is unsafe. Instead the desk gets ONE high-level verb, executed by the daemon
with its own authority; the daemon decides how. The desk role gains exactly this verb.

    project_spawn(name, brief, template = "standard", soft_cap = None)

`project_stop` stays owner-only.

## Owner decisions (2026-10-31, via %126, i35520)

* A project is a **workspace + folder + agents**, with a local git repo (GitHub later) and
  a `dunkingsheep` directory inside it for local truths. No GitHub repo, no GitHub Apps.
* Standard team: **1 research agent** writes the plan, then **workers** pull tasks from the
  queue and **reviewers** review.
* Cost: **no cap by default, but the daemon ASKS at creation** and supports a **soft cap**:
  when reached, the project's agents stop until the owner lets it past.
* Models: research = sonnet or opus; workers default to the newest deepseek flash, then
  codex when out of budget, then claude (reverse order for research agents). Rust lanes
  today: Claude Sonnet low.
* Approval is one word from the owner.

## Current source steps (durable and idempotent by `name`)

1. Validate the slug, configured project policy, provider budget, and fresh source reading before
   opening the durable project store. Return an existing live project without starting it again.
2. Scaffold the project directory without overwriting foreign content.
3. Create one workspace and start one Codex Luna low writer and one Codex Luna medium reviewer
   with `--no-daemon`; persist each pane identity before recording its runtime role.
4. Send the project report through the canonical owner inbox and record success only after the
   inbox returns a committed message id.
5. Persist every state transition and step receipt. An unresolved external action remains
   uncertain across restart and requires inspection before retry.

## Guards

* Max agents per project and max live projects (constants, named in the refusal text).
* REFUSE when the fresh five-hour provider usage is >= 70% (the existing Rust-gate headroom guard).
* Soft cap: counted from usage readings; at the cap the project is paused, not killed.
* Every step is an event, so partial state is visible. Uncertain external actions refuse replay.
* Tests use a fake terminal; no real agents.

## Current runtime boundary (t3158)

* The production dispatcher registers `project_spawn` once. The desk request is limited to
  `name`, `brief`, `template`, and `soft_cap`; provider, budget, paths, approval, and argv come
  from an explicit typed policy attached to the canonical Flock.
* Runtime policy is selected by exact project name. Each entry carries an explicit provider,
  budget JSON (including required `credit_ceiling` and explicit provider keys), and approval
  state. Missing, invalid, unknown, or unapproved policy refuses before opening SQLite or
  writing a project directory.
* Provider usage is read for each request. The source subscription is checked first, followed by
  credit fallback only when policy explicitly enables it and the reading is attributed to that
  exact project and provider. Current Rust sources do not report project-attributed credit spend;
  account balances and deltas are not project usage. Unknown, stale, future, malformed, and
  unattributed evidence refuses.
* Project agent launches use the bounded Codex Luna template (`--no-daemon`, writer low,
  reviewer medium). The existing fleet role registry calls the writer lane `worker`; its
  `project-<name>-writer` label distinguishes it from the registered `reviewer`. Pane identity is
  saved before role registration; role and owner-report steps remain uncertain after an ambiguous
  failure, so retries cannot blindly start or report again.
* This source slice does not install live project settings, choose account amounts, establish
  owner-approval provenance, or activate a production startup attachment. Those decisions remain
  owner-held under t1692/t1605. Tests use temporary stores and controlled local fakes; no live
  provider/account data or agent starts are part of this source work.

## Not decided / not verified

* The exact per-project agent and project maxima.
* How the owner approval transport proves provenance in a live runtime.
* Actual operator-approved project/provider limits and real project-credit attribution.
* Production startup attachment and live role/model behavior.
