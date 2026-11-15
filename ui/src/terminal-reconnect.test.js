import test from 'node:test';
import assert from 'node:assert/strict';
import { terminalReconnect } from './terminal-reconnect.js';

function fixture() {
  let time = 0;
  const reconnect = terminalReconnect(() => time);
  return { reconnect, connection({ snapshot = false, up = 10 } = {}) {
    reconnect.opened();
    if (snapshot) reconnect.received();
    time += up;
    return reconnect.closed();
  } };
}

test('repeated snapshot-then-close connections back off instead of storming', () => {
  const f = fixture();
  assert.equal(f.reconnect.retrying, false);
  assert.deepEqual(Array.from({ length: 8 }, () => f.connection({ snapshot: true })),
    [0, 1000, 2000, 4000, 8000, 15000, 15000, 15000]);
  assert.equal(f.reconnect.retrying, true);
});

test('a long-lived connection with screen data permits one prompt recovery', () => {
  const f = fixture();
  assert.equal(f.connection(), 0);
  assert.equal(f.connection({ snapshot: true }), 1000);
  assert.equal(f.connection({ snapshot: true, up: 2000 }), 0);
  assert.equal(f.connection({ snapshot: true }), 1000);
  assert.equal(f.connection({ snapshot: true }), 2000);
});

test('handshakes without a screen never reset backoff, even after two seconds', () => {
  const f = fixture();
  assert.equal(f.connection({ up: 3000 }), 0);
  assert.equal(f.connection({ up: 3000 }), 1000);
  assert.equal(f.connection({ up: 3000 }), 2000);
});

test('a screen followed by failure before the stability window retains backoff', () => {
  const f = fixture();
  assert.equal(f.connection({ snapshot: true }), 0);
  assert.equal(f.connection({ snapshot: true, up: 1999 }), 1000);
  assert.equal(f.connection({ snapshot: true, up: 1999 }), 2000);
});
