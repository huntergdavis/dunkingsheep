import test from 'node:test';
import assert from 'node:assert/strict';
import { laneModel } from './lane-model.js';

test('a launch flag reads "launched as" with the launch time, however long ago, and never "seen"', () => {
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', effort: 'medium', source: 'launch flag', observed: false, age_s: 3 * 3600 + 5 }),
    { known: true, text: 'launched as opus-5.5 medium', detail: 'launch flag, launched 3 h ago' },
  );
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', effort: null, source: 'launch flag', observed: false, age_s: null }),
    { known: true, text: 'launched as opus-5.5', detail: 'launch flag, launch time not recorded' },
  );
  assert.equal(laneModel({ value: 'm', source: 'launch flag', age_s: 4 * 86400 }).detail, 'launch flag, launched 4 d ago');
});

test('a process environment value is a launch value with its read time, unknown once the read is stale', () => {
  assert.deepEqual(
    laneModel({ value: 'sol', effort: 'high', source: 'process environment', observed: false, age_s: 185 }),
    { known: true, text: 'launched as sol high', detail: 'process environment, read 3 min ago' },
  );
  assert.deepEqual(
    laneModel({ value: 'sol', source: 'process environment', observed: false, age_s: 901 }),
    { known: false, text: 'unknown', detail: 'the last environment reading is older than 15 minutes' },
  );
});

test('"seen" is only for a value read from the running lane, and only while fresh', () => {
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', effort: 'medium', source: 'pane footer', observed: true, age_s: 12 }),
    { known: true, text: 'opus-5.5 medium', detail: 'seen less than 1 min ago, pane footer' },
  );
  assert.deepEqual(
    laneModel({ value: 'opus-5.5', source: 'pane footer', observed: true, age_s: 901 }),
    { known: false, text: 'unknown', detail: 'the last reading is older than 15 minutes' },
  );
  assert.equal(laneModel({ value: 'opus-5.5', source: 'pane footer', observed: true }).known, false);
});

test('a missing value, a value with no source, or an unrecognised source is unknown with the reason', () => {
  assert.deepEqual(
    laneModel({ value: null, age_s: null, reason: 'no model flag on the launch command' }),
    { known: false, text: 'unknown', detail: 'no model flag on the launch command' },
  );
  assert.deepEqual(laneModel(null), { known: false, text: 'unknown', detail: 'no model information' });
  assert.deepEqual(laneModel({ value: 'model-a', age_s: 5 }), { known: false, text: 'unknown', detail: 'the model has no recorded source' });
  assert.deepEqual(
    laneModel({ value: 'model-a', source: 'launch command', age_s: 5 }),
    { known: false, text: 'unknown', detail: 'unrecognised model source: launch command' },
  );
});

test('a declared model shows who declared it and when, and is not "seen" or "launched as"', () => {
  assert.deepEqual(
    laneModel({ value: 'gpt-luna', effort: 'low', source: 'declared', declared_by: '%12', stale: false, observed: false, age_s: 600 }),
    { known: true, text: 'gpt-luna low', detail: 'declared by %12 10 min ago' },
  );
  assert.deepEqual(
    laneModel({ value: 'gpt-luna', effort: null, source: 'declared', declared_by: 'owner', age_s: null }),
    { known: true, text: 'gpt-luna', detail: 'declared by owner, time not recorded' },
  );
});

test('a stale record shows "stale" with the reason and never its value', () => {
  assert.deepEqual(
    laneModel({ value: null, stale: true, reason: 'the model was declared for an earlier process in this pane', age_s: 600 }),
    { known: false, text: 'stale', detail: 'the model was declared for an earlier process in this pane' },
  );
  // Even if a value were attached, a stale cell does not show it.
  assert.deepEqual(
    laneModel({ value: 'gpt-luna', source: 'declared', stale: true }),
    { known: false, text: 'stale', detail: 'the record is for an earlier process' },
  );
});
