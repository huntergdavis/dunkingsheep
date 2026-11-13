// Quick message (t4644): message any pane the desk can already message, from the nav, without
// opening the Terminals view. Targets and the send rule come from the same /api/fleet rows and
// the same guarded /api/pane-send call the Terminals composer uses; nothing here widens access.

// A lane is blocked when it needs the guarded typing path and the desk may not use it. This is
// the rule the Terminals composer applies to its Send button.
export function laneBlocked(lane) {
  return Boolean(lane) && lane.send !== 'owner_push' && !lane.can_type;
}

// The selectable targets, in a stable order: project, then name. Rows without a pane id are
// dropped; a duplicated pane id is ambiguous and is dropped entirely.
export function quickMessageTargets(fleet) {
  const rows = Array.isArray(fleet?.rows) ? fleet.rows.filter(row => typeof row?.pane_id === 'string' && /^%\d+$/.test(row.pane_id)) : [];
  const count = new Map();
  for (const row of rows) count.set(row.pane_id, (count.get(row.pane_id) || 0) + 1);
  return rows
    .filter(row => count.get(row.pane_id) === 1)
    .map(row => ({
      pane_id: row.pane_id,
      project: row.project || 'Unassigned',
      label: `${row.name || row.pane_id} · ${row.pane_id}`,
      blocked: laneBlocked(row),
    }))
    .sort((a, b) => a.project.localeCompare(b.project) || a.label.localeCompare(b.label));
}

// Why Send is unavailable, or null when the message can be sent.
export function quickMessageBlock(targets, pane, draft, busy) {
  if (busy) return 'Sending…';
  const target = targets.find(item => item.pane_id === pane);
  if (!target) return 'Choose a pane';
  if (target.blocked) return 'Messaging is unavailable for this agent';
  if (!String(draft || '').trim()) return 'Write a message';
  return null;
}

// The server performed the transport; it gets to describe it. The outcome is prefixed so
// 'stored' and 'unconfirmed' cannot be read as 'delivered'.
export function sendOutcomeText(data) {
  return `${data.pane} [${data.outcome || 'unknown'}]: ${data.note || 'accepted'}`
    + (data.detail && !data.unconfirmed ? ` (${data.detail})` : '');
}
