// Operations line for the leader-held metric. The qualifiers are part of the
// honesty contract: 'none' is reserved for a MEASURED zero, an unknown or
// missing count (truncated page, legacy payload) stays unknown without
// asserting the empty state, and a partial timestamp coverage renders the
// oldest KNOWN age as a labeled lower bound.
export function leaderLine(tasks, ageText) {
  if (tasks?.state !== 'ok') return 'unavailable leader-held';
  const count = tasks.leader_held;
  if (count == null) return 'unknown leader-held';
  if (count === 0) return '0 leader-held · none';
  const unknown = tasks.leader_held_ages_unknown ?? 0;
  if (unknown > 0) {
    const oldest = tasks.leader_held_oldest_age_s;
    return `${count} leader-held (oldest known${oldest != null ? ` ≥ ${ageText(oldest)}` : ' unknown'}; ${unknown} without timestamps)`;
  }
  return `${count} leader-held${tasks.leader_held_oldest_age_s != null ? ` · oldest ${ageText(tasks.leader_held_oldest_age_s)}` : ' · oldest unknown'}`;
}
