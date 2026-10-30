# Theme authoring contract

Register a theme by adding only two files: <id>.css and <id>.json in this directory. The eager CSS/JSON globs discover them; do not edit main.jsx, a shared theme array, or another theme's files. IDs use lowercase letters, digits and hyphens and must match both filenames.

A manifest is {"id":"nostromo","name":"Nostromo","order":20}. Default is always first. Other entries sort by numeric order, then name, then ID as a stable tie-break. The registry rejects missing partners, mismatched IDs, empty names, nonnumeric order and a missing Default theme.

New theme CSS contains only token blocks, scoped to :root[data-theme="<id>"]. Inherit the Default tokens and override the roles needed by the theme. Do not add component selectors, layout rules, media queries, network font imports, animations or shared-file edits. This token-only rule applies to NEW themes: crt.css retains its pre-existing component, overlay and media rules unchanged as a fidelity exception.

Set bg, surface, surface-raised, ink, muted, accent, accent-contrast, borders and semantic colors; remap named component tokens when necessary. The contrast gate accepts opaque hexadecimal backgrounds and hexadecimal foregrounds with optional alpha; aliases may refer to other tokens. Unsupported token selectors or colors fail visibly.

The existing --overlay token carries CRT scanline strength through its rgba alpha (0.12), with a fixed 1px dark/3px clear pattern. Keep that legacy value unchanged. A new draft can declare --scanline-strength and consume it inside its own --overlay value, but that token alone does not enable an effect. Generic overlay, display-font and ChatScope body-font consumers are NOT implemented by this split: the existing effect selectors are CRT-only. Those consumers and CRT's dedicated strength-token migration need a separate implementation and verification; do not claim a new theme's overlay/font behavior from token presence alone.

Use system font stacks, or bundle an openly licensed font with its license text beside its font files in a theme-owned asset directory. Font assets/licenses may add files, but registration still needs only the CSS/JSON pair and no shared edit. Until generic font consumers exist, verify each actual font role in the browser and report unsupported ones.

Run from the repository root:

    node web/tools/check-theme-scope.cjs
    .venv/bin/python tools/theme_contrast.py --json
    .venv/bin/python -m unittest tests.test_theme_contrast tests.test_theme_capture

Attach the gate report: ink/bg >=7, muted/bg >=4.5, accent/bg >=3, accent-contrast/accent >=4.5. These token checks do not certify every rendered text node, alpha overlay, focus outline or component.

Capture the actual theme with all dialogs closed at1067 and390, plus the owner1881 blocking widths1422/1067/960/800/700/390. Keep24x24 targets, unobscured keyboard focus,200% text, contrast4.5/3 and no clipping/overflow at blocking widths.320 is advisory: measure and name failures without treating them as blockers. Use the capture guard; a screenshot through an open Appearance dialog is invalid evidence.

Record CSS viewport size and raw devicePixelRatio. Viewport plus deviceScaleFactor1.8 is EMULATION, not proof of real browser zoom, operating-system scaling or physical-screen dimensions. Scanlines can rasterize differently across DPR values; report the configuration and inspect the actual result.

The stylesheet loader imports Default before the separate eager variant-glob module, at the original theme position between library and component CSS. main.jsx imports that loader and eagerly discovers the JSON manifests. This module boundary prevents Vite from hoisting the variant CSS before the existing library/default styles; adding a theme still changes only its own pair.

The selector-scope gate (also run by the web Node test suite) requires every non-default selector branch, including rules inside media/supports conditions, to begin with that variant's own :root[data-theme="<id>"]. Write plain :root[data-theme="<id>"] ... selectors: no native CSS nesting and no :is()/:where() wrapping of the root constraint. Those semantically safe forms are conservatively rejected; :is() within an already root-constrained descendant selector remains supported.

Variant @import, @font-face, @property and @keyframes (including vendor-prefixed keyframes) are rejected with explicit messages. Imports bypass selector inspection; the others register global names. Bundled font assets remain permitted by the font policy above, but global font registration must be handled separately from variant files. default.css is exempt on trust, not by a scope check, and can affect every theme. This gate checks selector branches and the listed at-rules; it does not certify every CSS effect, contrast or rendered accessibility.

## One-command wave check

Prerequisite: run `npm --prefix web ci` to install the lockfile-pinned dependencies (including PostCSS 8.5.29), then `npm --prefix web run build` so the full harness unit suite has `web/dist`; a bare worktree or system PostCSS is not equivalent.

From your isolated theme branch, run the worker through the normal heavy queue:

    /home/hunter/workspace/poolrad-heavy.sh .venv/bin/python tools/check_theme_wave.py --theme <id> --base <reviewed-main-sha> --artifacts data/theme-wave/<unique-run>

Use your owned detached submitter and completion inbox; submission is not a passing result. A failed run retains its evidence directory, so every retry needs a new --artifacts path. The worker exits nonzero on scope/build/tool errors, raw contrast failures, missing own icon tokens or named rows, incomplete evidence, changed inputs/assets, failed analytical icon overlay bounds, or failed blocking browser checks. It never edits the contrast baseline. Pass each completed exception task explicitly with --completed-task tNNN. A candidate must declare its own --icon-button-border and --icon-button-panel-border even when inherited fallback colors happen to pass.

Static preflight runs before the build/browser work. The full contrast report retains exact reviewed BASELINE-FAIL/KNOWN-FAIL rows and their expiry; it never invents a waiver for a new theme. The contrast report and named rows remain labelled STATIC. After browser capture, overlay-bounds.json computes analytical icon-border bounds from resolved source colors and actual panel computed colors. It uses the current declared --overlay: none, or the supported black scanline gradient (including its token opacity). Unsupported overlays and missing/nonopaque measurements fail closed. Both endpointMinimum and independentContinuousPhaseLowerBound must meet3:1 (the reviewed Nostromo/2001 margin remains3.3:1). This reuses the independently reviewed t205 supplemental calculation; it is not rendered pixel/phase sampling or proof of other effects, other controls, or full accessibility. summary.json links the artifact and its SHA256 in overlay_bounds, with passed, named_rows, computed_rows, min_endpoint_minimum and min_continuous. Analytical bounds are blocking at every width including320 (VP i28625); capture-level320 failures remain named advisories. For fixed colors and opacity the bound is width-independent, but responsive styles may change computed colors; those rows remain blocking. All320 rows are retained. The artifact uses method/source_rows/computed_panel_rows/rows/passed/not_verified and names the theme and --overlay token when a declaration is missing or unsupported. A failed row fails the run; an earlier abort says this stage was not reached.

The existing tests/browser_theme_capture.py fixture runs CoS and Operations for the candidate plus Default/CRT, normal/doubled text, at1422/1067/960/800/700/390 blocking widths and320 advisory. Add --include-1440 explicitly for that additional width. The browser and summary use tools/theme_wave_checks.py ADVISORY_WIDTHS as the shared policy; summary.advisory_widths records it.320 measured failures are named advisories; missing cases, missing images, guard failures and tool errors still fail. Targets must satisfy24×24 for both the union box AND every fragment. Focus containment, picker round trip, actual panel icon colors, and composer paint/noise checks are recorded. An absent closed-dialog icon remains absent; no base control is invented. Paint checks establish glyph visibility, not overlay phase bounds.

summary.json records the exact head/base, hashes of Git-visible web/tools/tests inputs (including build configs and nonignored new theme files; ignored private files are excluded), bundle hashes, command exits, full STATIC contrast and named rows, per-case checks, browser/viewport/dPR, capture hashes and limitations. Closed-page images use ThemeCapture, including an open-dialog rejection control. Intentional open-dialog images are labelled separately. The fixture uses synthetic APIs and an owned temporary loopback server; it cannot send owner messages or use production credentials. Emulated dPR1.8 and doubled CSS text do not establish real zoom, physical-device behavior, unavailable-channel behavior, font conformance or whole-page accessibility.

Open Appearance close-icon hit testing is mandatory evidence at every width. A failed hit test at320 is a named, nonblocking entry in summary.json advisories (case, theme, route, text scale and check); the same failure at a blocking width fails the run. Missing the check itself still fails, including at320. The terminal summary prints the advisory count, so a passing run is not mistaken for zero findings. This diagnostic does not repair the dialog layout.
