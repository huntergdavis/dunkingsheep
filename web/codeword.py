"""Programmatic code-wheel recognition for the DOS play desk.

This is a faithful, model-free port of the desk's existing browser solver in
``web/src/codewheel.js`` (the same ``bitmap``/``normalize``/``overlap`` primitives,
path classification, and conservative abstain thresholds). It runs entirely on
the host: the private rune GIFs are decoded into 32x32 masks in memory and are
never sent anywhere. No language model is involved in detection or typing.

The port keeps the original thresholds so a DOS frame that the browser would
abstain on also abstains here, rather than inventing a code word.
"""

from __future__ import annotations

import subprocess
import threading
from pathlib import Path

WORDS = ['AXEIAX', 'BEWARE', 'COPPER', 'DRAGON', 'EFREET', 'FRIEND', 'GOOGLE', 'HARASH',
         'IXYVSI', 'JUNGLE', 'KNIGHT', 'LQLGMT', 'MLSSXS', 'NOTNOW', 'OTTAWA', 'POOLRD',
         'QUOHOG', 'RHUDIA', 'SAVIOR', 'TEMPLE', 'UICDRH', 'VULCAN', 'WYVERN', 'XRSEHK',
         'YUFSTA', 'ZOMBIE', '1GKKRY', '2IOLCD', '3MASAI', '4NINER', '5GUNGA', '6BROWN',
         '7GNATS', '80ASIS', '9TROUT', '0SOMAS']
RINGS = ['esp', 'det']
PATH_NAMES = ['dots', 'dash / two dots', 'dashes']


def wheel_word(a, b, path):
    """Return the code word for two rune numbers and a path id (0/1/2)."""
    if not all(isinstance(n, int) and 1 <= n <= 36 for n in (a, b)) or path not in (0, 1, 2):
        return None
    word = WORDS[(a + b - 1 + path * 12) % len(WORDS)]
    return word[1:] if word[:1].isdigit() else word


def bitmap_rgb(rgb: bytes, width: int, height: int, kind: str = 'rune') -> dict:
    """Mask an RGB24 buffer the same way ``bitmap()`` masks a browser frame."""
    pixels = bytearray(width * height)
    for i in range(width * height):
        r, g, b = rgb[i * 3], rgb[i * 3 + 1], rgb[i * 3 + 2]
        if kind == 'template':
            hit = (r + g + b) / 3 < 128
        else:
            hit = r > 130 and r > g * 1.6 and r > b * 1.6
        pixels[i] = 1 if hit else 0
    return {'width': width, 'height': height, 'pixels': pixels}


def _bounds(points, width: int) -> dict:
    x0 = y0 = 1 << 30
    x1 = y1 = -1
    for i in points:
        x, y = i % width, i // width
        x0 = min(x0, x); y0 = min(y0, y); x1 = max(x1, x); y1 = max(y1, y)
    return {'x': x0, 'y': y0, 'width': x1 - x0 + 1, 'height': y1 - y0 + 1, 'points': points}


def _components(mask: dict) -> list[dict]:
    width, height, pixels = mask['width'], mask['height'], mask['pixels']
    seen = bytearray(len(pixels))
    result = []
    for start in range(len(pixels)):
        if not pixels[start] or seen[start]:
            continue
        todo = [start]
        points = []
        seen[start] = 1
        while todo:
            i = todo.pop()
            x, y = i % width, i // width
            points.append(i)
            for dy in (-1, 0, 1):
                for dx in (-1, 0, 1):
                    nx, ny = x + dx, y + dy
                    if 0 <= nx < width and 0 <= ny < height:
                        nxt = ny * width + nx
                        if pixels[nxt] and not seen[nxt]:
                            seen[nxt] = 1
                            todo.append(nxt)
        if len(points) >= 3:
            result.append(_bounds(points, width))
    return result


def normalize(mask: dict) -> bytearray:
    """Aspect-preserving 32x32 resample, matching the browser normalizer."""
    points = [i for i, value in enumerate(mask['pixels']) if value]
    out = bytearray(32 * 32)
    if not points:
        return out
    box = _bounds(points, mask['width'])
    scale = 28 / max(box['width'], box['height'])
    w = max(1, round(box['width'] * scale))
    h = max(1, round(box['height'] * scale))
    left = (32 - w) // 2
    top = (32 - h) // 2
    for y in range(h):
        sy = box['y'] + min(box['height'] - 1, int((y + 0.5) * box['height'] / h))
        for x in range(w):
            sx = box['x'] + min(box['width'] - 1, int((x + 0.5) * box['width'] / w))
            out[(top + y) * 32 + left + x] = mask['pixels'][sy * mask['width'] + sx]
    return out


def _overlap(a: bytearray, b: bytearray) -> float:
    total = sum(a) + sum(b)
    if not total:
        return 0.0
    best = 0.0
    for dy in (-1, 0, 1):
        for dx in (-1, 0, 1):
            shared = 0
            for y in range(1, 31):
                for x in range(1, 31):
                    if a[y * 32 + x] and b[(y + dy) * 32 + x + dx]:
                        shared += 1
            best = max(best, 2 * shared / total)
    return best


def rank_rune(mask: dict, templates: list[dict], ring: str) -> list[dict]:
    sample = normalize(mask)
    ranked = [{'number': t['number'], 'ring': ring, 'score': _overlap(sample, t['normalized'])}
              for t in templates if t['ring'] == ring]
    ranked.sort(key=lambda item: item['score'], reverse=True)
    return ranked


def _path_for(row: list[dict]):
    if len(row) < 3:
        return None
    heights = [c['height'] for c in row]
    if max(heights) > min(heights) * 1.8:
        return None
    kinds = []
    for c in row:
        ratio = c['width'] / c['height']
        kinds.append('dash' if ratio >= 2 else 'dot' if ratio <= 1.5 else '?')
    if '?' in kinds:
        return None
    ordered = sorted(row, key=lambda c: c['x'])
    if all(k == kinds[0] for k in kinds):
        gaps = [ordered[i + 1]['x'] + ordered[i + 1]['width'] / 2 - ordered[i]['x'] - ordered[i]['width'] / 2
                for i in range(len(ordered) - 1)]
        if max(gaps) > min(gaps) * 1.35:
            return None
        return 0 if kinds[0] == 'dot' else 2
    if len(ordered) < 6:
        return None
    sequence = ['dash' if c['width'] / c['height'] >= 2 else 'dot' for c in ordered]
    for phase in (0, 1, 2):
        if all(k == ('dash' if (i + phase) % 3 == 0 else 'dot') for i, k in enumerate(sequence)):
            return 1
    return None


def segment_frame(mask: dict) -> dict:
    width, height = mask['width'], mask['height']
    parts = [c for c in _components(mask)
             if c['x'] > 2 and c['y'] > 2 and c['x'] + c['width'] < width - 2 and c['y'] + c['height'] < height - 2
             and c['width'] < width * 0.18 and c['height'] < height * 0.24]
    rows: list[list[dict]] = []
    for c in parts:
        if c['height'] <= height * 0.035 and c['width'] <= width * 0.06:
            cy = c['y'] + c['height'] / 2
            row = next((r for r in rows
                        if abs(r[0]['y'] + r[0]['height'] / 2 - cy) <= max(2, height * 0.006)), None)
            if row is None:
                row = []
                rows.append(row)
            row.append(c)
    paths = [{'row': row, 'path': _path_for(row)} for row in rows]
    paths = [p for p in paths if p['path'] is not None]
    path = paths[0]['path'] if len(paths) == 1 else None
    path_ids = {id(c) for p in paths for c in p['row']}
    groups = [c for c in parts if id(c) not in path_ids]
    gap = max(2, round(width / 80))
    merged = True
    while merged:
        merged = False
        for i in range(len(groups)):
            for j in range(i + 1, len(groups)):
                a, b = groups[i], groups[j]
                if (a['x'] > b['x'] + b['width'] + gap or b['x'] > a['x'] + a['width'] + gap
                        or a['y'] > b['y'] + b['height'] + gap or b['y'] > a['y'] + a['height'] + gap):
                    continue
                together = _bounds(a['points'] + b['points'], width)
                if together['width'] >= width * 0.18 or together['height'] >= height * 0.24:
                    continue
                groups[i] = together
                groups.pop(j)
                merged = True
                break
            if merged:
                break
    glyphs = []
    for c in groups:
        pixels = bytearray(c['width'] * c['height'])
        for i in c['points']:
            pixels[(i // width - c['y']) * c['width'] + i % width - c['x']] = 1
        glyphs.append({'width': c['width'], 'height': c['height'], 'pixels': pixels})
    return {'glyphs': glyphs, 'path': path}


def recognize_wheel(mask: dict, templates: list[dict]) -> dict:
    segment = segment_frame(mask)
    glyphs, path = segment['glyphs'], segment['path']

    def abstain(reason):
        return {'confident': False, 'reason': reason, 'path': path, 'glyph_count': len(glyphs)}

    if len(glyphs) != 2:
        return abstain('A complete pair of wheel runes was not found.')
    if path is None:
        return abstain('The path is missing or ambiguous.')
    for ring in RINGS:
        if len({t['number'] for t in templates if t['ring'] == ring}) != 36:
            return abstain('The private rune templates are incomplete.')
    assignments = []
    for first, second in ((0, 1), (1, 0)):
        esp, det = rank_rune(glyphs[first], templates, 'esp'), rank_rune(glyphs[second], templates, 'det')
        assignments.append({'esp': esp, 'det': det, 'score': esp[0]['score'] + det[0]['score']})
    assignments.sort(key=lambda item: item['score'], reverse=True)
    best = assignments[0]
    if (best['score'] - assignments[1]['score'] < 0.12
            or any(r[0]['score'] < 0.90 or r[0]['score'] - r[1]['score'] < 0.08 for r in (best['esp'], best['det']))):
        return abstain('The rune shapes do not match clearly enough.')
    a, b = best['esp'][0]['number'], best['det'][0]['number']
    return {'confident': True, 'a': a, 'b': b, 'path': path, 'word': wheel_word(a, b, path),
            'glyph_count': 2, 'signature': f'{a}:{b}:{path}'}


# --- Private GIF decoding with an in-process cache ---------------------------------

_TEMPLATE_CACHE: dict[str, tuple] = {}
_TEMPLATE_LOCK = threading.Lock()


def _parse_ppm(data: bytes) -> tuple[bytes, int, int]:
    fields = []
    pos = 0
    while len(fields) < 4:
        if pos >= len(data):
            raise ValueError('truncated PPM header')
        ch = data[pos]
        if ch == ord('#'):
            while data[pos] != 10:
                pos += 1
            continue
        if ch <= 32:
            pos += 1
            continue
        start = pos
        while pos < len(data) and data[pos] > 32:
            pos += 1
        fields.append(data[start:pos].decode('ascii'))
    pos += 1
    width, height = int(fields[1]), int(fields[2])
    return data[pos:pos + width * height * 3], width, height


def _gif_to_rgb(path: Path) -> tuple[bytes, int, int]:
    result = subprocess.run(['convert', str(path), '-depth', '8', 'ppm:-'],
                            check=True, capture_output=True, timeout=30)
    return _parse_ppm(result.stdout)


def load_templates(directory: Path) -> list[dict]:
    """Decode the private rune GIFs into normalized masks. Requires ImageMagick."""
    templates = []
    for ring in RINGS:
        for number in range(1, 37):
            rgb, width, height = _gif_to_rgb(directory / f'{ring}{number:02d}.gif')
            templates.append({'ring': ring, 'number': number,
                              'normalized': normalize(bitmap_rgb(rgb, width, height, 'template'))})
    return templates


def templates_cached(directory: Path) -> list[dict]:
    """Return templates for ``directory``, reloading only when the GIFs change."""
    directory = Path(directory)
    try:
        stamps = tuple(sorted((p.name, p.stat().st_mtime_ns, p.stat().st_size)
                              for p in directory.glob('*.gif')))
    except OSError:
        stamps = ()
    key = str(directory)
    with _TEMPLATE_LOCK:
        cached = _TEMPLATE_CACHE.get(key)
        if cached and cached[0] == stamps:
            return cached[1]
    templates = load_templates(directory)
    with _TEMPLATE_LOCK:
        _TEMPLATE_CACHE[key] = (stamps, templates)
    return templates
