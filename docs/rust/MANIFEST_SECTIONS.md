# Which MANIFEST.md sections a generator can derive, and from what (T00, t779)

`tools/gen_manifest.py` (TASKS.md T00) must regenerate `docs/rust/MANIFEST.md`
**byte-identically**. That file is 2803 lines over **68 `##` sections**. This page
says, section by section, where each one's content comes from, so the generator can
be written in one pass instead of guessed at.

Measured on `flat-fleet/rust-spec` @ `5f0ef81`, 2026-10-30.

## The three classes

| class | meaning | how the generator gets it |
| --- | --- | --- |
| **D** derived | a function of the tree; regenerate it and the bytes follow | read the named source, format exactly |
| **C** curated | design decisions and prose no derivation produces | must be **embedded as data in the generator**, or the manifest must change shape (below) |
| **X** external | comes from something outside the tree (a live database, a recorded session) | embedded, or read from the named artifact with a stated fallback |

## Section map

| lines | section | class | source of truth |
| --- | --- | --- | --- |
| 5-41 | Module map | **C** | python → rust file, crate, phase, notes: a design decision per row |
| 42-46 | CLI top-level commands | **D** | the argparse subparsers in `dunkingsheep` |
| 47-51 | Files on disk | **C**/D | mostly `CONFIG_DIR / "…"` sites, but the list is hand-ordered and includes names no single grep yields (`system-token`, `web-token`, `github-app-*.pem`) |
| 52-56 | Systemd units | **D** | `packaging/systemd/*.service`/`*.timer` (the FF-9 sentence is curated) |
| 57-179 | Commands with role permissions | **D** | `dunk_api.COMMANDS` (name, params, required, mcp) + `dunk_roles.ALLOW` / `WEB_DENIED` / `READ_ONLY` |
| 180-195 | MCP tool groups | **D** | `dunk_mcp.MCP_GROUPS` |
| 196-360 | Error strings tests assert on | **D**/**C** | string literals in `tests/` that are asserted on — derivable, but the *selection* rule ("tests assert on") is a judgement, so treat the selection as curated and the text as derived |
| 361-633 | Schemas (7 databases) | **X** | the live databases under `~/.config/dunkingsheep`, printed verbatim; the SCHEMA constants in `dunk_tasks`/`dunk_inbox`/`dunk_metrics`/… are the tree-side equivalent and differ where a migration ran |
| 634-653 | Tests inventory | **D** | `tests/*.py` count and names |
| 654-2803 | Inventory, one section per module (65 modules) | **D** | AST of each module: line count in the heading, first line of the module docstring truncated to ~140 chars, then each class/function/constant with its docstring's first sentence; `dunkingsheep` and `web/app.py` have their own shapes |
| 2755-2803 | Web routes (T62) | **C** | the route table as of PR #120 — curated, because it names what T62 must reproduce |

## The decision this page exists to force

Byte-identical reproduction is only possible if the **C** rows are embedded in the
generator. That is legitimate (a curated table is data), but it puts ~1,000 lines of
prose and design decisions inside `tools/gen_manifest.py`, where a reviewer will not
look for them.

The maintainable alternative: author the **C** sections by hand in `MANIFEST.md`
outside fenced blocks, and have the generator own only the fenced **D**/**X**
sections, refusing to write if a fence is unbalanced. That changes T00's acceptance
from "byte-identical" to "the generated sections are byte-identical and the curated
ones are never touched" — which is the same guarantee where it matters and a weaker
one where it does not.

**Nothing in this file is implemented yet.** It exists so the next lane does not have
to re-derive the split, and so the owner can answer the question in one line.
