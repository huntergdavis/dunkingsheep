import test from 'node:test';
import assert from 'node:assert/strict';
import { taskPage, taskQuery, taskAge, metricNumber } from './tasks-model.js';
const filters = { view: 'active', status: '', priority: '', assignee: '' };
const row = { id: 't1', title: 'Fixture', status: 'blocked', priority: 'high', assignees: ['%4'], created_at: 10, block_reason: 'waiting' };
const page = () => ({ tasks: [row], scope: 'global', view: 'active', total: 1, next_cursor: null, checked_at: 100, legacy_limited: false });
test('task pages refuse mismatched, duplicate or malformed upstream data', () => {
  assert.equal(taskPage(page(), filters).tasks.length, 1);
  for (const patch of [{ view: 'closed' }, { scope: '%99' }, { tasks: [row, row], total: 2 }, { next_cursor: 'next', tasks: [] }, { total: 0 }, { tasks: [{ ...row, status: 'done' }] }, { legacy_limited: true }]) {
    assert.throws(() => taskPage({ ...page(), ...patch }, filters));
  }
  for (const applied of [{ ...filters, status: 'open' }, { ...filters, priority: 'low' }, { ...filters, assignee: '%99' }]) assert.throws(() => taskPage(page(), applied));
  assert.doesNotThrow(() => taskPage({ ...page(), legacy_limited: true, total: null }, filters));
});
test('filters and opaque cursors are encoded without becoming URL parameters', () => {
  const query = new URLSearchParams(taskQuery({ ...filters, assignee: '%4&scope=other' }, '{"after":"t1"}'));
  assert.equal(query.get('assignee'), '%4&scope=other'); assert.equal(query.get('scope'), null);
  assert.equal(query.get('cursor'), '{"after":"t1"}');
});
test('ages use the daemon snapshot and unavailable measurements stay unknown', () => {
  assert.equal(taskAge(0, 86400), '1d'); assert.equal(taskAge(100, 99), 'Unknown'); assert.equal(taskAge(null, 100), 'Unknown');
  assert.equal(metricNumber(null), 'Unknown'); assert.equal(metricNumber(-1), 'Unknown'); assert.equal(metricNumber(0), '0');
});
