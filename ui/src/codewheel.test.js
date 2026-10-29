import test from 'node:test';
import assert from 'node:assert/strict';
import { normalize, recognizeWheel, segmentFrame, wheelWord } from './codewheel.js';

function shape(kind) {
  const width = 20, height = 24, pixels = new Uint8Array(width * height);
  for (let y = 0; y < height; y++) for (let x = 0; x < width; x++) {
    pixels[y * width + x] = (kind === 'L' ? x < 3 || y > 20 : kind === 'T' ? y < 3 || (x >= 8 && x <= 10) : true) ? 1 : 0;
  }
  return { width, height, pixels };
}
const templates = ['esp', 'det'].flatMap(ring => Array.from({ length: 36 }, (_, i) => ({
  ring, number: i + 1, normalized: normalize(shape(ring === 'esp' && i === 20 ? 'L' : ring === 'det' && i === 13 ? 'T' : 'box')),
})));
function frame({ path = 2, glyphs = ['L', 'T'], scale = 1, secondPath = false } = {}) {
  const width = 640 * scale, height = 400 * scale, data = new Uint8ClampedArray(width * height * 4);
  for (let i = 3; i < data.length; i += 4) data[i] = 255;
  function pixel(x, y) {
    for (let dy = 0; dy < scale; dy++) for (let dx = 0; dx < scale; dx++) {
      const i = ((y * scale + dy) * width + x * scale + dx) * 4;
      data.set([255, 70, 70, 255], i);
    }
  }
  glyphs.forEach((kind, index) => {
    const mask = shape(kind);
    for (let y = 0; y < mask.height; y++) for (let x = 0; x < mask.width; x++) if (mask.pixels[y * mask.width + x]) pixel(150 + 200 * index + x, 80 + y);
  });
  const marks = path === 1 ? [9, 3, 3, 9, 3, 3] : Array(5).fill(path === 0 ? 3 : 9);
  for (const top of secondPath ? [200, 260] : [200]) if (path !== null) {
    marks.forEach((w, index) => { for (let y = 0; y < 3; y++) for (let x = 0; x < w; x++) pixel(180 + index * 20 + x, top + y); });
  }
  return { width, height, data };
}

test('lookup preserves the existing formula and requires explicit valid inputs', () => {
  assert.equal(wheelWord(21, 14, 2), 'WYVERN');
  assert.equal(wheelWord(30, 20, 1), 'ZOMBIE');
  assert.equal(wheelWord(36, 36, 0), 'SOMAS');
  for (const args of [[null, 1, 0], [1, 1, null], [0, 1, 0], [37, 1, 0], [1.5, 1, 0], [1, 1, 3]]) assert.equal(wheelWord(...args), null);
});
test('aspect normalization never makes a horizontal stroke equal to a vertical stroke', () => {
  assert.notDeepEqual(normalize({ width: 20, height: 2, pixels: new Uint8Array(40).fill(1) }), normalize({ width: 2, height: 20, pixels: new Uint8Array(40).fill(1) }));
});
test('recognizes a clear pair at either display scale and with the rings reversed', () => {
  for (const scale of [1, 2]) for (const glyphs of [['L', 'T'], ['T', 'L']]) {
    const result = recognizeWheel(frame({ scale, glyphs }), templates);
    assert.equal(result.confident, true, JSON.stringify(result));
    assert.equal(result.word, 'WYVERN');
    assert.deepEqual([result.a, result.b, result.path], [21, 14, 2]);
  }
});
test('classifies dots, dash-two-dots and dashes; ambiguous paths abstain', () => {
  for (const path of [0, 1, 2]) assert.equal(segmentFrame(frame({ path })).path, path);
  for (const picture of [frame({ path: null }), frame({ secondPath: true })]) assert.equal(recognizeWheel(picture, templates).confident, false);
});
test('no word is returned for missing, extra, unknown or ambiguous glyphs', () => {
  for (const glyphs of [[], ['L'], ['L', 'T', 'L'], ['box', 'T']]) {
    const result = recognizeWheel(frame({ glyphs }), templates);
    assert.equal(result.confident, false); assert.equal(result.word, undefined);
  }
  const duplicate = templates.map(t => t.ring === 'esp' && t.number === 22 ? { ...t, normalized: normalize(shape('L')) } : t);
  assert.equal(recognizeWheel(frame(), duplicate).confident, false);
  assert.equal(recognizeWheel(frame(), templates.slice(1)).confident, false);
});
