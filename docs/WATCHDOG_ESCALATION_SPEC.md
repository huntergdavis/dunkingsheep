# Exception watchdog: escalation policy spec

**Status:** reviewer spec (par-a, 2026-10-20, VP i19806), written before reading the implementation. It is the review criterion: if the code disagrees, change the code or argue the spec in writing.

**Channels:**
- **OWNER** is anything that reaches the owner's phone (the Telegram bridge, or a VP brief forwarded by it).
- **VP** is the VP's dunkingsheep inbox.
- The owner's trust in OWNER is the scarce resource. A false page costs more than a missed one, because it teaches the owner to ignore the channel.
- **OWNER is one channel, however it is delivered.** A Telegram page and a VP brief forwarded to Telegram count the same. A page counts as delivered only when the forwarding path confirms it, for example when the bridge's `last_brief_id` advances. A failure anywhere on the forwarding path is a **`watchdog.self_error`**: it goes to VP with the raw error, and the page stays **undelivered** in state. It must not be recorded as sent, and its dedupe or cooldown entry must not start. A page that silently fails is worse than one never generated, because the system believes it informed the owner.

## 1. Conditions and who they may reach

**Only three classes may page the owner: security, outage and money.** Everything else is VP-only, however long it persists.

| Condition | Class | Channel |
| --- | --- | --- |
| A secret or credential appears in a tracked file, an outgoing commit or a PR body (token pattern match on content the watchdog already reads) | security | **OWNER** |
| A protected secret file changes mode, or becomes readable by a group or others | security | **OWNER** |
| A service the owner depends on is down: the VP web desk, the Telegram bridge, or the host itself (disk full, or out of memory) | outage | **OWNER**, with the exception below |
| Paid-model spend or quota is projected to cross the owner-set cap before reset | money | **OWNER** |
| Host load or CPU pressure is high, `/tmp` is at 70–90%, or inodes are high (the existing `host_watch` levels) | ops | VP |
| An agent lane is quiet, idle, stuck or looping, or a dunk failed or went unconsumed | ops | VP |
| A task lease expired, or a task was requeued | ops | VP |
| CI is red on main, the accounting pin has drifted, or a gate script fails | project | VP |
| A heavy-job semaphore slot is stale (the lock holder is dead) | ops | VP |
| **Anything the watchdog itself cannot evaluate** (see section 3) | self | VP, never OWNER |

**Exception:** if the Telegram bridge itself is the failing service, its outage page goes to VP with a desk banner. It must not be sent through the bridge (see section 5).

### Current coverage (as of PR #1 at `d3462d6`, 2026-10-20)

- **No rule implements the security or money classes.** The 13 rules plus `watchdog.self_error` and `watchdog.deadman` cover work, delivery, scheduling, audit and outage only.
- So the only rules that could reach OWNER today are the outage-labelled ones: `host.tmp`, `health.failed` and, until it is relabelled, `host.load`.
- An empty security or money row in this table means **not monitored**. It does not mean nothing happened.
- Update this paragraph whenever a rule is added or relabelled.

**Gates (VP i19818):**
- **Tier 1**, before any install:
  - delivered means confirmed;
  - nonsensical values (a zero threshold or zero cooldown) are refused;
  - the dry run prints the threshold and the measured value, and supports `--replay`;
  - the dead-man has a non-inbox fallback.
- **Tier 2**, before owner paging is ever enabled:
  - `host.tmp` and `host.load` move to an `ops` category outside owner eligibility;
  - the hold condition from section 2;
  - state-loss suppression;
  - an independent owner rate cap.
- Owner paging stays disabled in the shipped config until Tier 2 has landed and been reviewed.

## 2. Firing rules for owner pages

| Condition | Threshold | Must hold for | Rate limit | Dedupe key |
| --- | --- | --- | --- | --- |
| Secret exposure | one confirmed match, filtered by an allowlist of known test fixtures | a single run is enough, but it must re-verify the match on a second read before paging | 1 page per key, then silent until the owner acknowledges or the content changes | `(kind, path, sha256 of the matched line)` |
| Secret-file permission change | the mode differs from the recorded baseline | 2 consecutive runs | 1 per file per 24 h | `(path, mode)` |
| Service outage | health check fails | at least 3 consecutive runs, spaced 60 s or more apart (about 3 min), and still failing on the page attempt | 1 page per outage, then 1 reminder after 60 min, then silence until recovery | `(service, outage-start-time)` |
| Host disk/memory outage | `/tmp` or root ≥ 98%, or the OOM killer fired on a listed service | 2 consecutive runs | 1 per 30 min | `(resource, level)` |
| Spend projection | projected ≥ 100% of the owner cap before reset, using a linear fit over ≥ 2 h of samples | 2 consecutive runs | 1 per billing period per provider, plus 1 when actual spend reaches 100% | `(provider, period)` |

- **Recovery** messages go to VP only. The owner receives at most one "resolved" line, and only for a page that actually reached them.
- **Global cap:** at most 3 owner pages per rolling hour across all rules. Anything beyond that is collapsed into a single VP message.

## 3. Fail-safe: fail toward not paging

Each of these **must not page the owner**:
- a parse error;
- missing config;
- an unknown key;
- a non-numeric, zero or negative threshold, or a rate limit of 0;
- an unreadable source;
- a missing state file.

**At start-up**, a bad config means the process exits non-zero with a named error (`CONFIG: unknown key "pagethreshold" in rules[3]`). It sends one VP message carrying that raw error and **sends nothing to the owner**.

**At run time**, a single rule that cannot evaluate is skipped and reported to VP with the raw error. The other rules continue.

**State loss** (the dedupe store is missing or corrupt) suppresses owner pages for that run. Otherwise a lost dedupe key would re-page an old incident. It is reported to VP.

**What "not paging" means:**
- no Telegram send;
- no brief marked for forwarding;
- no write to the owner channel's queue.

Each of these must be checkable in a test. Required tests:
- truncated JSON;
- an unknown key;
- a threshold of 0 or below;
- missing state;
- the owner channel's queue unchanged after each of the above.

## 4. Dry-run contract

`--dry-run` evaluates every rule, sends nothing and writes no state. For each would-be page it prints one line:

`WOULD-PAGE OWNER rule=<id> class=<security|outage|money> threshold=<value+unit> measured=<value+unit> window=<n runs/period> dedupe=<key> suppressed=<no|rate|dedupe|global-cap> evidence=<path:line | url | metric@time>`

It prints the same line with `WOULD-NOTIFY VP` for VP-only rules, and `SKIPPED rule=<id> error=<raw>` for rules it could not evaluate.

It also accepts `--replay <history dir>`, which re-evaluates past samples and prints how many owner pages each rule *would* have sent. That lets thresholds be tuned against real history before the first live page.

## 5. Never

- **No UI:** the output is lines, a state file and inbox messages.
- **No model calls.** The decisions are deterministic, so the same inputs always give the same pages.
- **No writes to the stores it reads.** That covers repos, the dunk and task databases, and metrics. It writes only its own state and outbox.
- **No escalating through the failing channel.** A Telegram-bridge outage never pages via Telegram, and a broken inbox is reported via the desk banner and the log.
- **No auto-remediation:** it does not kill, restart or delete.
- **No paging the owner about the watchdog itself.**
- **No new owner-paging rule without a VP-approved edit to section 1.**
