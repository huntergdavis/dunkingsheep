# t475: the accepted harness misses a clipped/covered control (gap reproduced)

Bounded prerequisite for the `clipped-control` rule under t160. **No product rule is implemented here** — one positive, two negatives, and a proposed check with its false-positive risks.

Frozen inputs
* tool: `tools/desk_matrix.py` at main `92a57c6` (PR #93 merged), sha256 `6e2c073573acb60695ef97ecdf00e786e57f29b15f736c11164ca9ab6b95d429`
* fixtures: `fixtures/{visible,clipped,hitcloaked}/index.html` (each carries `.dash-root` so the `renders` rule can pass honestly)
* one shared heavy job (`poolrad-heavy.sh`, ticket granted), widths 1440 and 320, theme `default`

Measured outcome — **expected vs actual**

| variant | expectation | actual rule outcome |
|---|---|---|
| `visible` (positive) | pass | **passed**, no failures — the positive is not vacuous |
| `clipped` (control inside a 20px `overflow:hidden` box; 20 of its 32px are visible, so 62.5% visible / **37.5% clipped**) | should be flagged: part of the control is never painted | **passed** — no rule fires |
| `hitcloaked` (control fully in the box, sibling overlay covers it, `elementFromPoint` at its centre returns a DIV) | should be flagged: a user cannot click it | **passed** — no rule fires |

**GAP REPRODUCED.** Both negatives pass today, and each existing rule is honest about why: `no-descendant-overflow` compares `scrollWidth > clientWidth` (horizontal only, and a clipped child does not widen the parent), `target-size` measures `getBoundingClientRect()`, which reports the **unclipped** 120x32 box, and `renders`/`dialog-closed`/`no-writes` are unrelated. So this is not a rule lying — it is a case no rule covers.

Proposed check (geometry + hit-test, no new framework)

For every interactive control:
1. intersect its `getBoundingClientRect()` with each ancestor whose computed `overflow`/`overflow-y` is not `visible` (and with the viewport); require the visible area to be the whole box (allow a 1px rounding epsilon);
2. require `document.elementFromPoint(centreX, centreY)` to be the control or a descendant — this catches overlays that clip nothing;
3. report the control's selector, its box, the clipping ancestor and the visible fraction.

Limitations / false-positive risks, stated before the code
* controls legitimately clipped while **closed** (collapsed menus, hidden drawers) would fire; the check must run only on the states the matrix already treats as "open" (dialog-closed guard precedent), or the fixture must mark them;
* intentionally scrolled containers (`overflow:auto` with a scrolled control) read as clipped although the user can scroll to it — needs either "scroll into view first" or an exemption for scrollable ancestors;
* `elementFromPoint` returns the topmost element, so a control inside a transformed/pseudo-element stack can look covered when it is not; a second sample at the box corners reduces that;
* 1px sub-pixel layout differences at dpr 1.8 must not fire — hence the epsilon;
* a control covered only by a transparent-but-click-through layer (`pointer-events:none`) is fine and must not fire.
