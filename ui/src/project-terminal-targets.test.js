import test from 'node:test';
import assert from 'node:assert/strict';
import { terminalTargetForPane } from './project-terminal-targets.js';
test('only an unambiguous server-authorized pane is selectable', () => {
  const lane = { pane_id: '%42', name: 'worker' };
  assert.equal(terminalTargetForPane([lane], '%42'), lane);
  assert.equal(terminalTargetForPane([lane], '%43'), null);
  assert.equal(terminalTargetForPane([lane, lane], '%42'), null);
  assert.equal(terminalTargetForPane([lane], '../worker'), null);
  assert.equal(terminalTargetForPane(null, '%42'), null);
});

import { terminalActionForLane, terminalListReason } from './project-terminal-targets.js';
// t4643: every lane row shows a working terminal action or a plain reason.
test('a lane row gets its authorized terminal or the reason there is none', () => {
  const worker = { pane_id: '%42', name: 'worker' };
  const owner = { pane_id: '%1', name: '__owner_terminal__' };
  const ready = { state: 'ready', targets: [worker, owner] };
  assert.deepEqual(terminalActionForLane(ready, '%42'), { target: worker, label: 'Open terminal', reason: null });
  assert.deepEqual(terminalActionForLane(ready, '%1'), { target: { ...owner, label: 'Owner' }, label: 'Open owner terminal', reason: null });
  const none = terminalActionForLane(ready, '%133');
  assert.equal(none.target, null);
  assert.match(none.reason, /^No terminal: not a named, registered/);
  assert.equal(terminalActionForLane({ state: 'ready', targets: [worker, worker] }, '%42').target, null);
  assert.equal(terminalActionForLane({ state: 'loading' }, '%42').reason, 'Checking terminal access…');
  assert.equal(terminalActionForLane(null, '%42').target, null);
});
test('a missing terminal list says why for every lane and offers no target', () => {
  for (const [status, text] of [[404, 'Terminals are switched off on this desk'], [401, 'Sign in again to open terminals'], [403, 'Sign in again to open terminals'], [503, 'Terminal list unavailable: the desk could not read its lane records'], [undefined, 'Terminal list unavailable']]) {
    assert.equal(terminalListReason(status), text);
    assert.deepEqual(terminalActionForLane({ state: 'error', status, targets: [{ pane_id: '%42', name: 'worker' }] }, '%42'), { target: null, reason: text });
  }
});
