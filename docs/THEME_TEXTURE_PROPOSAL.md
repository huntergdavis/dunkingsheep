# Theme texture layer: options and recommendation

Task t3702 (owner request #1980). Status: **proposal only**. This document adds no theme file,
no CSS, no JavaScript and no shared-file change. It states what a texture would have to satisfy,
the options for three effects (sand, glyph rain, grain), and a recommendation. Nothing here is
implemented or verified in a browser.

Base: origin/main at the branch point (3b2214c8). Every claim about the code is from reading
`ui/src` at that commit. Contrast figures are computed from the base colour tokens only.

## 1. What exists today

**Themes.** Eleven non-default themes are registered as token pairs in `ui/src/themes/`:
2001, blade-runner, crt, dune, forbidden-planet, imperial, lcars, metropolis, nostromo, tron,
wargames. Default is the shared token set. The contract is in `ui/src/themes/README.md`.

**Token-only contract for new themes.** A new theme's CSS may contain only token blocks scoped to
`:root[data-theme="<id>"]`. It may not add component selectors, layout rules, media queries,
animations, `@keyframes` or network font imports. The selector-scope gate enforces this for
variant files.

**Overlay consumers.** Two mechanisms exist:
- `ui/src/style.css` defines one generic fixed layer, `body::after`, for every theme except
  `default` and `crt`. It is `position: fixed; inset: 0; z-index: 100; pointer-events: none` and its
  `background-image` is `var(--overlay, none)`. It is removed under `print` and `forced-colors`.
  The same file gives the same themes a display font on `h1, h2, .dash-nav-brand` and a body font
  on the chat message classes.
- `crt.css` has its own legacy overlay, a repeating 1px dark / 3px clear gradient at alpha 0.12.
  `2001.css` has an interim copy of the `body::after` rule, marked "t177 generalises into token
  hooks".

**Discrepancy to resolve first.** `themes/README.md` says the generic overlay, display-font and
chat body-font consumers "are NOT implemented by this split: the existing effect selectors are
CRT-only". That disagrees with `style.css` on origin/main. Until a browser check settles it, treat
the README sentence as stale and `style.css` as the code. The proposal below assumes `style.css`
is correct, and the implementation task must verify it.

**Motion.** The UI sources contain no `transition`, `animation` or `@keyframes`. Nothing in `ui/`
or `web/` handles `prefers-reduced-motion`. Any moving effect would be the first motion in the
desk, and it would need its own reduced-motion gate.

**Contrast gate.** `bridges/tools/theme_contrast.py` checks the token pairs (ink/bg at least 7, muted/bg at
least 4.5, accent/bg at least 3, accent-contrast/accent at least 4.5). It does not measure an
overlay composited over text. The README says so directly.

**Base headroom.** Computed from the token values (WCAG relative luminance):

| Theme | ink / surface | muted / surface | ink / bg | muted / bg |
|---|---|---|---|---|
| dune | 13.53 | 8.19 | 14.68 | 8.89 |
| blade-runner | 14.05 | 7.19 | 15.38 | 7.87 |
| metropolis | 16.07 | 9.07 | 17.04 | 9.61 |
| wargames | 13.02 | 7.78 | 13.95 | 8.34 |
| tron | 17.10 | 9.10 | 17.96 | 9.56 |

Every base pair clears 7:1, so there is room for a low-alpha texture. Whether the composited text
still clears 4.5:1 is not known until it is measured.

## 2. Constraints every texture must meet

- **C1 Static by default.** A token-only theme can only add a static background through
  `--overlay`. Anything that moves needs a shared mechanism (a keyframes block, or JavaScript)
  that is outside the theme-pair rule. That is a decision for the owner or coordinator.
- **C2 Readability.** The overlay sits above content (`z-index: 100`). It lowers the effective
  contrast of every glyph under it. The token gate does not see this, so the implementation task
  must measure rendered text over the texture.
- **C3 Reduced motion.** No motion exists today and no reduced-motion handling exists. A moving
  effect must be off under `prefers-reduced-motion: reduce`, and it must offer an explicit off
  switch that is not only the OS setting.
- **C4 Cost.** The layer is one full-viewport fixed element. A static gradient is cheap. An SVG
  `feTurbulence` filter is expected to cost more on each resize. A canvas or per-frame JavaScript
  effect is expected to cost the most. These are expectations; nothing here is measured. Each option below states its cost.
- **C5 Print and forced colours.** The existing consumer removes the overlay under `print` and
  `forced-colors`. Keep that.
- **C6 CRT fidelity.** `crt.css` is a fidelity exception with its own overlay. The proposal does
  not change it.

## 3. Options per effect

### 3.1 Sand (dune)

- **A. Static dune dither (recommended).** A `--overlay` that is a very low-alpha dithered
  gradient, roughly 0.03 to 0.06 alpha, in a warm tone. Mechanism: token only, existing consumer.
  Motion: none. Cost: a static gradient, negligible. Readability: dune's base pairs leave about
  13.5:1 for ink on surface, so a faint texture has room. Verify the composite. Limitation: a
  static pattern can band and does not read as moving sand.
- **B. Drifting sand.** Animated background position. Needs C1 (shared keyframes) and C3. Not
  recommended until a shared motion mechanism exists.

### 3.2 Glyph rain (blade-runner)

- **C. Static glyph field (recommended for v1, if assets are allowed).** A pre-rendered tile of
  sparse glyphs at low alpha, supplied as `--overlay` with `url()`. Mechanism: the README permits
  bundled font assets in a theme-owned directory, but it does not say whether image assets are
  allowed. That needs a decision. Motion: none. Cost: one image, decoded once. Readability: keep
  glyph density low and alpha at or below about 0.05, and measure the composite.
- **D. Animated columns (CSS keyframes).** Falling columns through a shared keyframes block. Needs
  C1, C3 and a pause when the tab is hidden. Cost: expected to repaint every frame on a
  full-viewport layer; not measured.
  Not recommended for v1.
- **E. Canvas rain (JavaScript).** The most faithful look. It needs a frame-rate cap, a pause when
  hidden, reduced-motion handling, and a separate module. Cost: the highest of the options. Not
  recommended.

Recommendation for glyph rain: ship option C only after the image-asset question is decided. If
the answer is no, defer glyph rain until a shared motion mechanism exists, and do not fake it with
a static glyph wall that hurts reading.

### 3.3 Grain (metropolis, wargames)

- **F. Static grain (recommended for metropolis).** Film grain as `--overlay`: either a small
  `feTurbulence` SVG in a data URI, or a pre-rendered tile. Mechanism: token only if a data URI is
  allowed, and the same asset question as C applies to a tile. Motion: none. Cost: an SVG filter is
  expected to rasterise once per size and cost more on resize; the tile is expected to be cheaper.
  Neither is measured.
  Readability: grain at low alpha over metropolis's 16:1 base headroom is safe; verify the
  composite. Metropolis is art-deco silver, and grain suits it.
- **G. Phosphor grain for wargames.** Not recommended as a separate grain. Wargames already has a
  phosphor character, and scanlines from the shared `--scanline-strength` mechanism would be more
  faithful. Keep grain out of wargames, or keep it well below the scanline strength.

## 4. Per-theme recommendation

| Theme | Effect | Option | Mechanism | Motion | Readability to verify |
|---|---|---|---|---|---|
| dune | sand | A | `--overlay` gradient | none | ink on surface 13.5:1 base; composite |
| blade-runner | glyph rain | C (after asset decision) | `--overlay` tile | none in v1 | low glyph density; composite |
| metropolis | grain | F | `--overlay` data URI or tile | none | 16:1 base; composite |
| wargames | (none) | G not recommended | scanlines already available | none | keep phosphor clear |

## 5. Recommendation

1. Ship only static options in v1: A for dune, F for metropolis, and C for blade-runner if images
   are allowed. All go through `--overlay` with no shared CSS change and no motion.
2. Decide the shared-motion question separately, because D and E need it. If the owner approves a
   shared animation mechanism, it must include a reduced-motion gate that is on by default, pause
   when hidden, and a frame cap for any JavaScript effect.
3. Before any implementation, resolve the README and `style.css` discrepancy in section 1.
4. Add a composite-contrast check: render sample text over each texture and measure the result
   against 4.5:1. The current gate measures token pairs only.
5. Add a reduced-motion test if any motion is approved.

## 6. Verification plan (for the implementation task, not this proposal)

- Browser capture of each theme with the overlay on and off, at the blocking widths in the README
  (1422, 1067, 960, 800, 700, 390), recording the CSS viewport and raw devicePixelRatio.
- Measured contrast of sample text over the texture (composite), against 4.5:1 for body text.
- Print and forced-colours emulation: the overlay must be absent.
- Cost: paint timing on scroll and on resize with and without the texture, and a check that the
  SVG filter (if used) is rasterised once.
- Reduced motion emulation, if any motion is approved.

## 7. Not verified and open questions

- No browser run. Contrast figures are base token pairs only. Composite contrast is unknown.
- The README and `style.css` disagree about the overlay consumers (section 1). Not resolved here.
- Whether image assets or data URIs are allowed in a theme is not stated in the README. This
  decides options C and F.
- Whether the owner accepts a shared animation mechanism (options D and E). Not decided here.
- Performance numbers are not measured. Costs above are qualitative.

Questions for the coordinator go through the coordinator pane, not the owner mailbox.
