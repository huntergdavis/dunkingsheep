import test from 'node:test';
import assert from 'node:assert/strict';
import { leaderLine } from './leader-line.js';

const ageText = seconds => seconds == null ? 'unknown' : seconds < 3600 ? Math.floor(seconds / 60) + 'm' : Math.floor(seconds / 3600) + 'h';

test('a source that is not ok renders unavailable, never a number', () => {
  assert.equal(leaderLine({ state: 'unavailable' }, ageText), 'unavailable leader-held');
  assert.equal(leaderLine(null, ageText), 'unavailable leader-held');
});

test('a null or missing count stays unknown without an empty-state claim', () => {
  // t118 P2: a full page truncates the aggregate (count None) and a legacy
  // payload misses the field; neither may render the measured-zero 'none'.
  assert.equal(leaderLine({ state: 'ok', leader_held: null, leader_held_oldest_age_s: null,
                            leader_held_ages_unknown: null }, ageText),
               'unknown leader-held');
  assert.equal(leaderLine({ state: 'ok' }, ageText), 'unknown leader-held');
});

test('a measured zero is the only state that renders none', () => {
  assert.equal(leaderLine({ state: 'ok', leader_held: 0, leader_held_oldest_age_s: null,
                            leader_held_ages_unknown: 0 }, ageText),
               '0 leader-held · none');
});

test('full coverage renders the definite oldest', () => {
  assert.equal(leaderLine({ state: 'ok', leader_held: 3, leader_held_oldest_age_s: 4900,
                            leader_held_ages_unknown: 0 }, ageText),
               '3 leader-held · oldest 1h');
  assert.equal(leaderLine({ state: 'ok', leader_held: 4, leader_held_oldest_age_s: null,
                            leader_held_ages_unknown: 0 }, ageText),
               '4 leader-held · oldest unknown');
});

test('partial coverage renders the oldest known age as a lower bound', () => {
  assert.equal(leaderLine({ state: 'ok', leader_held: 2, leader_held_oldest_age_s: 2200,
                            leader_held_ages_unknown: 1 }, ageText),
               '2 leader-held (oldest known ≥ 36m; 1 without timestamps)');
  assert.equal(leaderLine({ state: 'ok', leader_held: 2, leader_held_oldest_age_s: null,
                            leader_held_ages_unknown: 2 }, ageText),
               '2 leader-held (oldest known unknown; 2 without timestamps)');
});
