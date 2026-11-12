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
