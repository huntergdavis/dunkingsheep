# Ground-truth intent query set (t14)

The reviewer asked for a hand-labelled intent set: natural-language questions
with a **known relevant session**, so recall@5 is meaningful and is published
next to top-5 overlap. This set lives in `tools/memory_parity.py` as
`INTENT_GROUND_TRUTH` and runs by default (skip with `--no-intent`).

## Set

- **24 queries**, unique and balanced across harnesses:
  claude 6, codex 10, opencode 3, pi 5
- Each query is a paraphrase written from the session's content, **not a slice**
  of its text. Validation: every query is absent verbatim from every chunk of
  its expected session, and every expected session exists in the live DB
  (488 sources / 326 sessions / 262,299 chunks).
- `tests/test_memory_parity.py` enforces both properties, plus balanced harnesses
  and the side-by-side recall/overlap rendering.

## Results (memory-only, live DB copy)

- **memory recall@5 = 0.9167** (22/24)
- latency: p50 1928.88 ms, p95 3507.89 ms
- deja overlap: **skipped this run** — deja's CLI re-scans the ~17 GB corpus on
  every invocation (minutes per query), so a 24-query deja run is not viable.
  The harness marks overlap `skipped` rather than inventing a number. When deja
  answers, the same table fills both columns.
- A warm deja MCP `recall` was also tried: it took **134 s** and returned
  "No prior deja sessions matched", so deja is not a usable reference on
  this host right now. The harness therefore reports overlap as `skipped`
  rather than fabricating it; the last measured verbatim overlap stands.
- Last measured full-corpus **verbatim** overlap: **0.336** (top-1 0.30); the
  reviewer independently measured 0.31 / 0.2667. Those are ranker disagreement,
  not a parity claim.

## The two misses

- how do we spend the remaining claude weekly allowance on porting c functions
  - expected `01a14a62-2b5e-7123-b77e-dde9dad43205`; memory returned `01a1281b`, `01a1281b`, `019ff310`
- why was the host put on hold and deja bounded during high psi
  - expected `a3ad1536-abb2-4fb8-af2c-1362b8656678`; memory returned `4d9851e1`, `12a7f8ad`, `997d7fe9`

## Per-query ground truth

| # | query | harness | expected | memory hit |
| --- | --- | --- | --- | --- |
| 1 | how do i wire deja memory and the dunkingsheep agent nerve center together as mcp servers | pi | `01a14ae4-5c2` | True |
| 2 | what is the poolrad review and queue accounting lane supposed to do | pi | `01a14c56-d19` | True |
| 3 | the vp process crashed how do we restore the web interface and check the free models | pi | `01a1500b-3f4` | True |
| 4 | which routines did astra recover for the poolrad strict c port | pi | `01a14b16-22b` | True |
| 5 | why did deja break with an append eof and how was it repaired | pi | `01a14c46-4d9` | True |
| 6 | what did pr 860 deliver for the poolrad c chain and byte comparisons | codex | `01a14af7-41a` | True |
| 7 | can crackdown run on a switch lite under switchroot linux with box64 | codex | `01a14b7c-7c8` | True |
| 8 | how is the code word reference frame captured and matched with rune templates | codex | `01a14c77-7e8` | True |
| 9 | how do we spend the remaining claude weekly allowance on porting c functions | codex | `01a14a62-2b5` | False |
| 10 | who is staffing the poolrad panes and what are their weekly limits | codex | `01a12b3e-8ce` | True |
| 11 | how do i use dunking sheep to federate work to the poolrad research tabs | codex | `01a1281b-85e` | True |
| 12 | cap a deja memory lookup at 90 seconds when the index is unresponsive | claude | `281c8700-5ac` | True |
| 13 | how fast does the jolt vehicle recover from a high speed side slide | claude | `f341143d-cff` | True |
| 14 | fake herdr workspace creation and the 88 test suite | claude | `fb4de8d7-211` | True |
| 15 | how do sessions name and message their peer sessions | claude | `9742b6e1-d50` | True |
| 16 | why was the host put on hold and deja bounded during high psi | claude | `a3ad1536-abb` | False |
| 17 | the opus 5.5 task for 0x1951 capture parity | claude | `12a7f8ad-6b6` | True |
| 18 | take over as vp from the handoff and keep the web apps running | opencode | `ses_eb525b6e` | True |
| 19 | run the crackdown pc recomp on a nintendo switch running linux | opencode | `ses_eb5752dd` | True |
| 20 | enable all permissions and lsps in opencode and add deja memory | opencode | `ses_eb584d76` | True |
| 21 | which snap interfaces did bottom need for hardware monitoring | codex | `019e9dc8-a55` | True |
| 22 | how does dunkingbird capture the screen on kde wayland | codex | `019e9ddb-827` | True |
| 23 | does moving a function change the binary when section sorting is enabled | codex | `019e9dc7-5b1` | True |
| 24 | how was the dunkingbird key delay and send timeout tuned | codex | `019ea2fd-ece` | True |

## Reproduce

```sh
# full comparison (memory + deja) when deja is answering:
python3 tools/memory_parity.py --queries 30 \
  --markdown docs/MEMORY_PARITY_REPORT.md --detail docs/memory_parity_detail.json

# ground-truth recall only (no deja; fast, stable):
python3 tools/memory_parity.py --queries 0 --memory-only \
  --markdown /tmp/memory-intent.md --detail /tmp/memory-intent.json
```
