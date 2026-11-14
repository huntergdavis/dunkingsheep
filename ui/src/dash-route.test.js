// t4642: Projects is the landing view and the DS brand opens it; Inbox stays at /dash/cos.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { canonicalDashLocation, DASH_HOME, DASH_REDIRECTS, DASH_ROUTES, DEFAULT_DASH_VIEW, requestedPane, resolveDashRoute, viewForPath } from './dash-route.js';

test('the bare desk, /dash and /dash/ open Projects', () => {
  for (const entry of ['/', '/dash', '/dash/', '/index.html']) {
    assert.deepEqual(viewForPath(entry), { view: 'projects', known: true }, entry);
  }
  assert.equal(DEFAULT_DASH_VIEW, 'projects');
  assert.equal(DASH_HOME, '/dash/projects');
});

test('every dashboard keeps its own route, Inbox included, and Projects is first in the nav', () => {
  assert.deepEqual(Object.keys(DASH_ROUTES), ['projects', 'cos', 'operations', 'play']);
  for (const key of Object.keys(DASH_ROUTES)) {
    assert.deepEqual(viewForPath(`/dash/${key}`), { view: key, known: true });
    assert.deepEqual(resolveDashRoute(`/dash/${key}/`), { standalone: true, view: key, known: true });
    assert.equal(canonicalDashLocation(`/dash/${key}`, '?pane=%257'), null);
  }
  assert.equal(DASH_ROUTES.cos.title, 'Inbox');
});

// t4646: the Terminals view is retired; its bookmarks land on Projects with the lane kept.
test('the retired Terminals path opens Projects and keeps the pane deep link', () => {
  assert.equal(Object.hasOwn(DASH_ROUTES, 'fleet'), false);
  assert.deepEqual(DASH_REDIRECTS, { fleet: 'projects' });
  assert.deepEqual(viewForPath('/dash/fleet'), { view: 'projects', known: true });
  assert.deepEqual(resolveDashRoute('/dash/fleet/'), { standalone: true, view: 'projects', known: true });
  assert.equal(canonicalDashLocation('/dash/fleet', '?pane=%257'), '/dash/projects?pane=%257');
  assert.equal(canonicalDashLocation('/dash/fleet/', ''), '/dash/projects');
  assert.equal(canonicalDashLocation('/dash/fleet'), '/dash/projects');
  assert.equal(canonicalDashLocation('/dash/fleet', '?pane=%257&other=kept', '#output'), '/dash/projects?pane=%257&other=kept#output');
  assert.equal(canonicalDashLocation('/dash/nope', '?x=1'), null);
  assert.equal(canonicalDashLocation('/', '?pane=%257'), null);
  assert.equal(requestedPane('?pane=%257'), '%7');
  assert.equal(requestedPane('?pane=%7&x=1'), '%7');
  for (const bad of ['', '?pane=7', '?pane=%25', '?pane=..%2Fx', '?other=%257', undefined]) {
    assert.equal(requestedPane(bad), null, String(bad));
  }
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
