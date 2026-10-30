"""Analytical icon bounds, not a rendered-overlay or general CSS proof.

Reuses the independently reviewed calculation from codex-design's
data/design-evidence/theme-wave-current/run.py (t205, i27591/i27606).
Unlike that supplemental script, opacity comes from current declared tokens,
and row coverage comes from the requested wave rather than a fixed 96 cases.
"""
import math
import re
from tools import theme_contrast as contrast

ICON_PAIRS = ('icon-button-border/own-surface', 'icon-button-panel-border/own-surface')


def overlay_alpha(tokens):
    if 'overlay' not in tokens:
        raise ValueError('Missing --overlay declaration')
    value = contrast.resolve('overlay', tokens)
    value = re.sub(r'var\(\s*--([\w-]+)\s*\)',
                   lambda m: contrast.resolve(m[1], tokens), value)
    compact = re.sub(r'\s+', '', value)
    if compact == 'none':
        return 0.0
    match = re.fullmatch(
        r'repeating-linear-gradient\(0deg,(?:rgba\(0,0,0,([0-9.]+)\)|'
        r'rgb\(000/([0-9.]+)\))01px,transparent1px4px\)', compact)
    if not match:
        raise ValueError('Unsupported --overlay: ' + value)
    alpha = float(next(v for v in match.groups() if v is not None))
    if not math.isfinite(alpha) or not 0 <= alpha <= 1:
        raise ValueError('Invalid black overlay opacity')
    return alpha


def rgb(value):
    if not isinstance(value, str):
        raise ValueError('Missing color')
    if value.startswith(('rgb(', 'rgba(')):
        match = re.fullmatch(r'rgba?\(\s*([0-9.]+)[, ]+([0-9.]+)[, ]+([0-9.]+)'
                             r'(?:\s*[,/]\s*([0-9.]+))?\s*\)', value)
        if not match or (match[4] is not None and float(match[4]) != 1):
            raise ValueError('Unsupported or nonopaque color: ' + value)
        result = tuple(float(match[i]) for i in (1, 2, 3))
    else:
        result = contrast.color(value)
    if any(not math.isfinite(c) or not 0 <= c <= 255 for c in result):
        raise ValueError('Invalid color: ' + value)
    return result


def luminance(color):
    channels = [v / 255 for v in color]
    return sum((v / 12.92 if v <= .04045 else ((v + .055) / 1.055) ** 2.4) * w
               for v, w in zip(channels, (.2126, .7152, .0722)))


def bounds(foreground, background, alpha):
    f, b = rgb(foreground), rgb(background)
    fd, bd = ([n * (1 - alpha) for n in color] for color in (f, b))
    def ratio(a, b):
        a, b = luminance(a), luminance(b)
        return (max(a, b) + .05) / (min(a, b) + .05)
    endpoints = dict(static=ratio(f, b), both=ratio(fd, bd),
                     foregroundOnly=ratio(fd, b), backgroundOnly=ratio(f, bd))
    fi, bi = sorted((luminance(f), luminance(fd))), sorted((luminance(b), luminance(bd)))
    continuous = (1 if max(fi[0], bi[0]) <= min(fi[1], bi[1]) else
                  (fi[0] + .05) / (bi[1] + .05) if fi[0] > bi[1] else
                  (bi[0] + .05) / (fi[1] + .05))
    return dict(alpha=alpha, foreground=foreground, background=background, endpoints=endpoints,
                endpointMinimum=min(endpoints.values()), independentContinuousPhaseLowerBound=continuous)


def overlay_proof(summary, tokens):
    rows, errors = [], []
    themes = summary['themes']
    expected = {f'{t}-{v}-{w}-text{s}' for t in themes for v in ('cos', 'operations')
                for w in summary['widths'] for s in (1, 2)}
    cases = summary.get('browser', {}).get('cases', [])
    ids = [c.get('id') for c in cases]
    if not expected or len(ids) != len(set(ids)) or set(ids) != expected:
        errors.append('Missing, duplicate or unexpected computed cases')

    def add(theme, pair, measurement, fg, bg, **metadata):
        label = theme + ' ' + pair + ((' width ' + str(metadata['width'])) if 'width' in metadata else '')
        try:
            if theme not in tokens:
                raise ValueError('Missing --overlay token map for theme')
            row = dict(theme=theme, pair=pair, measurement=measurement, **metadata,
                       **bounds(fg, bg, overlay_alpha(tokens[theme])))
            # Preserve the reviewed Nostromo/2001 icon margin.
            row['requiredMinimum'] = 3.3 if theme in ('nostromo', '2001') else 3
            row['passed'] = (row['endpointMinimum'] >= row['requiredMinimum'] and
                             row['independentContinuousPhaseLowerBound'] >= row['requiredMinimum'])
            rows.append(row)
            if not row['passed']:
                errors.append(label + ': contrast below required minimum')
        except (KeyError, TypeError, ValueError) as error:
            errors.append(label + ': ' + str(error))

    for theme in themes:
        for pair in ICON_PAIRS:
            matches = [r for r in summary.get('named_rows', []) if r['theme'] == theme and r['pair'] == pair]
            if len(matches) != 1:
                errors.append(theme + ' ' + pair + ': missing or duplicate source row')
                continue
            row = matches[0]
            add(theme, pair, 'source-resolved', row.get('foreground_value'), row.get('background_value'))
    for case in cases:
        icon = case.get('iconOpen')
        if case.get('theme') not in themes or not isinstance(icon, dict) or icon.get('opaque') is not True:
            errors.append(str(case.get('id')) + ': missing/nonopaque computed icon')
            continue
        add(case['theme'], 'panel computed color', 'browser computed color',
            icon.get('border'), icon.get('surface'), case=case['id'], width=case.get('width'),
            view=case.get('view'), textScale=case.get('textScale'))
    method = ('Analytical solid-color bounds for icon borders under the declared black --overlay only; '
              'source-resolved base/panel rows and browser-computed panel colors. '
              'Analytical bounds are blocking at every width including 320 (VP i28625); '
              'capture-level 320 failures remain named advisories. For fixed colors/opacity the bound '
              'is width-independent; responsive styles may change computed colors, which remain blocking.')
    return dict(method=method, scope=method,
                not_verified=['Rendered pixel/phase sampling', 'Other effects and controls', 'Full accessibility'],
                source_head=summary.get('head'), bundle=summary.get('bundle'),
                source_rows=sum(r['measurement'] == 'source-resolved' for r in rows),
                computed_panel_rows=sum(r['measurement'] == 'browser computed color' for r in rows),
                passed=bool(rows) and not errors, errors=errors, rows=rows)
