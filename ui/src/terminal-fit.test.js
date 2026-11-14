import test from 'node:test';
import assert from 'node:assert/strict';
import { fitFont, sizeMessage, wantedSize, FONT_MAX, FONT_MIN } from './terminal-fit.js';

const cell = { cellWidth: 0.6, cellHeight: 1.2 };

test('a size message is JSON text with whole, bounded columns and rows', () => {
  assert.deepEqual(sizeMessage('{"type":"size","cols":132,"rows":40}'), { cols: 132, rows: 40 });
  for (const bad of ['{"type":"size","cols":0,"rows":40}', '{"type":"size","cols":80.5,"rows":24}',
    '{"type":"size","cols":"80","rows":24}', '{"type":"size","cols":80}', '{"type":"other","cols":80,"rows":24}',
    '{"type":"size","cols":99999,"rows":24}', 'plain terminal text', '', 'null', '[1]']) {
    assert.equal(sizeMessage(bad), null, bad);
  }
  assert.equal(sizeMessage(new Uint8Array([123])), null);
});

test('the font is the largest that fits both ways, capped for a roomy box', () => {
  // 80 columns in 1200 px would allow 25 px; the cap keeps it readable, not huge.
  assert.deepEqual(fitFont({ width: 1200, height: 600, cols: 80, rows: 24, ...cell }), { fontSize: FONT_MAX, fits: true });
  // 132 columns in 740 px: 740 / (132 * 0.6) = 9.34 -> 9.0 (half pixels, rounded down).
  assert.deepEqual(fitFont({ width: 740, height: 600, cols: 132, rows: 40, ...cell }), { fontSize: 9, fits: true });
  // Height is the limit here: 300 / (40 * 1.2) = 6.25 < min.
  assert.deepEqual(fitFont({ width: 2000, height: 300, cols: 80, rows: 40, ...cell }), { fontSize: FONT_MIN, fits: false });
});

test('a phone-width box with a wide pane keeps the smallest font and reports that it scrolls', () => {
  const phone = fitFont({ width: 350, height: 500, cols: 200, rows: 50, ...cell });
  assert.deepEqual(phone, { fontSize: FONT_MIN, fits: false });
  // A narrow pane fits a phone: 350 / (60 * 0.6) = 9.72 -> 9.5.
  assert.deepEqual(fitFont({ width: 350, height: 500, cols: 60, rows: 20, ...cell }), { fontSize: 9.5, fits: true });
});

test('the fitted cells never exceed the box', () => {
  for (const [width, cols] of [[390, 80], [768, 132], [1280, 200], [333, 37], [1000, 99]]) {
    const { fontSize, fits } = fitFont({ width, height: 10000, cols, rows: 24, ...cell });
    if (fits) assert.ok(cols * cell.cellWidth * fontSize <= width, `${cols} columns in ${width}px at ${fontSize}px`);
  }
});

test('nonsense measurements never produce a size', () => {
  assert.deepEqual(fitFont({ width: 0, height: 600, cols: 80, rows: 24, ...cell }), { fontSize: FONT_MAX, fits: false });
  assert.deepEqual(fitFont({ width: 800, height: 600, cols: NaN, rows: 24, ...cell }), { fontSize: FONT_MAX, fits: false });
  assert.equal(wantedSize({ width: 0, height: 600, ...cell }), null);
});

test('the wanted size fills the box at the largest font, and tiny boxes ask for nothing', () => {
  assert.deepEqual(wantedSize({ width: 900, height: 540, ...cell }), { cols: 100, rows: 30 });
  assert.equal(wantedSize({ width: 150, height: 540, ...cell }), null);
  assert.equal(wantedSize({ width: 900, height: 60, ...cell }), null);
});
