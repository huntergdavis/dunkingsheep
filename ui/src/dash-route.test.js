// t4642: Projects is the landing view and the DS brand opens it; Front Desk stays at /dash/cos.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DASH_HOME, DASH_ROUTES, DEFAULT_DASH_VIEW, resolveDashRoute, viewForPath } from './dash-route.js';

test('the bare desk, /dash and /dash/ open Projects', () => {
  for (const entry of ['/', '/dash', '/dash/', '/index.html']) {
    assert.deepEqual(viewForPath(entry), { view: 'projects', known: true }, entry);
  }
  assert.equal(DEFAULT_DASH_VIEW, 'projects');
  assert.equal(DASH_HOME, '/dash/projects');
});

test('every dashboard keeps its own route, Front Desk included, and Projects is first in the nav', () => {
  assert.deepEqual(Object.keys(DASH_ROUTES), ['projects', 'cos', 'fleet', 'operations', 'play']);
  for (const key of Object.keys(DASH_ROUTES)) {
    assert.deepEqual(viewForPath(`/dash/${key}`), { view: key, known: true });
    assert.deepEqual(resolveDashRoute(`/dash/${key}/`), { standalone: true, view: key, known: true });
  }
  assert.equal(DASH_ROUTES.cos.title, 'Front Desk');
});

test('an unregistered dashboard is reported unknown, not silently replaced', () => {
  assert.deepEqual(viewForPath('/dash/nope'), { view: 'nope', known: false });
  assert.deepEqual(viewForPath('/dash/constructor'), { view: 'constructor', known: false });
});

test('main.jsx routes through this module and the brand link opens the landing view', () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const src = readFileSync(path.join(here, 'main.jsx'), 'utf8');
  assert.match(src, /className="dash-nav-brand" href=\{DASH_HOME\}/);
  assert.match(src, /viewForPath\(/);
  assert.doesNotMatch(src, /const DASH_ROUTES = \{/);
  assert.doesNotMatch(src, /: 'cos'\}? known=/);
});
