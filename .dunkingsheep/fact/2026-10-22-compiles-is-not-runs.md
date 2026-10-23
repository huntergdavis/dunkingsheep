---
class: fact
subject: compiles-is-not-runs
type: lesson
scope: project
authority: agent
stated: 2026-10-22
source: vp i21531 (#1437), docs/DEPENDENCY_RULE.md
recheck_after: null
supersedes: null
superseded_by: 2026-10-22-compiles-is-not-runs-2
---

'It compiles' is not 'it runs'. A gate that proves a module BINDS proves nothing about EXECUTION: an undefined name or a bad hook signature only fails when the code path runs. So a registration means 'runs': CI executes each registered checker/command on a smoke case, and the doc says which of the two claims it makes.

Proven 2026-10-22 in poolrad #1437: --compile-only passed while an undefined Unicorn register constant reached main, because the hook only executes inside an audit.
Proven the same day here: our CLI parity test greps help text, which proves a name is spelled, not that a handler works. The smoke test executes all 86 commands and found two that indexed args['target'] without declaring it required.

Corollary: a REPAIR that changes no claim carries a dated note beside the evidence, not a rewritten document - a rewritten doc reads as if the claim always held.
