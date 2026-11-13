// Quick message (t4644): leave an inbox message for any live lane, from the nav, without opening
// the Terminals view. Targets come from the /api/fleet rows; the message is stored by
// POST /api/pane-inbox, which uses the daemon's inbox send. Nothing is typed into a pane, so a
// lane the desk may not type into is still a valid target; the server refuses a lane that is gone.

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
      name: row.name || row.pane_id,
      label: `${row.name || row.pane_id} · ${row.pane_id}`,
    }))
    .sort((a, b) => a.project.localeCompare(b.project) || a.label.localeCompare(b.label));
}

// Why Send is unavailable, or null when the message can be sent.
export function quickMessageBlock(targets, pane, draft, busy) {
  if (busy) return 'Sending…';
  if (!targets.some(item => item.pane_id === pane)) return 'Choose a pane';
  if (!String(draft || '').trim()) return 'Write a message';
  return null;
}

// What the dialog says after /api/pane-inbox answered: where the daemon stored the message, its
// id, and whether the lane was told. The mailbox is the daemon's; the lane name is only a fallback.
export function inboxOutcomeText(data, target) {
  const where = (typeof data?.mailbox === 'string' && data.mailbox) || target?.name || data?.pane || 'the lane';
  const told = data?.notified === true ? 'lane notified' : 'lane not notified yet';
  const moved = data?.redirected ? ' · redirected by the daemon' : '';
  return `Stored in ${where} inbox · ${data?.message_id || 'no id'} · ${told}${moved}`;
}

// The server performed the transport; it gets to describe it. The outcome is prefixed so
// 'stored' and 'unconfirmed' cannot be read as 'delivered'.
export function sendOutcomeText(data) {
  return `${data.pane} [${data.outcome || 'unknown'}]: ${data.note || 'accepted'}`
    + (data.detail && !data.unconfirmed ? ` (${data.detail})` : '');
}
