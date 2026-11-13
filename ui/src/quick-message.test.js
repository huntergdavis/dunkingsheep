// t4644: the quick-message dialog offers the panes the desk can already message and reports
// the server's own delivery outcome.
import test from 'node:test';
import assert from 'node:assert/strict';
import { laneBlocked, quickMessageBlock, quickMessageTargets, sendOutcomeText } from './quick-message.js';

const fleet = { rows: [
  { pane_id: '%304', name: 'ds-fable-writer', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '%1', name: 'owner', project: null, send: 'owner_push', can_type: false },
  { pane_id: '%260', name: 'poolrad-writer', project: 'Poolrad', send: 'guarded', can_type: false },
  { pane_id: '%9', name: 'twin-a', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '%9', name: 'twin-b', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '../x', name: 'bad' }, { name: 'no pane' }, null,
] };

test('targets are the uniquely identified fleet panes, ordered, with the composer block rule', () => {
  assert.deepEqual(quickMessageTargets(fleet), [
    { pane_id: '%304', project: 'dunkingsheep', label: 'ds-fable-writer · %304', blocked: false },
    { pane_id: '%260', project: 'Poolrad', label: 'poolrad-writer · %260', blocked: true },
    { pane_id: '%1', project: 'Unassigned', label: 'owner · %1', blocked: false },
  ]);
  assert.deepEqual(quickMessageTargets(null), []);
  assert.deepEqual(quickMessageTargets({ state: 'unavailable' }), []);
  assert.equal(laneBlocked({ send: 'owner_push', can_type: false }), false);
  assert.equal(laneBlocked(undefined), false);
});

test('send is offered only for a chosen, messageable pane with text, and not while sending', () => {
  const targets = quickMessageTargets(fleet);
  assert.equal(quickMessageBlock(targets, '%304', 'hello', false), null);
  assert.equal(quickMessageBlock(targets, '%304', '   ', false), 'Write a message');
  assert.equal(quickMessageBlock(targets, '', 'hello', false), 'Choose a pane');
  assert.equal(quickMessageBlock(targets, '%9', 'hello', false), 'Choose a pane');
  assert.equal(quickMessageBlock(targets, '%260', 'hello', false), 'Messaging is unavailable for this agent');
  assert.equal(quickMessageBlock(targets, '%304', 'hello', true), 'Sending…');
});

test('the outcome text is the server\'s, with stored and unconfirmed never shown as delivered', () => {
  assert.equal(sendOutcomeText({ pane: '%304', outcome: 'stored', note: 'in the inbox', detail: 'i123' }), '%304 [stored]: in the inbox (i123)');
  assert.equal(sendOutcomeText({ pane: '%304', outcome: 'unconfirmed', note: 'typed, not confirmed', detail: 'x', unconfirmed: true }), '%304 [unconfirmed]: typed, not confirmed');
  assert.equal(sendOutcomeText({ pane: '%304' }), '%304 [unknown]: accepted');
});
