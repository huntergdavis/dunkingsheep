import test from 'node:test';
import assert from 'node:assert/strict';
import { provenance, splitEntries, standing, viewStateText , informationHref, requestedStore, storeSearch } from './project-info.js';

test('every non-ok state has a plain sentence, with the reason when there is one', () => {
  assert.equal(viewStateText({ state: 'ok' }), '');
  assert.equal(viewStateText({ state: 'not_mapped' }), 'No information store is mapped for this project.');
  assert.equal(viewStateText({ state: 'mapping_invalid', reason: 'the directory is not an absolute path' }),
    'The mapping for this project is invalid: the directory is not an absolute path.');
  assert.equal(viewStateText({ state: 'no_match' }), 'No entry matches the search.');
  assert.equal(viewStateText(null), 'Loading…');
  assert.equal(viewStateText({ state: 'weird' }), 'Unknown state.');
});

test('ground truths are kept apart from facts, in their order', () => {
  const split = splitEntries([
    { slug: 'g1', ground_truth: true }, { slug: 'f1', ground_truth: false }, { slug: 'g2', ground_truth: true }, { slug: 'h1' },
  ]);
  assert.deepEqual(split.groundTruths.map(e => e.slug), ['g1', 'g2']);
  assert.deepEqual(split.others.map(e => e.slug), ['f1', 'h1']);
  assert.deepEqual(splitEntries(undefined), { groundTruths: [], others: [] });
});

test('provenance and standing say what the entry is and whether to trust it', () => {
  assert.equal(provenance({ type: 'rule', authority: 'owner', source: 'owner mail', stated: '2026-11-01' }),
    'rule · authority: owner · source: owner mail · stated 2026-11-01');
  assert.equal(provenance({}), '');
  assert.deepEqual(standing({ retired: true, superseded_by: 'x', stale: true }),
    ['retired', 'superseded by x', 'stale: re-check before trusting']);
  assert.deepEqual(standing({ error: 'no front matter' }), ['unreadable: no front matter']);
  assert.deepEqual(standing({}), []);
});

test('t5241: a Databases deep link selects a project or the global store, and survives a round trip', () => {
  assert.equal(requestedStore('?project=alpha'), 'project:alpha');
  assert.equal(requestedStore('?project=global'), 'global');
  assert.equal(requestedStore('?project=a%20b'), 'project:a b');
  for (const search of ['', '?project=', '?project=%20alpha', '?other=1', undefined]) {
    assert.equal(requestedStore(search), '', String(search));
  }
  for (const selected of ['project:alpha', 'global', 'project:a b&c']) {
    assert.equal(requestedStore(storeSearch(selected)), selected);
  }
  assert.equal(storeSearch(''), '');
  assert.equal(informationHref('alpha'), '/dash/knowledge?project=alpha');
  assert.equal(informationHref('a b'), '/dash/knowledge?project=a%20b');
});
