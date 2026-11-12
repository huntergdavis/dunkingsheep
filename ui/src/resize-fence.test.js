import test from 'node:test';
import assert from 'node:assert/strict';
import { fencedResize } from './resize-fence.js';

test('overlapping tabs cannot clear an uncertain resize marker', async () => {
  const data = new Map();
  const storage = { getItem: key => data.get(key) ?? null, setItem: (key, value) => data.set(key, value), removeItem: key => data.delete(key) };
  let held = false;
  const locks = { async request(_key, _opts, callback) { if (held) return callback(null); held = true; try { return await callback({}); } finally { held = false; } } };
  let finish;
  const pending = new Promise((_, reject) => { finish = reject; });
  const args = { locks, storage, key: '%7', send: () => pending };
  const first = fencedResize(args);
  assert.equal(await fencedResize({ ...args, send: () => { throw new Error('must not dispatch'); } }), 'busy');
  finish(new Error('network lost'));
  await assert.rejects(first, /network lost/);
  assert.equal(await fencedResize({ ...args, send: () => { throw new Error('must not dispatch'); } }), 'unconfirmed');
  assert.equal(storage.getItem('%7'), 'unconfirmed');
});

test('definite refusal clears marker and allows a later retry', async () => {
  const data = new Map();
  const storage = { getItem: key => data.get(key) ?? null, setItem: (key, value) => data.set(key, value), removeItem: key => data.delete(key) };
  const locks = { request: async (_key, _opts, callback) => callback({}) };
  const args = { locks, storage, key: '%7' };
  assert.equal(await fencedResize({ ...args, send: async () => ({ status: 409, ok: false }) }), 'refused');
  assert.equal(await fencedResize({ ...args, send: async () => ({ status: 200, ok: true }) }), 'applied');
  assert.equal(storage.getItem('%7'), null);
});
