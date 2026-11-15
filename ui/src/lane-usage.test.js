import test from 'node:test';
import assert from 'node:assert/strict';
import { harnessSourceText, laneUsage, providerFor } from './lane-usage.js';

const NOW = Date.parse('2026-11-15T12:00:00Z');
const budget = { providers: [
  { provider: 'claude', state: 'ok', meaning: 'percent_used', measured_at: '2026-11-15T11:55:00Z',
    windows: [{ name: 'five_hour', percent_used: 42.4, resets_at: '2026-11-15T14:00:00Z' }, { name: 'weekly_all', percent_used: 10 }],
    extra_usage: { used_credits: 1, monthly_limit: 4 } },
  { provider: 'codex', state: 'stale', meaning: 'percent_used', measured_at: '2026-11-15T09:00:00Z',
    reason: 'newest snapshot is 180 min old', windows: [{ name: 'primary', percent_used: 80 }] },
  { provider: 'pi', state: 'ok', meaning: 'local_session_estimate', measured_at: '2026-11-15T11:59:30Z',
    windows: [], local_estimate: { session_cost_usd: 1.234 } },
  { provider: 'opencode-go', state: 'unavailable', meaning: 'percent_used', measured_at: null,
    reason: 'no provider reading', windows: [] },
] };

test('a lane on an account with a reading gets that account\'s bar, labelled as shared', () => {
  const usage = laneUsage({ harness: 'claude' }, budget, NOW);
  assert.equal(usage.kind, 'bar');
  assert.equal(usage.percent, 42.4);
  assert.equal(usage.account, 'shared: claude account');
  assert.equal(usage.text, '42% of five_hour used');
  assert.equal(usage.detail, 'measured 5 min ago, resets 2026-11-15T14:00:00Z');
  assert.equal(usage.windows.length, 2);
  // Two lanes on the same account get the same numbers: it is not per-lane usage.
  assert.deepEqual(laneUsage({ harness: 'Claude' }, budget, NOW), usage);
});

test('a stale reading is said to be stale, with its age and reason, and draws no bar', () => {
  const usage = laneUsage({ harness: 'codex' }, budget, NOW);
  assert.equal(usage.kind, 'stale');
  assert.equal(usage.text, 'usage reading is stale');
  assert.equal(usage.detail, 'measured 3 h ago: newest snapshot is 180 min old');
  assert.equal(usage.percent, undefined);
});

test('a local estimate is labelled an estimate and never drawn as a limit', () => {
  const usage = laneUsage({ harness: 'pi' }, budget, NOW);
  assert.equal(usage.kind, 'estimate');
  assert.equal(usage.text, 'estimate: local session cost $1.23');
  assert.match(usage.detail, /not an account limit/);
  assert.equal(usage.percent, undefined);
});

test('unknown providers, missing readings and missing data are unavailable with the reason', () => {
  const none = laneUsage({}, budget, NOW);
  assert.deepEqual([none.kind, none.detail], ['unavailable', 'the lane\'s provider is not known']);
  assert.equal(laneUsage({ harness: 'gemini' }, budget, NOW).detail, 'no usage reading for gemini');
  const opencode = laneUsage({ harness: 'opencode' }, budget, NOW);
  assert.equal(opencode.account, 'shared: opencode-go account');
  assert.match(opencode.detail, /no provider reading/);
  assert.equal(laneUsage({ harness: 'claude' }, null, NOW).detail, 'usage readings are unavailable');
  const noWindow = { providers: [{ provider: 'claude', state: 'ok', meaning: 'percent_used', measured_at: null, windows: [] }] };
  assert.equal(laneUsage({ harness: 'claude' }, noWindow, NOW).detail, 'measurement time unknown: the reading has no usage window');
  assert.equal(providerFor({ harness: 'opencode' }, budget).name, 'opencode-go');
});

test('a percentage outside 0..100 is clamped for the bar', () => {
  const over = { providers: [{ provider: 'claude', state: 'ok', measured_at: '2026-11-15T11:59:00Z', windows: [{ name: 'w', percent_used: 140 }] }] };
  assert.equal(laneUsage({ harness: 'claude' }, over, NOW).percent, 100);
});

test('t5043: a stale local estimate says stale, an errored provider is unavailable, and the harness source is named', () => {
  const stalePi = { providers: [{ provider: 'pi', state: 'unknown', meaning: 'local_session_estimate', measured_at: '2026-11-13T21:00:00Z', windows: [], local_estimate: { session_cost_usd: 0.5 } }] };
  const usage = laneUsage({ harness: 'pi' }, stalePi, NOW);
  assert.equal(usage.kind, 'estimate');
  assert.equal(usage.text, 'estimate, stale: local session cost $0.50');
  assert.equal(usage.detail, 'measured 39 h ago; a local estimate, not an account limit');
  const errored = { providers: [{ provider: 'opencode', state: 'error', meaning: 'unknown', measured_at: '2026-11-15T11:59:00Z', reason: 'no provider reading', windows: [] }] };
  const opencode = laneUsage({ harness: 'opencode' }, errored, NOW);
  assert.equal(opencode.kind, 'unavailable');
  assert.match(opencode.detail, /no provider reading/);
  assert.equal(harnessSourceText({ harness_source: 'launch' }), 'harness from the launch command');
  assert.equal(harnessSourceText({ harness_source: 'registry' }), 'harness from the pane registry');
  assert.equal(harnessSourceText({}), '');
});
