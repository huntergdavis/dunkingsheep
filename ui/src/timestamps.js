// Instants keep their original timezone meaning; only their presentation changes.
let cachedZone, cachedFormatter;
export function timestampIso(value, seconds = false) {
  if (value == null || value === '' || !['string', 'number'].includes(typeof value) && !(value instanceof Date)) return null;
  if (typeof value === 'string' && !/(?:Z|[+-]\d\d:\d\d)$/i.test(value)) return null;
  const date = new Date(typeof value === 'number' && seconds ? value * 1000 : value);
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

export function formatTimestamp(value, timeZone, seconds = false) {
  // A context entry's date-only statement has no time or offset to convert.
  if (typeof value === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(value)) return value;
  const iso = timestampIso(value, seconds);
  if (!iso) return 'Time unavailable';
  const zone = typeof timeZone === 'string' && timeZone ? timeZone : 'UTC';
  try {
    if (cachedZone !== zone) {
      cachedFormatter = new Intl.DateTimeFormat('en-US', {
        timeZone: zone, weekday: 'short', year: 'numeric', month: 'short', day: 'numeric',
        hour: '2-digit', minute: '2-digit', hour12: true, timeZoneName: 'short',
      });
      cachedZone = zone;
    }
    return cachedFormatter.format(new Date(iso));
  } catch {
    return zone === 'UTC' ? 'Time unavailable' : formatTimestamp(value, 'UTC', seconds);
  }
}
