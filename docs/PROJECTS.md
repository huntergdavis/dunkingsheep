# project_spawn: the front desk starts a project (t1171)

Owner ask (2026-10-31): "I want to tell the front desk to spin up a new project and that
project gets spun up with new agents. I don't want to write any commands or spin up agents
myself."

This is a CONTRACT for the Rust daemon (python is frozen). Nothing here is implemented yet.

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

## Steps (each logged as a task event; the verb is idempotent by `name`)

1. Validate `name` (slug) and refuse an existing live project of that name (return it).
2. Ask the owner at creation: confirm the template and the soft cap; wait for one word.
3. Create `~/workspace/<name>`, `git init`, write `BRIEF.md` and a `dunkingsheep/` dir.
4. Create the workspace and tabs through the terminal backend.
5. Start the research agent; register roles. Workers and reviewers start when the research
   agent files the plan as tasks.
6. Pick each agent's model by real usage headroom (usage.rs readings; an `unknown`
   reading is never treated as headroom).
7. Post a report to the owner mailbox with pane ids.

## Guards

* Max agents per project and max live projects (constants, named in the refusal text).
* REFUSE when five-hour Claude usage >= 70% (the Rust-gate headroom number).
* Soft cap: counted from usage readings; at the cap the project is paused, not killed.
* Every step is an event, so a half-spawned project is visible and resumable.
* Tests use a fake terminal; no real agents.

## Not decided / not verified

* The exact per-project agent and project maxima.
* How "one word" approval is delivered (owner inbox vs. pane).
* Behaviour with the real tmux backend; nothing here has been run.
