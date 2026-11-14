# One-shot PR waits (t4694)

After `gh pr create`, run `dunkingsheep pr wait huntergdavis/dunkingsheep#123 --head FULL_SHA` and end the turn. The existing review-sync service checks the PR on behalf of every interested lane and sends one ordinary inbox notice when the expected head moves, a canonical exact-head review verdict is recorded or a terminal GitHub review status appears, required CI finishes (including failing check names), or the PR merges/closes. No GitHub write is introduced by the wait service. It does not grant merge or review authority.

Use `dunkingsheep pr wait --list` to inspect your registrations, last poll time, next due time, expiry and any pending notice. Use `dunkingsheep pr wait --cancel ID` to cancel your own registration. API/MCP: `pr_wait` accepts `target`, optional `head`, `list`, or `cancel`. Registrations are caller scoped; another lane cannot list or cancel them. Re-registering an active target with the same head is idempotent; a different explicit head requires cancellation first. Without `--head`, the first successful PR detail read pins the head; supply the full SHA to detect changes before that first read.

The service uses repositories from the existing review registry, its reviewer token provider, and the existing two-minute review timer. Registration/list/cancel makes no GitHub requests. There is no separate timer and no polling process per waiter. A sweep captures successful GET responses from review-sync and reuses them **only** for waits; review decisions always make their original fresh reads. The wait pass performs at most four shared GETs per PR: detail, combined commit statuses, required status-check policy, and check runs. Detail/status reads already made by review-sync reduce this count. Each endpoint supports persisted ETags when GitHub supplies them; later wait reads send `If-None-Match` and reuse the body on 304. No credentials are persisted with this cache.

At most ten distinct PRs are watched globally, with at most twenty registrations per lane. The upper bound is 40 extra GET attempts per sweep, or 1,200/hour at the minimum 120-second cadence, before read reuse and conditional responses. Fixture measurements: **two waiters together** use four GETs for completed CI, two for a GitHub review-status verdict (one for a canonical local verdict), and one for head movement/merge/closure. This measures the wait pass, excluding the existing review sweep and token minting. GET attempts and ending-event counts are logged per sweep. Completed waits make no further check-run reads. A later explicit registration starts a new interest.

SQLite stores registrations, pinned heads, next due times, retry counts, HTTP cache and pending ending events in the existing inbox database. Poll claims survive restart and keep manual/concurrent sweeps from creating another per-PR stream. The review handler serializes sweeps. Four failed observations, separated by at least 240/480/960 seconds after the initial attempt, produce one “could not watch” event. A successful observation resets this backoff. Waits expire after 24 hours; expiry is evaluated before polling and delivered on the next review sweep. If the timer is stopped, nothing can send a notice until it resumes; `--list` exposes the stale last/next timestamps.

Ending events are persisted before delivery through the existing `Flock.inbox_send` notification path, and removed after a successful send. Restart before send retains the pending event; restart after the delivery mark sends nothing again. **Delivery is at least once across a crash between inbox_send and the delivery mark:** that narrow window can repeat a notice. No new typing or notice delivery path is added. Undeliverable recipients retain their pending notice for a subsequent sweep.

CI completion uses the base branch's required-status-check endpoint. Missing checks remain pending. Check-run requirements bound to an App ID accept only that App's runs; superseded attempts are ignored. A completed canonical review task for the exact repo/PR/head ends the wait even when a multi-review repository still has a pending aggregate status. This reuses the daemon task actor without another API read. The `dunkingsheep/review` context also has its own verdict event and does not hold CI completion open. Unavailable required-check policy, malformed responses, or more than 100 check runs/statuses fail the observation and eventually produce “could not watch”; the service does not infer success. Ruleset-only requirements are not supported by this first slice. A branch with no configured CI checks waits for the other ending events or expiry. GitHub App installations need read access to PRs, commit statuses, checks and branch-protection metadata.

Tests use in-process fixture transports only. The worktree's local commands use `ds-cargo` with an isolated target directory. This change ships in a later independently reviewed daemon release; it does not alter production services or release files. Python generator edits describe the Rust-only CLI/API extension and refresh test fixtures; the retired Python runtime is unchanged, so there is no Python behavior requiring a Rust parity task.


When the required-check policy read returns 403 or 404, a wait can report observed
CI outcomes without claiming branch protection is satisfied. A nonempty, complete
set in which every observed run/status succeeded is labelled exactly
`required checks unknown / all observed checks green`. Failures name the observed
checks, while pending or empty results keep waiting. Missing, malformed, partial
or truncated data never produces a green notice. Other HTTP failures retain the
bounded error/retry behavior. A readable required policy keeps the existing
required-check behavior.

This is a notification fallback only. It does not alter review-sync's merge gate
or substitute observed green results for a verified required-check policy.
