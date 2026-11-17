export function taskQuery(filters, cursor = null) {
  const query = new URLSearchParams({ view: filters.view });
  for (const key of ['status', 'priority', 'assignee', 'project', 'kind', 'needs_owner']) if (filters[key]) query.set(key, filters[key]);
  if (cursor) query.set('cursor', cursor);
  return query.toString();
}
export function metricNumber(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value.toLocaleString() : 'Unknown';
}
export function taskAge(created, checked) {
  if (!Number.isFinite(created) || !Number.isFinite(checked) || created < 0 || checked < created) return 'Unknown';
  const seconds = checked - created;
  for (const [unit, size] of [['d', 86400], ['h', 3600], ['m', 60]]) if (seconds >= size) return `${Math.floor(seconds / size)}${unit}`;
  return `${Math.floor(seconds)}s`;
}
export function taskPage(raw, filters) {
  if (!raw || raw.scope !== 'global' || raw.view !== filters.view || !Array.isArray(raw.tasks) || raw.tasks.length > 50
      || typeof raw.legacy_limited !== 'boolean' || !Number.isFinite(raw.checked_at)
      || !(raw.total === null || Number.isSafeInteger(raw.total) && raw.total >= raw.tasks.length)
      || !(raw.next_cursor === null || typeof raw.next_cursor === 'string' && raw.next_cursor.length > 0 && raw.next_cursor.length <= 8192)
      || raw.legacy_limited && (raw.total !== null || raw.next_cursor !== null)) throw new Error('Invalid task page; refresh to start again');
  const ids = new Set();
  const statuses = filters.view === 'closed' ? ['done', 'cancelled'] : ['open', 'in_progress', 'blocked'];
  for (const task of raw.tasks) {
    if (!task || !/^t\d+$/.test(task.id) || ids.has(task.id) || typeof task.title !== 'string'
        || !statuses.includes(task.status) || filters.status && task.status !== filters.status
        || !['urgent', 'high', 'medium', 'low'].includes(task.priority) || filters.priority && task.priority !== filters.priority
        || !Array.isArray(task.assignees) || !task.assignees.every(value => typeof value === 'string')
        || filters.assignee && !task.assignees.includes(filters.assignee)
        || !Number.isFinite(task.created_at) || !(task.block_reason === null || typeof task.block_reason === 'string')) throw new Error('Task page does not match these filters');
    ids.add(task.id);
  }
  if (raw.next_cursor && !raw.tasks.length) throw new Error('Empty task page has a continuation');
  return raw;
}
