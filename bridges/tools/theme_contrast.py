#!/usr/bin/env python3
"""Project theme contrast gate; no CSS mutation or copied palette.

Reads unconditional :root and :root[data-theme=...] token blocks (including
repeated blocks and inherited var aliases). Contrast colors must resolve to
hex colors; foreground alpha composites over the opaque paired background.
Unsupported conditional/complex token overrides fail closed.
The floors are project policy, not a claim of whole-page WCAG conformance.
Formula: https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html
"""
import argparse
import json
from pathlib import Path
import re
import sys

PAIRS = (("ink", "bg", 7.0), ("muted", "bg", 4.5),
         ("accent", "bg", 3.0), ("accent-contrast", "accent", 4.5))
DEFAULT_CSS = Path(__file__).resolve().parents[1] / "web/src/themes"
THEME_SELECTOR = re.compile(r':root\[data-theme\s*=\s*(?:"([\w-]+)"|\'([\w-]+)\'|([\w-]+))\]')


def read_css(source=DEFAULT_CSS):
    """Read one legacy CSS file or all theme files, with base tokens first."""
    source = Path(source)
    if source.is_file():
        return source.read_text()
    files = sorted(source.glob('*.css'), key=lambda file: (file.name != 'default.css', file.name))
    if not files:
        raise ValueError(f'No theme CSS files found: {source}')
    return '\n'.join(file.read_text() for file in files)


def blocks(css):
    """Yield top-level rule bodies, preserving conditional context for rejection."""
    css = re.sub(r'/\*.*?\*/', '', css, flags=re.S)
    cursor = 0
    while (opening := css.find('{', cursor)) >= 0:
        selector = css[cursor:opening].strip()
        depth, quote, escaped, end = 1, None, False, opening + 1
        while end < len(css) and depth:
            char = css[end]
            if quote:
                if escaped: escaped = False
                elif char == '\\': escaped = True
                elif char == quote: quote = None
            elif char in ('"', "'"): quote = char
            elif char == '{': depth += 1
            elif char == '}': depth -= 1
            end += 1
        if depth: raise ValueError('Unclosed CSS rule')
        yield selector, css[opening + 1:end - 1]
        cursor = end
    if css[cursor:].strip(): raise ValueError('Unsupported trailing CSS')


def theme_tokens(css):
    base, overrides = {}, {}
    for selector, body in blocks(css):
        declarations = dict(re.findall(r'--([\w-]+)\s*:\s*([^;{}]+)(?:;|$)', body))
        match = THEME_SELECTOR.fullmatch(selector)
        if selector == ':root': target = base
        elif match: target = overrides.setdefault(next(v for v in match.groups() if v), {})
        elif (len(branches := [part.strip() for part in selector.split(',')]) == 2 and
              ':root:not([data-theme])' in branches and
              any((m := THEME_SELECTOR.fullmatch(part)) and
                  next(v for v in m.groups() if v) == 'default' for part in branches)):
            # Both no-theme and explicit Default use these values. Other themes
            # inherit only generic :root, so their fallback branches stay active.
            target = overrides.setdefault('default', {})
        else:
            if declarations:
                raise ValueError(f'Unsupported token selector: {selector}')
            continue
        if '{' in body: raise ValueError(f'Nested token rule: {selector}')
        target.update({key: value.strip() for key, value in declarations.items()})
    if not base: raise ValueError('No :root token block found')
    themes = {'default': {**base, **overrides.pop('default', {})}}
    themes.update({name: {**base, **values} for name, values in overrides.items()})
    return themes


def resolve(name, tokens, seen=()):
    if name in seen: raise ValueError('Cyclic token alias: ' + ' -> '.join((*seen, name)))
    if name not in tokens: raise ValueError(f'Missing --{name}')
    value = tokens[name]
    alias = re.fullmatch(r'var\(\s*--([\w-]+)\s*(?:,\s*(.+))?\)', value)
    if alias:
        reference, fallback = alias.groups()
        if reference in tokens: return resolve(reference, tokens, (*seen, name))
        if fallback is not None: return resolve(name, {**tokens, name: fallback}, seen)
        raise ValueError(f'Missing --{reference} referenced by --{name}')
    return value


def rgba(value):
    value = {'white': '#ffffff', 'black': '#000000', 'transparent': '#00000000'}.get(value.lower(), value)
    if not re.fullmatch(r'#[0-9a-fA-F]+', value) or len(value) not in (4, 5, 7, 9):
        raise ValueError(f'Expected a CSS hex color, got {value!r}')
    digits = value[1:]
    if len(digits) in (3, 4): digits = ''.join(char * 2 for char in digits)
    channels = tuple(int(digits[i:i + 2], 16) for i in range(0, len(digits), 2))
    return (*channels[:3], channels[3] / 255 if len(channels) == 4 else 1.0)


def color(value):
    red, green, blue, alpha = rgba(value)
    if alpha != 1: raise ValueError('Paired background must be opaque; its underlying surface is unknown')
    return red, green, blue


def foreground_color(value, background):
    *rgb, alpha = rgba(value)
    return tuple(channel * alpha + behind * (1 - alpha) for channel, behind in zip(rgb, background))


def luminance(rgb):
    channels = [value / 255 for value in rgb]
    linear = [value / 12.92 if value <= 0.04045 else ((value + 0.055) / 1.055) ** 2.4 for value in channels]
    return sum(weight * value for weight, value in zip((0.2126, 0.7152, 0.0722), linear))


def contrast(first, second):
    light, dark = sorted((luminance(first), luminance(second)), reverse=True)
    return (light + 0.05) / (dark + 0.05)


def passing_foreground(foreground, background, floor):
    """Suggest a verified passing mix toward black/white; never change the CSS."""
    candidates = []
    for endpoint in (0, 255):
        for step in range(1, 256):
            candidate = tuple(round(channel + (endpoint - channel) * step / 255) for channel in foreground)
            if contrast(candidate, background) >= floor:
                candidates.append(candidate)
                break
    if not candidates: return None  # No foreground can meet this floor on this background.
    best = min(candidates, key=lambda candidate: sum((a - b) ** 2 for a, b in zip(candidate, foreground)))
    return '#' + ''.join(f'{channel:02x}' for channel in best)


def measure(css):
    """Original palette calibration; the CLI additionally gates CSS consumers."""
    rows = []
    for theme, tokens in theme_tokens(css).items():
        for foreground, background, floor in PAIRS:
            pair = f'{foreground}/{background}'
            try:
                foreground_value, background_value = resolve(foreground, tokens), resolve(background, tokens)
                bg = color(background_value)
                fg = foreground_color(foreground_value, bg)
                ratio = contrast(fg, bg)
            except ValueError as error:
                raise ValueError(f'{theme} {pair}: {error}') from error
            state = 'FAIL' if ratio < floor else 'MARGINAL' if ratio <= floor + 0.3 else 'PASS'
            suggestion = passing_foreground(fg, bg, floor) if state == 'FAIL' else None
            rows.append(dict(theme=theme, pair=pair, foreground=foreground, ratio=ratio,
                             floor=floor, status=state, foreground_value=foreground_value, background_value=background_value, suggestion=suggestion,
                             suggestion_ratio=contrast(color(suggestion), bg) if suggestion else None))
    return rows


def measure_all(css, consumers=None, expired_known_fails=()):
    try:
        from . import theme_contrast_consumers as usage
    except ImportError:
        import theme_contrast_consumers as usage
    if consumers is None: consumers = usage.read_consumers()
    rows = usage.measure_consumers(css, consumers, measure(css), sys.modules[__name__], expired_known_fails)
    return (rows + usage.measure_icon_button(css, consumers, sys.modules[__name__]) +
            usage.measure_attach(css, consumers, sys.modules[__name__]))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('css', nargs='?', type=Path, default=DEFAULT_CSS, help='theme directory (default) or a single legacy CSS file')
    parser.add_argument('--json', action='store_true', help='emit structured evidence')
    parser.add_argument('--consumer-css', action='append', type=Path,
                        help='CSS consumer source (repeatable; defaults to style.css and dash.css)')
    parser.add_argument('--baseline', type=Path, help='design-triaged exact-pair baseline with expiry tasks')
    parser.add_argument('--no-baseline', action='store_true', help='report all raw failures without baseline deferrals')
    parser.add_argument('--expire-known-fail', '--completed-task', action='append', default=[],
                        help='task completed: expire its named or baseline exceptions (repeatable)')
    args = parser.parse_args(argv)
    try:
        try:
            from . import theme_contrast_consumers as usage
        except ImportError:
            import theme_contrast_consumers as usage
        consumers = usage.read_consumers(args.consumer_css or usage.DEFAULT_CONSUMERS)
        if any(not re.fullmatch(r't[0-9]+', task) for task in args.expire_known_fail):
            raise ValueError('Completed task identifiers must be t followed by digits')
        if args.baseline and args.no_baseline: raise ValueError('Choose --baseline or --no-baseline, not both')
        baseline_path = None if args.no_baseline else (args.baseline or usage.DEFAULT_BASELINE)
        rows = measure_all(read_css(args.css), consumers, args.expire_known_fail)
        if baseline_path:
            rows = usage.apply_baseline(rows, json.loads(baseline_path.read_text()), args.expire_known_fail)
    except (OSError, ValueError) as error:
        print(json.dumps({'error': str(error)}) if args.json else f'ERROR: {error}')
        return 2
    if args.json: print(json.dumps({'source': str(args.css), 'consumer_parser': consumers['parser'],
                                   'expired_known_fails': args.expire_known_fail,
                                   'baseline': str(baseline_path) if baseline_path else None,
                                   'scope': 'Static named pairs; not rendered/cascade/DOM ancestry conformance',
                                   'pairs': rows}, indent=2))
    else:
        for row in rows:
            if row.get('error'):
                print(f"{row['status']} {row['theme']} {row['pair']}: {row['error']}")
                continue
            print(f"{row['status']} {row['theme']} {row['pair']}: {row['ratio']:.2f}:1 ({row['foreground_value']} on {row['background_value']}; floor {row['floor']:g}:1)")
            if row.get('known_fail'):
                print(f"  NAMED {row['known_fail']['name']} expires with {row['known_fail']['expires_with']}; expired={row['known_fail']['expired']}")
            if row.get('baseline'):
                print(f"  BASELINE expires with {row['baseline']['expires_with']}; expired={row['baseline']['expired']}; scope_changed={row['baseline']['scope_changed']}: {row['baseline']['reason']}")
            if row['status'] in ('FAIL', 'KNOWN-FAIL', 'BASELINE-FAIL'):
                if row['suggestion']:
                    print(f"  --{row['foreground']}: {row['suggestion']} WOULD PASS this pair at {row['suggestion_ratio']:.2f}:1; rerun all pairs after editing.")
                elif row.get('control'):
                    print('  Fix the named control border/surface; rerun all checks.')
                else: print('  No foreground can pass on this background; change the background token.')
    return int(any(row['status'] == 'FAIL' for row in rows))


if __name__ == '__main__':
    raise SystemExit(main())
