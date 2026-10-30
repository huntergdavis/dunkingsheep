import test from 'node:test';
import assert from 'node:assert/strict';
import { laneStatus } from './lane-status.js';

test('absent and legacy unknown states show a dash and missing-source explanation', () => {
  for (const agent_status of [undefined, null, '', 'unknown']) {
    assert.deepEqual(laneStatus({ agent_status }), {
      status: '—', provenance: 'inferred: no activity source',
    });
  }
});
test('declared states keep their inference label and source', () => {
  for (const agent_status of ['working', 'idle']) {
    assert.deepEqual(laneStatus({ agent_status, agent_status_inferred: true,
      agent_status_source: 'codex declared turns' }), {
      status: agent_status, provenance: 'inferred: codex declared turns',
    });
  }
});
test('expired turns retain the reason instead of implying idle', () => {
  assert.deepEqual(laneStatus({ agent_status: null, agent_status_reason: 'turn expired without an end' }), {
    status: '—', provenance: 'inferred: turn expired without an end',
  });
});
test('backend status and legacy status do not acquire invented provenance', () => {
  assert.equal(laneStatus({ agent_status: 'working', agent_status_source: 'terminal backend',
    agent_status_inferred: false }).provenance, 'source: terminal backend');
  assert.equal(laneStatus({ agent_status: 'working' }).provenance, 'provenance unavailable');
});
