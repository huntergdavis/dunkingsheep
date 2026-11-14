import test from 'node:test';
import assert from 'node:assert/strict';
import { normaliseMailbox } from '../src/mailboxKey.js';

test('plain and trailing pane aliases share a mailbox without changing other labels', () => {
  for (const key of ['VP', 'VP (%1)', ' VP ( %12 ) ', 'VP (wR:p1)', 'VP (window-a:pB_2)']) {
    assert.equal(normaliseMailbox(key), 'VP', key);
  }
  for (const key of ['VP (second shift)', 'VP (p4)', 'VP (w9:p)', 'VP (%x)', 'VP (%1) history', '(%1)']) {
    assert.equal(normaliseMailbox(key), key, key);
  }
  assert.equal(normaliseMailbox('PM (par-a) (wW:p1)'), 'PM (par-a)');
  assert.notEqual(normaliseMailbox('Other (%1)'), normaliseMailbox('VP'));
});
