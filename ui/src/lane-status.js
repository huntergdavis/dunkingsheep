// Keep absent activity distinct from idle, and show how a state was derived.
export function laneStatus(row) {
  const missing = !row.agent_status || row.agent_status === 'unknown';
  const source = row.agent_status_source;
  const reason = row.agent_status_reason;
  return {
    status: missing ? '—' : row.agent_status,
    provenance: missing
      ? `inferred: ${reason || 'no activity source'}`
      : row.agent_status_inferred === true
        ? `inferred: ${source || 'declared turns'}`
        : source ? `source: ${source}` : 'provenance unavailable',
  };
}
