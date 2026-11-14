import test from 'node:test';
import assert from 'node:assert/strict';
import { INBOX_VIEWS, validateInboxPage } from './inbox-views.js';
const row = (kind, audience = null) => ({ id: 1, message_id: 'i1', body: 'human agent-only VP words do not classify', conversation_source: kind, audience,
  thread_version: 1, parent_id: null, root_id: 'i1', conversation_kind: kind, sender_kind: 'agent' });
const page = (name, rows) => ({ source_filter: name, include_internal: false, next_before: null, items: rows });
test('three views only, with recorded lineage and no audience opt-out', () => {
  assert.deepEqual(Object.values(INBOX_VIEWS).map(v => v.label), ['Your inbox', 'VP', 'Historical / prior versions']);
  for (const audience of [null, 'agent']) {
    assert.equal(validateInboxPage(page('vp_threads', [row('vp', audience)]), 'vp_threads').items.length, 1);
    assert.equal(validateInboxPage(page('human_threads', [row('human', audience)]), 'human_threads').items.length, 1);
  }
  const legacy = { ...row('historical'), historical_reason: 'prior_version', historical_labels: ['Agent-only', 'Scheduled check-in'], thread_version: null, parent_id: null, root_id: null, conversation_kind: null, sender_kind: null };
  assert.equal(validateInboxPage(page('historical', [legacy]), 'historical').items.length, 1);
  assert.throws(() => validateInboxPage(page('human_threads', [legacy]), 'human_threads'));
  assert.throws(() => validateInboxPage(page('human_threads', [row('vp')]), 'human_threads'));
  assert.throws(() => validateInboxPage(page('historical', [row('human')]), 'historical'));
  assert.throws(() => validateInboxPage(page('human_threads', [{ ...row('human'), root_id: null }]), 'human_threads'));
  assert.throws(() => validateInboxPage(page('human_threads', [{ ...row('human'), sender_kind: null }]), 'human_threads'));
  assert.throws(() => validateInboxPage(page('vp_threads', [row('vp')]), 'human_threads'));
});

test('Historical requires honest fallback reasons and reviewed labels', () => {
  const legacy = { ...row('historical'), thread_version:null,parent_id:null,root_id:null,conversation_kind:null,sender_kind:null,historical_reason:'other',historical_labels:['Agent-only'] };
  for (const reason of ['prior_version','other','unknown']) assert.equal(validateInboxPage(page('historical', [{ ...legacy, historical_reason:reason }]), 'historical').items.length,1);
  for (const bad of [{historical_reason:null},{historical_labels:null},{historical_labels:['guessed human']}]) assert.throws(()=>validateInboxPage(page('historical',[{...legacy,...bad}]),'historical'));
});
