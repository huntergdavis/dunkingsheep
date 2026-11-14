import test from 'node:test';
import assert from 'node:assert/strict';
import { channelDisplayName, laneDisplayName } from './lane-label.js';

test('the VP has a display name while routing identities remain unchanged', () => {
  const lane = { pane_id: '%1', name: 'owner' };
  assert.equal(laneDisplayName(lane), 'VP of Agentry');
  assert.equal(channelDisplayName({ id: 'cos', name: 'Owner' }), 'VP of Agentry');
  assert.deepEqual(lane, { pane_id: '%1', name: 'owner' });
  assert.equal(laneDisplayName({ pane_id: '%42', name: 'writer' }), 'writer');
  assert.equal(laneDisplayName(null), 'Agent');
});
