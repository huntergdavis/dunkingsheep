---
class: fact
subject: compiles-is-not-runs
type: lesson
scope: project
authority: agent
stated: 2026-10-22
source: vp i21531 (#1437), docs/DEPENDENCY_RULE.md
recheck_after: null
supersedes: 2026-10-22-compiles-is-not-runs
superseded_by: 2026-10-22-compiles-is-not-runs-3
---

A CHECK THAT PROVES SPELLING IS NOT A CHECK THAT PROVES FUNCTION. Grepping a help text, importing a module, compiling a file, listing a registry - each proves a NAME exists; none proves a handler RUNS. The test must execute the thing and assert a RESULT, and for a parameterless case that means a real answer, not a polite nothing: reject None, {}, [] and "" explicitly, because a handler that returns an empty dict and calls it success passes every weaker assertion.

Evidence, both sides:
- poolrad #1437: --compile-only BOUND a module whose hook read an undefined Unicorn register; the NameError only fired inside a real audit, so an undefined name reached main.
- dunkingsheep, same night: 'every command is reachable from the CLI' grepped help text. Executing all 86 commands found two lies on the first run - clear_box raised KeyError 'target' on exactly the blocked panes it existed to unblock, and delivery printed a clear result as if it were a message. Both were commands the whole night depended on. A gate that finds two lies in one run should have existed three hours earlier.

Corollary: a REPAIR that changes no claim carries a dated note beside the evidence, not a rewritten document - a rewritten doc reads as if the claim had always held.
