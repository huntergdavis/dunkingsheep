# Poolrad evidence queue and diff dashboard (local proof of concept)

This standalone dashboard reads the existing Poolrad ledger and classifier. It
does not modify the project, assign workers, or infer per-function strict C
credit. No network or third-party Python packages are needed.

```sh
python3 tools/dashboard/dashboard.py render
python3 tools/dashboard/dashboard.py verify
python3 -m http.server 8765 --bind 127.0.0.1 --directory tools/dashboard
```

Run these commands from the Poolrad repository root. Open
`http://127.0.0.1:8765/dashboard.html`, or open the generated
`tools/dashboard/dashboard.html` directly. The server binds only to localhost
and serves only the dashboard directory. `render` copies its two cited public
reports into ignored `state/reports/` so their links work without serving
private `resources/`. The `verify` command runs an
assertion-enabled strict CMake build and all CTests in `/tmp/poolrad-dashboard-build`
and records commit, timestamp, exit codes, test totals, and logs in `state/`.
The rendered status becomes **stale** if the repository commit changes.
The page's "dashboard checkout" hash identifies the local checkout used to
render it. It is not a claim that this checkout is the published Poolrad main
branch. Strict C credit comes from the PM-reviewed accounting checkpoint below.

The function cards read reviewed numerators from
`docs/re/function_accounting_checkpoint.json`. The working function total
comes from `docs/re/function_ceiling_census.json`; candidate prologues and
resident callable rows cannot be added to obtain it. A census
may change to `status: reviewed` only after PM review, with an integer
`count`, `bound_kind` (`exact` or `upper_bound`),
`scope: original-game-functions-overlay-and-resident`, `source_digests`
matching the accounting checkpoint's `inputs`, `reviewed_by: PM`, and an
`evidence_doc` under `docs/re/`. The dashboard then uses the reviewed count
as its current working denominator and reports strict full-C and
verified-identity percentages separately. The `bound_kind` remains in the
data and evidence: the current census is a static upper bound, so new
evidence may revise the working total and percentages. The evidence document
states inclusion rules and unresolved boundaries. The cards remain count-only
while the census is pending.

The owner page leads with the PM-reviewed strict C-body count, the audited
candidate ceiling, and their qualified gap. It does not claim a whole-game
completion percentage or a verified backlog. The next gate is an independently
reviewed original-function body on an exact source head; authored local drafts
remain separate. The full candidate list remains in ignored
`state/snapshot.json` for research, but is omitted from the owner page.
PM assignments use separate workstream and source-gate queues; the classifier
list is not a task queue or the source of strict-C credit.

To compare two private, same-state **indexed pixel byte streams**:

```sh
python3 tools/dashboard/dashboard.py compare --dos /private/native.idx --c /private/portable.idx \
  --width 320 --height 200 --label 'same-state captured frame'
```

Both files must be exactly width × height bytes. Only SHA-256 digests and
aggregate match counts are saved in `state/local_diff.json`; the bytes are
not copied. The caller must independently establish capture state, palette,
and provenance. Equal bytes alone are not a whole-game or strict full-C claim.

The two published comparison cards come from explicit scoped reports in
`docs/re/`; the dashboard does not rerun their private DOS traces. They cover
different boundaries and must not be summed. Strict full-C credit is not
derived from these classifier rows; consult the
[generated accounting checkpoint](../../docs/re/function_accounting_checkpoint.json)
and [battle card](../../docs/BATTLE_CARD.md) for that measure. Queue priority
is an exploratory sort, not a PM task list.

For a local follow-up marker, use `python3 tools/dashboard/dashboard.py mark --id
0x17:0x0000 --status needs-evidence --note 'inspect caller'`. Marks are saved
only in `state/queue_marks.json`, show as stale after the source commit changes,
and do not represent assignments. The status choices are `watch`,
`needs-evidence`, `ready-for-review`, `blocked`, and `archived`.

This is a proof of concept. A production integration needs a reviewed,
machine-readable function-to-C-credit ledger, a committed diff manifest with
capture identity and scope, and a source commit boundary for every result.
