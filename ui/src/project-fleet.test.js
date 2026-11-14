// S5a (t4646): Projects shows the per-lane mail counts and the unrouted-process list that only
// the Terminals view had.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { agoText, laneMailText, unroutedSection } from './project-fleet.js';

const fleet = { state: 'ok', rows: [
  { pane_id: '%303', unread: 2, pending: 1, pending_state: 'ok' },
  { pane_id: '%305', unread: 0, pending: 0, pending_state: 'ok' },
  { pane_id: '%308', unread: 3, pending: null, pending_state: 'unknown' },
  { pane_id: '%9', unread: 1, pending: 0, pending_state: 'ok' },
  { pane_id: '%9', unread: 1, pending: 0, pending_state: 'ok' },
  null,
], unrouted: { state: 'ok', note: 'shells are not lanes', rows: [{ pid: 7, project: 'poolrad', agent: 'codex', reason: 'no pane' }] } };

test('a lane row shows its unread and awaiting-receipt counts', () => {
  assert.deepEqual(laneMailText(fleet, '%303'), { known: true, text: '2 unread · 1 awaiting receipt', reason: '' });
  assert.deepEqual(laneMailText(fleet, '%305'), { known: true, text: '0 unread · 0 awaiting receipt', reason: '' });
});

test('a count that could not be read is unknown, never zero', () => {
  assert.deepEqual(laneMailText(fleet, '%308'),
    { known: false, text: '3 unread · unknown awaiting receipt', reason: 'a count could not be read from the daemon' });
  // The server could not get an unread count for the lane (roster call failed, or the lane is not
  // in the roster): it sends null with unread_state 'unknown', which is shown as unknown.
  assert.equal(laneMailText({ state: 'ok', rows: [{ pane_id: '%4', unread: null, unread_state: 'unknown', pending: 2, pending_state: 'ok' }] }, '%4').text,
    'unknown unread · 2 awaiting receipt');
  assert.equal(laneMailText({ state: 'ok', rows: [{ pane_id: '%4', unread: 0, unread_state: 'unknown', pending: 2, pending_state: 'ok' }] }, '%4').text,
    'unknown unread · 2 awaiting receipt', 'a zero the server marks unknown is not shown as zero');
  assert.equal(laneMailText(fleet, '%1').text, 'Unknown');
  assert.equal(laneMailText(fleet, '%1').reason, 'this lane is not in the pane roster');
  assert.equal(laneMailText(fleet, '%9').reason, 'more than one roster row for this pane');
  assert.deepEqual(laneMailText(null, '%303'), { known: false, text: 'Loading…', reason: 'the pane roster is loading' });
  assert.deepEqual(laneMailText({ state: 'unavailable', error: 'pane roster could not be read: timeout', rows: [] }, '%303'),
    { known: false, text: 'Unknown', reason: 'pane roster could not be read: timeout' });
  assert.equal(laneMailText({ state: 'ok', rows: [{ pane_id: '%2', unread: -1, pending: 1.5, pending_state: 'ok' }] }, '%2').text,
    'unknown unread · unknown awaiting receipt');
});

test('the unrouted-process section carries its count, rows and the reason it is missing', () => {
  assert.deepEqual(unroutedSection(fleet), { known: true, title: 'Agent processes without a desk route (1)',
    note: 'shells are not lanes', rows: fleet.unrouted.rows, reason: '' });
  assert.equal(unroutedSection({ state: 'ok', rows: [], unrouted: { state: 'ok', rows: [] } }).title,
    'Agent processes without a desk route (0)');
  assert.deepEqual(unroutedSection({ state: 'ok', rows: [], unrouted: { state: 'unavailable', reason: 'proc is not readable' } }),
    { known: false, title: 'Agent processes without a desk route (unknown)', note: '', rows: [], reason: 'proc is not readable' });
  assert.equal(unroutedSection(null).reason, 'Loading the process inventory…');
  assert.equal(unroutedSection({ state: 'unavailable', error: 'daemon down', rows: [] }).reason, 'daemon down');
  assert.equal(unroutedSection({ state: 'ok', rows: [] }).reason, 'Process inventory unavailable');
});

test('Projects renders the mail column and the collapsed unrouted section', () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const source = readFileSync(path.join(here, 'main.jsx'), 'utf8');
  const start = source.indexOf('function Projects(');
  const projects = source.slice(start, source.indexOf('\nfunction ', start + 1));
  assert.match(projects, /api\('\/fleet'\)/, 'Projects reads the pane roster for the counts');
  assert.match(projects, /<th>Mail<\/th>/);
  assert.match(projects, /laneMailText\(fleet, lane\.pane_id\)/);
  assert.match(projects, /<details className="project-unrouted">/, 'collapsed by default: no open attribute');
  assert.doesNotMatch(projects, /<details className="project-unrouted" open/);
  assert.match(projects, /unroutedSection\(fleet\)/);
});

test('ages read as they did in the Terminals view', () => {
  assert.equal(agoText(null), 'no activity recorded');
  assert.equal(agoText(45), '45s ago');
  assert.equal(agoText(600), '10m ago');
  assert.equal(agoText(7200), '2.0h ago');
});
