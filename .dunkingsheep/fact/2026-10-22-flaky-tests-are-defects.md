---
class: fact
subject: flaky-tests-are-defects
type: decision
scope: project
authority: agent
stated: 2026-10-22
source: vp i21171, docs/DEPENDENCY_RULE.md
recheck_after: null
supersedes: null
superseded_by: null
---

A flaky test is a defect until proven otherwise: the first hypothesis is that the
product has a race, and the reproduction to hunt is the ORDERING, not the timing.
"It's the test" is a conclusion that needs its own evidence.

Proven 2026-10-22: a close-path test failed only on a loaded machine. A stopped
dunk's countdown thread wrote one more "Next: 60:00" outside the lock and without
checking whether the dunk was still running, so a closed dunk reported itself as
scheduled again. Fix: write under the lock and return once stopped (2.37.3).
Reproduction: start a dunk, close it one line later, sleep through a tick.
