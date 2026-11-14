import test from 'node:test';
import assert from 'node:assert/strict';
import { laneModel } from './lane-model.js';

test('a fresh sample shows the model with effort, age and source', () => {
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', effort: 'medium', source: 'launch command', age_s: 185, reason: null }),
    { known: true, text: 'opus-5.5 medium', detail: 'seen 3 min ago, launch command' },
  );
  assert.deepEqual(
    laneModel({ value: 'sol', effort: null, source: 'process environment', age_s: 12 }),
    { known: true, text: 'sol', detail: 'seen less than 1 min ago, process environment' },
  );
  assert.equal(laneModel({ value: 'sol', age_s: 900 }).detail, 'seen 15 min ago, source not recorded');
});

test('a stale sample is unknown even if a value is still attached', () => {
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', effort: 'high', source: 'launch command', age_s: 901 }),
    { known: false, text: 'unknown', detail: 'the last sample is older than 15 minutes' },
  );
});

test('a missing sample is unknown with the daemon reason', () => {
  assert.deepEqual(
    laneModel({ value: null, age_s: null, reason: 'no agent-state sample for this lane in the last 24 hours' }),
    { known: false, text: 'unknown', detail: 'no agent-state sample for this lane in the last 24 hours' },
  );
  assert.deepEqual(laneModel(null), { known: false, text: 'unknown', detail: 'no model sample' });
  assert.deepEqual(laneModel({ value: 'model-a' }), { known: false, text: 'unknown', detail: 'the sample has no age' });
});
