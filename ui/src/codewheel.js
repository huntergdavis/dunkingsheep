// Reference illustrations stay private. This module contains only recognition
// code and the desk's existing wheel lookup, never game/template pixel data.
const WORDS = ['AXEIAX','BEWARE','COPPER','DRAGON','EFREET','FRIEND','GOOGLE','HARASH','IXYVSI','JUNGLE','KNIGHT','LQLGMT','MLSSXS','NOTNOW','OTTAWA','POOLRD','QUOHOG','RHUDIA','SAVIOR','TEMPLE','UICDRH','VULCAN','WYVERN','XRSEHK','YUFSTA','ZOMBIE','1GKKRY','2IOLCD','3MASAI','4NINER','5GUNGA','6BROWN','7GNATS','80ASIS','9TROUT','0SOMAS'];
export const RINGS = ['esp', 'det'];
export const PATH_NAMES = ['dots', 'dash / two dots', 'dashes'];
export function wheelWord(a, b, path) {
  if (![a, b].every(n => Number.isInteger(n) && n >= 1 && n <= 36) || ![0, 1, 2].includes(path)) return null;
  const word = WORDS[(a + b - 1 + path * 12) % WORDS.length];
  return /^[0-9]/.test(word) ? word.slice(1) : word;
}
export const templateUrl = (ring, number) => `/api/game/codewheel/${ring}${String(number).padStart(2, '0')}.gif`;

export function bitmap(image, kind = 'rune') {
  const { width, height, data } = image;
  const pixels = new Uint8Array(width * height);
  for (let i = 0; i < pixels.length; i++) {
    const [r, g, b, a] = data.subarray ? data.subarray(i * 4, i * 4 + 4) : data.slice(i * 4, i * 4 + 4);
    pixels[i] = a > 100 && (kind === 'template' ? (r + g + b) / 3 < 128 : r > 130 && r > g * 1.6 && r > b * 1.6) ? 1 : 0;
  }
  return { width, height, pixels };
}

function bounds(points, width) {
  let x0 = Infinity, y0 = Infinity, x1 = -1, y1 = -1;
  for (const i of points) { const x = i % width, y = Math.floor(i / width); x0 = Math.min(x0, x); y0 = Math.min(y0, y); x1 = Math.max(x1, x); y1 = Math.max(y1, y); }
  return { x: x0, y: y0, width: x1 - x0 + 1, height: y1 - y0 + 1, points };
}

function components(mask) {
  const { width, height, pixels } = mask, seen = new Uint8Array(pixels.length), result = [];
  for (let start = 0; start < pixels.length; start++) {
    if (!pixels[start] || seen[start]) continue;
    const todo = [start], points = []; seen[start] = 1;
    while (todo.length) {
      const i = todo.pop(), x = i % width, y = Math.floor(i / width); points.push(i);
      for (let dy = -1; dy <= 1; dy++) for (let dx = -1; dx <= 1; dx++) {
        const nx = x + dx, ny = y + dy, next = ny * width + nx;
        if (nx >= 0 && nx < width && ny >= 0 && ny < height && pixels[next] && !seen[next]) { seen[next] = 1; todo.push(next); }
      }
    }
    if (points.length >= 3) result.push(bounds(points, width));
  }
  return result;
}

export function normalize(mask) {
  const points = [];
  for (let i = 0; i < mask.pixels.length; i++) if (mask.pixels[i]) points.push(i);
  const out = new Uint8Array(32 * 32);
  if (!points.length) return out;
  const box = bounds(points, mask.width);
  // Preserve aspect ratio: stretching a horizontal and a vertical stroke into
  // identical squares produced false matches on the owner's DOS reference.
  const scale = 28 / Math.max(box.width, box.height);
  const w = Math.max(1, Math.round(box.width * scale)), h = Math.max(1, Math.round(box.height * scale));
  const left = Math.floor((32 - w) / 2), top = Math.floor((32 - h) / 2);
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) {
    out[(top + y) * 32 + left + x] = mask.pixels[(box.y + Math.min(box.height - 1, Math.floor((y + .5) * box.height / h))) * mask.width + box.x + Math.min(box.width - 1, Math.floor((x + .5) * box.width / w))];
  }
  return out;
}

function overlap(a, b) {
  let best = 0;
  const total = a.reduce((s, p) => s + p, 0) + b.reduce((s, p) => s + p, 0);
  if (!total) return 0;
  for (let dy = -1; dy <= 1; dy++) for (let dx = -1; dx <= 1; dx++) {
    let shared = 0;
    for (let y = 1; y < 31; y++) for (let x = 1; x < 31; x++) shared += a[y * 32 + x] && b[(y + dy) * 32 + x + dx] ? 1 : 0;
    best = Math.max(best, 2 * shared / total);
  }
  return best;
}

export function rankRune(mask, templates, ring) {
  const sample = normalize(mask);
  return templates.filter(t => t.ring === ring).map(t => ({ number: t.number, ring, score: overlap(sample, t.normalized) })).sort((a, b) => b.score - a.score);
}

function pathFor(row) {
  if (row.length < 3) return null;
  const heights = row.map(c => c.height);
  if (Math.max(...heights) > Math.min(...heights) * 1.8) return null;
  const kinds = row.map(c => c.width / c.height >= 2 ? 'dash' : c.width / c.height <= 1.5 ? 'dot' : '?');
  if (kinds.includes('?')) return null;
  // Also require evenly spaced centers for uniform runs. Mixed paths have
  // unequal gaps but must repeat dash-dot-dot, in any starting phase.
  const sorted = [...row].sort((a, b) => a.x - b.x);
  if (kinds.every(k => k === kinds[0])) {
    const gaps = sorted.slice(1).map((c, i) => c.x + c.width / 2 - sorted[i].x - sorted[i].width / 2);
    if (Math.max(...gaps) > Math.min(...gaps) * 1.35) return null;
    return kinds[0] === 'dot' ? 0 : 2;
  }
  const sequence = sorted.map(c => c.width / c.height >= 2 ? 'dash' : 'dot');
  if (sequence.length < 6) return null;
  return [0, 1, 2].some(phase => sequence.every((k, i) => k === ((i + phase) % 3 === 0 ? 'dash' : 'dot'))) ? 1 : null;
}

export function segmentFrame(image) {
  const mask = bitmap(image), { width, height } = mask;
  const parts = components(mask).filter(c => c.x > 2 && c.y > 2 && c.x + c.width < width - 2 && c.y + c.height < height - 2 && c.width < width * .18 && c.height < height * .24);
  const rows = [];
  for (const c of parts.filter(c => c.height <= height * .035 && c.width <= width * .06)) {
    const cy = c.y + c.height / 2;
    let row = rows.find(r => Math.abs(r[0].y + r[0].height / 2 - cy) <= Math.max(2, height * .006));
    if (!row) { row = []; rows.push(row); } row.push(c);
  }
  const paths = rows.map(row => ({ row, path: pathFor(row) })).filter(p => p.path !== null);
  const path = paths.length === 1 ? paths[0].path : null;
  const pathParts = new Set(paths.flatMap(p => p.row));
  const groups = parts.filter(p => !pathParts.has(p));
  // Join disconnected strokes/dots belonging to one glyph, after removing the
  // repeated path marks. Never join distant rune windows or the red border.
  const gap = Math.max(2, Math.round(width / 80));
  let merged = true;
  while (merged) {
    merged = false;
    outer: for (let i = 0; i < groups.length; i++) for (let j = i + 1; j < groups.length; j++) {
      const a = groups[i], b = groups[j];
      if (a.x > b.x + b.width + gap || b.x > a.x + a.width + gap || a.y > b.y + b.height + gap || b.y > a.y + a.height + gap) continue;
      const together = bounds([...a.points, ...b.points], width);
      if (together.width >= width * .18 || together.height >= height * .24) continue;
      groups[i] = together; groups.splice(j, 1); merged = true; break outer;
    }
  }
  const glyphs = groups.map(c => {
    const pixels = new Uint8Array(c.width * c.height);
    for (const i of c.points) pixels[(Math.floor(i / width) - c.y) * c.width + i % width - c.x] = 1;
    return { width: c.width, height: c.height, pixels };
  });
  return { glyphs, path };
}

export function recognizeWheel(image, templates) {
  const { glyphs, path } = segmentFrame(image);
  const abstain = reason => ({ confident: false, reason, path, glyphCount: glyphs.length });
  if (glyphs.length !== 2) return abstain('A complete pair of wheel runes was not found.');
  if (path === null) return abstain('The path is missing or ambiguous.');
  if (RINGS.some(r => new Set(templates.filter(t => t.ring === r).map(t => t.number)).size !== 36)) return abstain('The private rune templates are incomplete.');
  const assignments = [[0, 1], [1, 0]].map(([a, b]) => {
    const esp = rankRune(glyphs[a], templates, 'esp'), det = rankRune(glyphs[b], templates, 'det');
    return { esp, det, score: esp[0].score + det[0].score };
  }).sort((a, b) => b.score - a.score);
  const best = assignments[0];
  if (best.score - assignments[1].score < .12 || [best.esp, best.det].some(r => r[0].score < .90 || r[0].score - r[1].score < .08)) return abstain('The rune shapes do not match clearly enough.');
  const a = best.esp[0].number, b = best.det[0].number;
  return { confident: true, a, b, path, word: wheelWord(a, b, path), glyphCount: 2, signature: `${a}:${b}:${path}` };
}
