// How the web terminal fits its box (t5007).
//
// The terminal shows a tmux pane that has its own size; tmux draws for that size. So the
// terminal always has exactly the pane's columns and rows (the server sends them), and it is
// the FONT that is fitted to the box: as large as fits both ways, within readable limits. When
// even the smallest font is too wide or too tall, the box scrolls. The terminal never takes a
// size of its own: a size that differs from the pane's is what wraps and jumps.
export const FONT_MAX = 15;
export const FONT_MIN = 8;

// A control message from the stream is a text frame holding JSON; pane output is binary.
// Returns {cols, rows} for a valid size message, null for anything else.
export function sizeMessage(text) {
  if (typeof text !== 'string') return null;
  let value;
  try { value = JSON.parse(text); } catch { return null; }
  if (!value || value.type !== 'size') return null;
  const whole = n => Number.isInteger(n) && n >= 1 && n <= 2000;
  return whole(value.cols) && whole(value.rows) ? { cols: value.cols, rows: value.rows } : null;
}

// The font size, in half pixels, at which `cols` x `rows` cells fit a box of `width` x
// `height` CSS pixels. `cellWidth` and `cellHeight` are the size of one cell per pixel of font
// size (measured from the real font). `fits` is false when the smallest font still overflows.
export function fitFont({ width, height, cols, rows, cellWidth, cellHeight, min = FONT_MIN, max = FONT_MAX }) {
  const ok = [width, height, cols, rows, cellWidth, cellHeight].every(n => Number.isFinite(n) && n > 0);
  if (!ok) return { fontSize: max, fits: false };
  const byWidth = width / (cols * cellWidth);
  const byHeight = height / (rows * cellHeight);
  const exact = Math.min(byWidth, byHeight);
  const fontSize = Math.max(min, Math.min(max, Math.floor(exact * 2) / 2));
  return { fontSize, fits: exact >= min };
}

// The columns and rows that fill the box at the largest font: what to ask the server for when
// the pane may be resized to the box.
export function wantedSize({ width, height, cellWidth, cellHeight, fontSize = FONT_MAX }) {
  const ok = [width, height, cellWidth, cellHeight, fontSize].every(n => Number.isFinite(n) && n > 0);
  if (!ok) return null;
  const cols = Math.floor(width / (cellWidth * fontSize));
  const rows = Math.floor(height / (cellHeight * fontSize));
  return cols >= 20 && rows >= 5 ? { cols: Math.min(cols, 500), rows: Math.min(rows, 200) } : null;
}
