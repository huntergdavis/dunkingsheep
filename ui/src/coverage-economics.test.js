import { test } from 'node:test';
import { strict as assert } from 'node:assert';
import { coverageEconomics, milestoneScenarios } from './coverage-economics.js';

test('fixed pool allocation stays explicitly separate from actual C spend', () => {
  const snapshot = { function_progress: { strict_full_c: 82, ceiling: 815, observed_at: '2026-10-16T22:55:00Z' }, fixed_subscription_usd_per_calendar_hour: 0.5376 };
  const history = { goal_observations: [
    { goal_key: 'strict_fullc', observed_at: '2026-10-14T21:00:00Z', value: 80 },
    { goal_key: 'strict_fullc', observed_at: '2026-10-15T02:45:00Z', value: 82 },
  ] };
  const result = coverageEconomics(snapshot, history, { lanes: 6, creditsPerWeek: 10, now: '2026-10-16T23:00:00Z' });
  assert.equal(result.actualCSpendHourly, null);
  assert.equal(result.laneHourly, 0.0896);
  assert.equal(result.remaining, 733);
  assert.equal(result.credits24h, 0);
  assert.equal(result.accountingAgeMinutes, 5);
  assert.equal(result.accountingFresh, true);
  assert.equal(result.scenarioWeeks, 73.3);
  assert.equal(result.lastCreditAt, '2026-10-15T02:45:00Z');
});

test('missing comparable checkpoint does not imply a zero-credit day', () => {
  const result = coverageEconomics({ function_progress: { strict_full_c: 82, ceiling: 815 } },
    { goal_observations: [{ goal_key: 'strict_fullc', observed_at: '2026-10-16T22:00:00Z', value: 82 }] },
    { now: '2026-10-16T23:00:00Z' });
  assert.equal(result.credits24h, null);
  assert.equal(result.laneHourly, null);
  assert.equal(result.lastCreditAt, null);
});

test('stale accounting cannot imply zero new credit over the last day', () => {
  const result = coverageEconomics(
    { function_progress: { strict_full_c: 82, ceiling: 815, observed_at: '2026-10-16T20:00:00Z' } },
    { goal_observations: [
      { goal_key: 'strict_fullc', observed_at: '2026-10-14T22:00:00Z', value: 80 },
      { goal_key: 'strict_fullc', observed_at: '2026-10-15T22:00:00Z', value: 82 },
    ] }, { now: '2026-10-16T23:00:00Z' });
  assert.equal(result.accountingFresh, false);
  assert.equal(result.accountingAgeMinutes, 180);
  assert.equal(result.credits24h, null);
});

test('accounting and manually recorded credit must agree before a 24-hour delta appears', () => {
  const result = coverageEconomics({ function_progress: { strict_full_c: 83, ceiling: 815, observed_at: '2026-10-16T22:55:00Z' } },
    { goal_observations: [
      { goal_key: 'strict_fullc', observed_at: '2026-10-14T22:00:00Z', value: 80 },
      { goal_key: 'strict_fullc', observed_at: '2026-10-15T22:00:00Z', value: 82 },
    ] }, { now: '2026-10-16T23:00:00Z' });
  assert.equal(result.credits24h, null);
});

test('milestones count only additional strict C credit', () => {
  assert.deepEqual(milestoneScenarios(82, 815, 10).map(row => [row.target, row.additional, row.weeks]),
    [[204, 122, 12.2], [408, 326, 32.6], [815, 733, 73.3]]);
});
