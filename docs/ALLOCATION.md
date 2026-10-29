# Deterministic worker and review allocation

The allocator selects registered workers without a model or a coordinator turn.
It runs in the daemon's existing task watchdog loop, at most once per 30 seconds;
registration, offers and review submissions wake that loop. An empty registry is
disabled by default. It does not launch agents, invent task scope, unblock tasks,
or override human drafts, native turn state, process identity or the dispatch kill
switch.

An operator registers each worker once with explicit pools, roles and skills:

```
dunkingsheep allocation register --pane-id %91 --pools poolrad --roles worker --skills c
dunkingsheep allocation register --pane-id %19 --pools poolrad --roles reviewer --skills c
```

Registration itself is dispatch opt-in; it does not require editing a second
allowlist. Existing explicit dispatch entries continue to work. `--disable` on a
registration prevents new pool allocations; it does not cancel existing work or
revoke a separate explicit dispatch entry.

Release an existing owned backlog item into a pool with the guarded operator CLI:

```
dunkingsheep allocation offer --task-id t123 --pool poolrad --requires c
```

The task must have an owner and a concrete body. Ownership remains accountable
while waiting, then transfers atomically to the selected worker. Existing tasks
are **not** guessed into pools from their prose. This one-time enrollment defines
the scope that may move between workers; routine selection thereafter is SQL,
not another request to a coordinator. Private tasks, active leases, missing
dependencies, blocked status and explicit block reasons never dispatch. Changing
the assignees after an offer invalidates that offer rather than stealing it back.

An author submits a frozen artifact for independent review:

```
dunkingsheep allocation review --task-id t123 --pool poolrad --requires c \
  --revision FULL_COMMIT_HASH --body 'Artifact location, acceptance commands and limits'
dunkingsheep task complete t123
```

The review command authenticates the native source owner. It creates one durable
review task and queues it for a registered reviewer automatically, excluding the
source's owners, creator and completer. It does not self-accept the source or mark
the source complete. Repeating the same revision returns the same review. A
corrected revision reopens that same review after the previous review finishes;
an active review cannot have its artifact silently replaced. Completing the
source advances the author's already assigned next task through the existing
completion path. If no assigned work remains, the next idle watchdog pass may
allocate pool work to that author.

Tasks sort by explicit priority, oldest creation time, then task ID. New tasks
already default to LOW. Matching workers sort by oldest last allocation, then
pane ID. Capacity is one. Existing assigned work takes precedence over new pool
work; offered work is excluded from the old pull/follower path so an author
cannot accidentally claim their own pending review.

Each claim, ownership transfer, allocation event and existing `nj-*` delivery
intent commits in **one SQLite transaction**. Failed inbox materialization leaves
the intent for normal next-job recovery. Concurrent ticks and restart cannot
claim the same offer twice. Registration and review revision records persist in
the task database. A fresh authenticated ended turn, matching live process, empty
pending delivery/unread queues and a ready guarded terminal are required before
allocation. Unknown activity is not idle; after a daemon restart, an idle worker
without fresh native turn evidence remains held until its producer supplies it.

```
dunkingsheep allocation status --json
```

Status exposes registry contracts, offered/assigned state and the most recent
pass's worker gates. `allocated` is a claim, **not** delivery or completion.
Use the existing inbox receipt, bound native turn and task completion events to
verify the real chain. Review acceptance, source merge, runtime rollout and
product evidence remain distinct.
