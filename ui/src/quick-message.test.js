// t4644: the quick-message dialog offers the panes the desk can already message and reports
// the server's own delivery outcome.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { inboxOutcomeText, quickMessageBlock, quickMessageRoster, quickMessageTargets, sendOutcomeText } from './quick-message.js';

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

test('the recipient list says whether it is loading, failed with the reason, empty or ready', () => {
  assert.deepEqual(quickMessageRoster({ status: 'loading' }, null), { kind: 'loading', text: 'Loading recipients…', retry: false });
  assert.deepEqual(quickMessageRoster(undefined, null), { kind: 'loading', text: 'Loading recipients…', retry: false });
  assert.deepEqual(quickMessageRoster({ status: 'failed', error: 'Request failed (502)' }, null),
    { kind: 'failed', text: 'Recipients could not be loaded: Request failed (502)', retry: true });
  assert.deepEqual(quickMessageRoster({ status: 'ready' }, { state: 'unavailable', error: 'pane roster could not be read: timeout', rows: [] }),
    { kind: 'failed', text: 'Recipients could not be loaded: pane roster could not be read: timeout', retry: true });
  assert.deepEqual(quickMessageRoster({ status: 'ready' }, { state: 'ok', rows: [] }),
    { kind: 'empty', text: 'No live lanes are listed right now.', retry: true });
  assert.deepEqual(quickMessageRoster({ status: 'ready' }, fleet), { kind: 'ready', text: '3 recipients', retry: false });
});

test('a lane the daemon lists without a detected agent is a recipient like any other', () => {
  // The row shape /api/fleet returns for a Claude lane: agent is the empty string.
  const rows = [{ pane_id: '%308', name: 'ds-fable-writer-fresh', project: 'dunkingsheep', agent: '', alive: true, send: 'guarded' }];
  assert.deepEqual(quickMessageTargets({ state: 'ok', rows }),
    [{ pane_id: '%308', project: 'dunkingsheep', name: 'ds-fable-writer-fresh', label: 'ds-fable-writer-fresh · %308' }]);
});

test('Send and Cancel are on their own row after the text box, inside the dialog form', () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const source = readFileSync(path.join(here, 'main.jsx'), 'utf8');
  const start = source.indexOf('function QuickMessage(');
  const dialog = source.slice(start, source.indexOf('\nfunction ', start + 1));
  const at = needle => { const index = dialog.indexOf(needle); assert.notEqual(index, -1, `missing ${needle}`); return index; };
  const order = [
    at('<form onSubmit={send}>'),
    at('id="quick-message-pane"'),
    at('<textarea id="quick-message-text"'),
    at('<div className="quick-message-actions">'),
    at('type="submit"'),
    at('<button type="button" onClick={onClose}>Cancel</button>'),
    at('</div>\n      {sent &&'),
    at('</form>'),
  ];
  assert.deepEqual(order, [...order].sort((a, b) => a - b), 'pane, text box, then the action row with Send before Cancel');
  assert.match(dialog, /className="settings-modal quick-message"/);
  // The row and the dialog are laid out by classes, with no fixed pixel width that could overflow.
  const css = readFileSync(path.join(here, 'dash.css'), 'utf8');
  const rules = css.split('\n').filter(line => line.includes('.quick-message'));
  assert.ok(rules.some(line => line.includes('white-space: normal')), 'the nav nowrap is undone for the dialog');
  assert.ok(rules.some(line => line.includes('.quick-message-actions') && line.includes('flex-wrap: wrap')), 'the action row may wrap');
  assert.ok(!rules.some(line => /(^|[^-])width: \d+px/.test(line)), 'no fixed widths');
});
