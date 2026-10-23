---
class: fact
subject: compiles-is-not-runs
type: lesson
scope: project
authority: agent
stated: 2026-10-22
source: vp i21531 (#1437), docs/DEPENDENCY_RULE.md
recheck_after: null
supersedes: 2026-10-22-compiles-is-not-runs-2
superseded_by: null
---

A CHECK THAT PROVES SPELLING IS NOT A CHECK THAT PROVES FUNCTION, and A RULE TESTED ONLY IN ITS HAPPY PATH MAY BE IMPOSSIBLE IN ITS ONLY CASE.

(1) Grepping a help text, importing a module, compiling a file, listing a registry - each proves a NAME exists; none proves a handler RUNS. The test must execute the thing and assert a RESULT; for a parameterless case that means a real answer, not a polite nothing (reject None, {}, [], '').

(2) When a mechanism exists for ONE purpose - supersede, retire, rollback, migration, restore - the test must exercise the case it exists FOR, not a convenient neighbour.

Evidence:
- poolrad #1437: --compile-only bound a module whose hook read an undefined Unicorn register; the NameError only fired inside a real audit.
- dunkingsheep: the CLI parity test grepped help text; executing all 86 commands found two lies at once - clear_box raised KeyError 'target' on exactly the blocked panes it existed to unblock, delivery printed a clear result as if it were a message.
- supersede() derived the same slug as the entry it replaced, so same-day supersession collided and refused: the ONLY mechanism for changing an entry was impossible in its only case. A rule documented, happy-path tested and impossible in practice is worse than no rule - it reads like a guarantee.
- memory_backfill exists to enqueue the corpus resumably; its nearest test exercised enqueue, not the command. The case that matters is a second run over a partly-done corpus, or it re-ingests 14 GB.

Corollary: a REPAIR that changes no claim carries a dated note beside the evidence, not a rewritten document.
