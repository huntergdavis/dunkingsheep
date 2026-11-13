// t4644: the quick-message dialog offers the panes the desk can already message and reports
// the server's own delivery outcome.
import test from 'node:test';
import assert from 'node:assert/strict';
import { inboxOutcomeText, quickMessageBlock, quickMessageTargets, sendOutcomeText } from './quick-message.js';

const fleet = { rows: [
  { pane_id: '%304', name: 'ds-fable-writer', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '%1', name: 'owner', project: null, send: 'owner_push', can_type: false },
  { pane_id: '%260', name: 'poolrad-writer', project: 'Poolrad', send: 'guarded', can_type: false },
  { pane_id: '%9', name: 'twin-a', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '%9', name: 'twin-b', project: 'dunkingsheep', send: 'guarded', can_type: true },
  { pane_id: '../x', name: 'bad' }, { name: 'no pane' }, null,
] };

test('targets are the uniquely identified fleet panes, ordered; typing ability does not matter for the inbox', () => {
  assert.deepEqual(quickMessageTargets(fleet), [
    { pane_id: '%304', project: 'dunkingsheep', name: 'ds-fable-writer', label: 'ds-fable-writer · %304' },
    { pane_id: '%260', project: 'Poolrad', name: 'poolrad-writer', label: 'poolrad-writer · %260' },
    { pane_id: '%1', project: 'Unassigned', name: 'owner', label: 'owner · %1' },
  ]);
  assert.deepEqual(quickMessageTargets(null), []);
  assert.deepEqual(quickMessageTargets({ state: 'unavailable' }), []);
});

test('send is offered only for a chosen pane with text, and not while sending', () => {
  const targets = quickMessageTargets(fleet);
  assert.equal(quickMessageBlock(targets, '%304', 'hello', false), null);
  assert.equal(quickMessageBlock(targets, '%260', 'hello', false), null);
  assert.equal(quickMessageBlock(targets, '%304', '   ', false), 'Write a message');
  assert.equal(quickMessageBlock(targets, '', 'hello', false), 'Choose a pane');
  assert.equal(quickMessageBlock(targets, '%9', 'hello', false), 'Choose a pane');
  assert.equal(quickMessageBlock(targets, '%304', 'hello', true), 'Sending…');
});

test('the dialog reports where the daemon stored the message, its id and whether the lane was told', () => {
  const target = { pane_id: '%304', name: 'ds-fable-writer' };
  assert.equal(inboxOutcomeText({ pane: '%304', message_id: 'i77', mailbox: 'ds-fable-writer', notified: true }, target), 'Stored in ds-fable-writer inbox · i77 · lane notified');
  assert.equal(inboxOutcomeText({ pane: '%304', message_id: 'i78', notified: false }, target), 'Stored in ds-fable-writer inbox · i78 · lane not notified yet');
  assert.equal(inboxOutcomeText({ pane: '%304', message_id: 'i79', mailbox: 'pm', notified: true, redirected: { from: 'vp', to: 'pm' } }, target), 'Stored in pm inbox · i79 · lane notified · redirected by the daemon');
  assert.equal(inboxOutcomeText({}, undefined), 'Stored in the lane inbox · no id · lane not notified yet');
});

test('the Terminals composer outcome text is the server\'s, with stored and unconfirmed never shown as delivered', () => {
  assert.equal(sendOutcomeText({ pane: '%304', outcome: 'stored', note: 'in the inbox', detail: 'i123' }), '%304 [stored]: in the inbox (i123)');
  assert.equal(sendOutcomeText({ pane: '%304', outcome: 'unconfirmed', note: 'typed, not confirmed', detail: 'x', unconfirmed: true }), '%304 [unconfirmed]: typed, not confirmed');
  assert.equal(sendOutcomeText({ pane: '%304' }), '%304 [unknown]: accepted');
});
