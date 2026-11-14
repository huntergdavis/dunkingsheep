// S5a (t4646): what the Terminals view showed and Projects did not, so the Terminals view can be
// retired without losing it. Both come from /api/fleet, which Projects now reads beside its
// roster: the per-lane mail counts, and the agent processes that have no desk route.

// The mail waiting on one lane, as "N unread · M awaiting receipt". A number that could not be
// read is shown as unknown, never as zero: an unreadable count is not "nothing waiting".
export function laneMailText(fleet, paneId) {
  if (!fleet) return { known: false, text: 'Loading…', reason: 'the pane roster is loading' };
  if (fleet.state === 'unavailable' || !Array.isArray(fleet.rows)) {
    return { known: false, text: 'Unknown', reason: fleet.error || 'the pane roster is unavailable' };
  }
  const rows = fleet.rows.filter(row => row && row.pane_id === paneId);
  if (rows.length !== 1) {
    return { known: false, text: 'Unknown', reason: rows.length ? 'more than one roster row for this pane' : 'this lane is not in the pane roster' };
  }
  const count = value => Number.isInteger(value) && value >= 0;
  const row = rows[0];
  const unread = count(row.unread) ? String(row.unread) : 'unknown';
  const pending = row.pending_state !== 'unknown' && count(row.pending) ? String(row.pending) : 'unknown';
  const known = unread !== 'unknown' && pending !== 'unknown';
  return {
    known,
    text: `${unread} unread · ${pending} awaiting receipt`,
    reason: known ? '' : 'a count could not be read from the daemon',
  };
}

// The 'agent processes without a desk route' diagnostic: a title with the count when it is known,
// the rows, and why the inventory is missing when it is.
export function unroutedSection(fleet) {
  const unrouted = fleet && fleet.state !== 'unavailable' ? fleet.unrouted : null;
  if (!unrouted || unrouted.state !== 'ok' || !Array.isArray(unrouted.rows)) {
    const reason = !fleet ? 'Loading the process inventory…'
      : fleet.state === 'unavailable' ? (fleet.error || 'The pane roster is unavailable')
      : (unrouted && unrouted.reason) || 'Process inventory unavailable';
    return { known: false, title: 'Agent processes without a desk route (unknown)', note: '', rows: [], reason };
  }
  return {
    known: true,
    title: `Agent processes without a desk route (${unrouted.rows.length})`,
    note: unrouted.note || '',
    rows: unrouted.rows,
    reason: '',
  };
}

// How long ago, in the words the Terminals view used.
export function agoText(seconds) {
  if (seconds === null || seconds === undefined) return 'no activity recorded';
  if (seconds < 90) return `${seconds}s ago`;
  if (seconds < 5400) return `${Math.round(seconds / 60)}m ago`;
  return `${(seconds / 3600).toFixed(1)}h ago`;
}
