import test from 'node:test';
import assert from 'node:assert/strict';
import { formatTimestamp, timestampIso } from './timestamps.js';

test('server zone formats winter, summer and both DST transitions', () => {
  for (const [instant, expected] of [
    ['2026-01-15T20:05:00Z', '12:05 PM PST'],
    ['2026-07-15T19:05:00Z', '12:05 PM PDT'],
    ['2026-03-08T09:59:00Z', '01:59 AM PST'],
    ['2026-03-08T10:00:00Z', '03:00 AM PDT'],
    ['2026-11-01T08:30:00Z', '01:30 AM PDT'],
    ['2026-11-01T09:30:00Z', '01:30 AM PST'],
  ]) assert.ok(formatTimestamp(instant, 'America/Los_Angeles').endsWith(expected));
  assert.equal(formatTimestamp('2026-01-01T01:05:00Z', 'America/Los_Angeles'), 'Wed, Dec 31, 2025, 05:05 PM PST');
  assert.ok(formatTimestamp('2026-01-15T20:05:00Z', 'Asia/Tokyo').endsWith('05:05 AM GMT+9'));
});

test('preserves instants and date-only provenance without guessing a client zone', () => {
  assert.equal(timestampIso('2026-01-15T21:05:00+01:00'), '2026-01-15T20:05:00.000Z');
  const seconds = Date.parse('2026-01-15T20:05:00Z') / 1000;
  assert.equal(timestampIso(seconds, true), '2026-01-15T20:05:00.000Z');
  assert.equal(formatTimestamp(seconds, 'America/Los_Angeles', true), 'Thu, Jan 15, 2026, 12:05 PM PST');
  assert.equal(formatTimestamp('2026-11-01', null), '2026-11-01');
  for (const value of [null, '', 'bad', '2026-01-15T12:00:00', Infinity]) {
    assert.equal(timestampIso(value), null);
    assert.equal(formatTimestamp(value, 'America/Los_Angeles'), 'Time unavailable');
  }
  assert.ok(formatTimestamp(seconds, null, true).endsWith('08:05 PM UTC'));
  assert.ok(formatTimestamp(seconds, 'not/a-zone', true).endsWith('08:05 PM UTC'));
});
