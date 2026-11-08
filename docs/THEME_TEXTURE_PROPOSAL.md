# Theme texture layer: options and recommendation

Task t3702 (owner request #1980). Status: **proposal only**. This document adds no theme file,
no CSS, no JavaScript, no image or data-URI asset, and no shared-file change. It states what a
texture would have to satisfy, the options for three effects (sand, glyph rain, grain), and a
recommendation. Nothing here is implemented or verified in a browser. Example values are
descriptions, not code to paste.

Base: origin/main at the branch point (3b2214c8). Every claim about the code is from reading
`ui/src` at that commit. Contrast figures are computed from the base colour tokens only.

Scope decision (coordinator, i49326): the baseline is static, CSS-generated textures through the
existing token and overlay contract. Image or data-URI pipelines and shared animation are not in
scope. They appear only in section 6, as separately scoped future work and tradeoffs.

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
the README sentence as stale and `style.css` as the code. The proposal assumes `style.css` is
correct, and the implementation task must verify it.

**Motion.** The UI sources contain no `transition`, `animation` or `@keyframes`. Nothing in `ui/`
or `web/` handles `prefers-reduced-motion`. Any moving effect would be the first motion in the
desk, and it would need its own reduced-motion gate.

**Contrast gate.** `bridges/tools/theme_contrast.py` checks the token pairs (ink/bg at least 7,
muted/bg at least 4.5, accent/bg at least 3, accent-contrast/accent at least 4.5). It does not
measure an overlay composited over text. The README says so directly.

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

- **C1 Static, token-only.** In v1 a theme adds a static background only through `--overlay`,
  built from CSS gradients. Nothing animates, and no keyframes are added.
- **C2 Readability.** The overlay sits above content (`z-index: 100`) and lowers the effective
  contrast of every glyph under it. The token gate does not see this, so the implementation task
  must measure rendered text over the texture.
- **C3 Reduced motion.** Not relevant to v1, which has no motion. It becomes binding for any future
  motion (section 6): off under `prefers-reduced-motion: reduce`, with an explicit off switch.
- **C4 Cost.** The layer is one full-viewport fixed element. A static CSS gradient is expected to
  be cheap. Costs are expectations, not measurements; each option states its own.
- **C5 Print and forced colours.** The existing consumer removes the overlay under `print` and
  `forced-colors`. Keep that.
- **C6 CRT fidelity.** `crt.css` is a fidelity exception with its own overlay. The proposal does
  not change it.

## 3. Options per effect (v1, CSS-only)

### 3.1 Sand (dune)

- **A. Static dune dither (recommended).** A very low-alpha dither built from layered repeating
  gradients at two or three angles with small periods, in a warm tone, at roughly 0.03 to 0.06
  alpha. Mechanism: token only, existing consumer, no image. Motion: none. Cost: expected to be
  negligible. Readability: dune's base pairs leave about 13.5:1 for ink on surface, so a faint
  texture has room; verify the composite. Limitation: a regular gradient pattern can band or moiré
  and reads as grain, not as moving sand.

### 3.2 Glyph rain (blade-runner)

- **C. Static rain streaks (recommended for v1).** Sparse vertical linear gradients at very low
  alpha, suggesting falling columns, with no glyphs. Mechanism: token only, existing consumer.
  Motion: none. Cost: expected to be negligible. Readability: keep alpha at or below about 0.05 and
  verify the composite. This does not look like glyph rain; it is a static suggestion of it.
- **Glyph rain proper** (individual glyphs, falling or animated) needs an image or a shared
  animation. Both are out of v1 scope. See section 6.

### 3.3 Grain (metropolis)

- **F. Static grain (recommended for metropolis).** Several small-period repeating gradients at
  different angles, combined at very low alpha, to approximate film grain. Mechanism: token only,
  existing consumer, no data URI. Motion: none. Cost: expected to be negligible. Readability:
  metropolis has about 16:1 for ink on surface, so grain at low alpha has room; verify the
  composite. Limitation: a gradient approximation is less natural than a true noise texture and
  may show moiré on some screens.
- **G. Phosphor grain for wargames (not recommended).** Wargames already has a phosphor character,
  and the shared `--scanline-strength` mechanism is the more faithful tool. Keep grain out of
  wargames, or keep it well below the scanline strength.

## 4. Per-theme recommendation (v1)

| Theme | Effect | Option | Mechanism | Motion | Readability to verify |
|---|---|---|---|---|---|
| dune | sand | A static dither | `--overlay` gradients | none | ink on surface 13.5:1 base; composite |
| blade-runner | rain | C static streaks | `--overlay` gradients | none | low alpha; composite |
| metropolis | grain | F static grain | `--overlay` gradients | none | 16:1 base; composite |
| wargames | (none) | G not recommended | scanlines already available | none | keep phosphor clear |

## 5. Recommendation

1. Ship the v1 options only: A for dune, C for blade-runner, and F for metropolis. Each uses
   `--overlay` with CSS gradients only, no shared CSS change, no image, no data URI, and no motion.
2. Before any implementation, resolve the README and `style.css` discrepancy in section 1.
3. Add a composite-contrast check: render sample text over each texture and measure the result
   against 4.5:1. The current gate measures token pairs only.
4. Do not build glyph rain proper, animated sand, or animated grain in v1.

## 6. Future work (separately scoped, not in v1)

These are tradeoffs for later decisions. They are not in scope and need their own tasks.

- **Repo-local licensed assets or data URIs.** A glyph tile for blade-runner, or an SVG
  `feTurbulence` grain for metropolis, would be more faithful than gradients. Tradeoffs: the README
  does not allow image assets today, so a theme-owned asset directory and licence files would
  need a contract change. Assets must be repo-local, with no external fetch. A data URI keeps the
  file count down but makes each theme's CSS heavier. An SVG filter is expected to rasterise once
  per size and cost more on resize; a pre-rendered tile is expected to be cheaper. Nothing here is
  measured.
- **Shared reduced-motion-aware animation.** Drifting sand (B), animated rain columns (D), and
  JavaScript canvas rain (E) all need a shared mechanism. Tradeoffs: this is the first motion in the
  desk, so it needs a reduced-motion gate that is on by default, a pause when the tab is hidden,
  and a frame cap for any JavaScript effect. Animated full-viewport layers are expected to repaint
  often. Canvas rain is the most faithful and the most expensive.

## 7. Verification plan (for the implementation task, not this proposal)

- Browser capture of each theme with the overlay on and off, at the blocking widths in the README
  (1422, 1067, 960, 800, 700, 390), recording the CSS viewport and raw devicePixelRatio.
- Measured contrast of sample text over the texture (composite), against 4.5:1 for body text.
- Print and forced-colours emulation: the overlay must be absent.
- Cost: paint timing on scroll and on resize with and without the texture. These are not measured
  here.

## 8. Not verified and open questions

- No browser run. Contrast figures are base token pairs only. Composite contrast is unknown.
- The README and `style.css` disagree about the overlay consumers (section 1). Not resolved here.
- Performance is not measured. Every cost statement is an expectation.
- Image assets, data URIs, and shared animation are out of v1 scope (coordinator, i49326). Section 6
  lists them as future work only.

Questions for the coordinator go through the coordinator pane, not the owner mailbox.
